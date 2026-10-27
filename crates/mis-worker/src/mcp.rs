//! Loopback MCP server for one task: the agent calls `report_status` and the
//! worker forwards it as `AgentEvent::Status`. Streamable HTTP, JSON replies
//! only (no SSE): POST `/mcp` with `Authorization: Bearer <token>`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use mis_types::{first_line, AgentEvent, TaskId};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Protocol versions we can answer as; a client asking for another gets the first.
const VERSIONS: [&str; 4] = ["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];
/// Longest status kept; it is meant to be one line.
const MAX_STATUS_CHARS: usize = 200;

/// Tell the agent the tool exists.
pub fn with_hint(prompt: &str) -> String {
    format!(
        "(You have a `report_status` tool: call it with a one-line status at each major \
         step so the user can follow along.)\n\n{prompt}"
    )
}

/// One JSON-RPC message in; the reply (None for a notification) and the status
/// text to post, if it was a valid `report_status` call.
pub fn handle(req: &Value) -> (Option<Value>, Option<String>) {
    let Some(id) = req.get("id").cloned() else {
        return (None, None); // notifications (e.g. notifications/initialized) need no reply
    };
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    let err = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
    let params = &req["params"];
    let reply = match req["method"].as_str() {
        Some("initialize") => {
            let asked = params["protocolVersion"].as_str();
            let version = VERSIONS.into_iter().find(|v| Some(*v) == asked);
            ok(json!({
                "protocolVersion": version.unwrap_or(VERSIONS[0]),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": mis_agent::MCP_SERVER, "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        Some("ping") => ok(json!({})),
        Some("tools/list") => ok(json!({"tools": [{
            "name": "report_status",
            "description": "Show the user a one-line status of what you are doing now.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "e.g. \"Running the tests\""}},
                "required": ["text"],
            },
        }]})),
        Some("tools/call") if params["name"] == "report_status" => {
            let text = params["arguments"]["text"].as_str().unwrap_or("");
            let text = first_line(text, MAX_STATUS_CHARS);
            if text.is_empty() {
                let content = json!([{"type": "text", "text": "text must be a non-empty string"}]);
                return (Some(ok(json!({"content": content, "isError": true}))), None);
            }
            let content = json!([{"type": "text", "text": "ok"}]);
            return (Some(ok(json!({"content": content}))), Some(text));
        }
        Some("tools/call") => err(-32602, "unknown tool"),
        Some(_) => err(-32601, "method not found"),
        None => err(-32600, "invalid request"),
    };
    (Some(reply), None)
}

/// A running server; dropping it stops listening.
pub struct Server {
    pub mcp: mis_agent::Mcp,
    serving: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

#[derive(Clone)]
struct Ctx {
    task: TaskId,
    token: String,
    tx: mpsc::Sender<(TaskId, AgentEvent)>,
}

/// Listen on a random loopback port with a fresh random token; statuses go to `tx`.
pub async fn serve(
    task: TaskId,
    tx: mpsc::Sender<(TaskId, AgentEvent)>,
) -> std::io::Result<Server> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    // v4 UUIDs are 122 random bits from the OS RNG.
    let token = uuid::Uuid::new_v4().simple().to_string();
    let ctx = Ctx {
        task,
        token: token.clone(),
        tx,
    };
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(post))
        .with_state(ctx);
    let serving = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(%task, "mcp server: {e}");
        }
    });
    Ok(Server {
        mcp: mis_agent::Mcp { url, token },
        serving,
    })
}

async fn post(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    if auth.and_then(|a| a.strip_prefix("Bearer ")) != Some(ctx.token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        let e = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}});
        return (StatusCode::BAD_REQUEST, Json(e)).into_response();
    };
    let (reply, status) = handle(&req);
    if let Some(text) = status {
        let _ = ctx.tx.send((ctx.task, AgentEvent::Status { text })).await;
    }
    match reply {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> (Option<Value>, Option<String>) {
        handle(&json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}))
    }

    #[test]
    fn handshake_and_listing() {
        let (r, _) = call("initialize", json!({"protocolVersion": "2025-11-25"}));
        let r = r.unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let (r, _) = call("initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r.unwrap()["result"]["protocolVersion"], VERSIONS[0]);
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(handle(&note), (None, None));
        assert_eq!(call("ping", json!(null)).0.unwrap()["result"], json!({}));
        let (r, _) = call("tools/list", json!({}));
        assert_eq!(r.unwrap()["result"]["tools"][0]["name"], "report_status");
    }

    #[test]
    fn report_status_posts_one_line() {
        let (r, s) = call(
            "tools/call",
            json!({"name": "report_status", "arguments": {"text": " Running tests\nmore"}}),
        );
        assert_eq!(r.unwrap()["result"]["content"][0]["text"], "ok");
        assert_eq!(s.as_deref(), Some("Running tests"));
        for args in [json!({"text": " "}), json!({}), json!({"text": 3})] {
            let (r, s) = call(
                "tools/call",
                json!({"name": "report_status", "arguments": args}),
            );
            assert_eq!(r.unwrap()["result"]["isError"], true);
            assert_eq!(s, None);
        }
    }

    #[test]
    fn errors() {
        let code = |(r, _): (Option<Value>, _)| r.unwrap()["error"]["code"].clone();
        assert_eq!(code(call("tools/call", json!({"name": "rm"}))), -32602);
        assert_eq!(code(call("server/discover", json!({}))), -32601);
        assert_eq!(code(handle(&json!({"id": 1}))), -32600);
    }

    /// Raw HTTP/1.1 POST to the server; returns the response text.
    async fn post_raw(url: &str, auth: Option<&str>, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = url.trim_start_matches("http://").trim_end_matches("/mcp");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = auth
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn serves_status_only_with_the_token() {
        let (tx, mut rx) = mpsc::channel(4);
        let task = TaskId::new_v4();
        let server = serve(task, tx).await.unwrap();
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"report_status","arguments":{"text":"hi"}}}"#;
        let url = server.mcp.url.clone();
        assert!(post_raw(&url, None, call).await.starts_with("HTTP/1.1 401"));
        assert!(post_raw(&url, Some("wrong"), call)
            .await
            .starts_with("HTTP/1.1 401"));
        let ok = post_raw(&url, Some(&server.mcp.token), call).await;
        assert!(
            ok.starts_with("HTTP/1.1 200") && ok.contains(r#""text":"ok""#),
            "{ok}"
        );
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let token = server.mcp.token.clone();
        assert!(post_raw(&url, Some(&token), note)
            .await
            .starts_with("HTTP/1.1 202"));
        assert_eq!(
            rx.recv().await,
            Some((task, AgentEvent::Status { text: "hi".into() }))
        );
        drop(server);
        tokio::task::yield_now().await;
        assert!(tokio::net::TcpStream::connect(
            url.trim_start_matches("http://").trim_end_matches("/mcp")
        )
        .await
        .is_err());
    }
}
