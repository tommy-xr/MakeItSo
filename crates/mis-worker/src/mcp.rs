//! Loopback MCP server for one task: the agent calls `report_status` and
//! `report_evidence`, and the worker forwards them as `AgentEvent::Status` and
//! `AgentEvent::Evidence`. Streamable HTTP, JSON replies only (no SSE): POST
//! `/mcp` with `Authorization: Bearer <token>`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use mis_types::{first_line, github_link, mcp, AgentEvent, Link, LinkKind, TaskId};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Longest status, evidence title or test detail kept; each is one line.
const MAX_LINE_CHARS: usize = 200;
/// Longest evidence URL accepted.
const MAX_URL_BYTES: usize = 2048;

/// Tell the agent the tools exist.
pub fn with_hint(prompt: &str) -> String {
    format!(
        "(You have a `report_status` tool: call it with a one-line status at each major \
         step so the user can follow along. After you push, open a PR, run tests or CI, or \
         capture screenshots, call `report_evidence` once for each (a screenshot without an \
         https URL: a note naming its path).)\n\n{prompt}"
    )
}

/// One JSON-RPC message in; the reply (None for a notification) and the event
/// to post, if it was a valid tool call.
pub fn handle(req: &Value) -> (Option<Value>, Option<AgentEvent>) {
    let tools = [
        json!({
            "name": "report_status",
            "description": "Show the user a one-line status of what you are doing now.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "e.g. \"Running the tests\""}},
                "required": ["text"],
            },
        }),
        json!({
            "name": "report_evidence",
            "description": "Show the user proof of your work: a link (pushed commit, PR, CI run, \
                deployed preview, screenshot), a test or check result, or a short note.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["link", "test", "note"]},
                    "title": {"type": "string", "description": "e.g. \"cargo test\", \"Preview\""},
                    "url": {"type": "string", "description": "https; required for a link"},
                    "ok": {"type": "boolean", "description": "a test's verdict"},
                    "detail": {"type": "string", "description": "a test's result, e.g. \"78 passed\""},
                },
                "required": ["kind", "title"],
            },
        }),
    ];
    let mut event = None;
    let reply = mcp::handle(req, mis_agent::MCP_SERVER, &tools, |name, args| {
        event = Some(match name {
            "report_evidence" => AgentEvent::Evidence(evidence(args)?),
            _ => {
                let text = first_line(args["text"].as_str().unwrap_or(""), MAX_LINE_CHARS);
                if text.is_empty() {
                    return Err("text must be a non-empty string".into());
                }
                AgentEvent::Status { text }
            }
        });
        Ok("ok".into())
    });
    (reply, event)
}

/// `report_evidence` arguments as an evidence row, text cut to one line. A
/// GitHub link becomes its PR/commit/CI/issue row, so it dedupes with the same
/// link found in the agent's output.
fn evidence(args: &Value) -> Result<Link, String> {
    let line = |k: &str| first_line(args[k].as_str().unwrap_or(""), MAX_LINE_CHARS);
    let label = line("title");
    if label.is_empty() {
        return Err("title must be a non-empty string".into());
    }
    // Checked only for kinds that keep it.
    let url = || match args["url"].as_str() {
        Some(u) if !is_https_url(u) => Err("url must be an https URL".to_string()),
        u => Ok(u.unwrap_or("").to_string()),
    };
    let row = |kind, url: String, ok, detail| Link {
        kind,
        url,
        label: label.clone(),
        ok,
        detail,
    };
    match args["kind"].as_str() {
        Some("link") => match url()? {
            u if u.is_empty() => Err("a link needs a url".into()),
            u => Ok(github_link(&u).unwrap_or_else(|| row(LinkKind::Url, u, None, None))),
        },
        Some("test") => {
            let detail = Some(line("detail")).filter(|d| !d.is_empty());
            Ok(row(LinkKind::Test, url()?, args["ok"].as_bool(), detail))
        }
        Some("note") => Ok(row(LinkKind::Note, String::new(), None, None)),
        _ => Err("kind must be link, test or note".into()),
    }
}

/// `https://<host>[:port]…` with an ASCII DNS-style host (no userinfo or IPv6
/// literal, which browsers may parse differently), no whitespace or control
/// chars, at most `MAX_URL_BYTES`.
fn is_https_url(u: &str) -> bool {
    let authority = u
        .strip_prefix("https://")
        .and_then(|r| r.split(['/', '?', '#']).next())
        .unwrap_or("");
    let (host, port) = authority.split_once(':').unwrap_or((authority, "443"));
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && port.parse::<u16>().is_ok_and(|p| p > 0)
        && u.len() <= MAX_URL_BYTES
        && !u.chars().any(|c| c.is_whitespace() || c.is_control())
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
        return (StatusCode::BAD_REQUEST, Json(mcp::parse_error())).into_response();
    };
    let (reply, event) = handle(&req);
    if let Some(event) = event {
        let _ = ctx.tx.send((ctx.task, event)).await;
    }
    match reply {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> (Option<Value>, Option<AgentEvent>) {
        handle(&json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}))
    }

    fn tool(name: &str, args: Value) -> (Option<Value>, Option<AgentEvent>) {
        call("tools/call", json!({"name": name, "arguments": args}))
    }

    #[test]
    fn report_status_posts_one_line() {
        let (r, s) = tool("report_status", json!({"text": " Running tests\nmore"}));
        assert_eq!(r.unwrap()["result"]["content"][0]["text"], "ok");
        let want = AgentEvent::Status {
            text: "Running tests".into(),
        };
        assert_eq!(s, Some(want));
        for args in [json!({"text": " "}), json!({}), json!({"text": 3})] {
            let (r, s) = tool("report_status", args);
            assert_eq!(r.unwrap()["result"]["isError"], true);
            assert_eq!(s, None);
        }
    }

    fn reported(args: Value) -> Link {
        let (r, e) = tool("report_evidence", args);
        assert_eq!(r.unwrap()["result"]["content"][0]["text"], "ok");
        let Some(AgentEvent::Evidence(link)) = e else {
            panic!("{e:?}")
        };
        link
    }

    #[test]
    fn report_evidence_posts_links_tests_and_notes() {
        let pr = reported(
            json!({"kind": "link", "title": "my PR", "url": "https://github.com/o/r/pull/42/files"}),
        );
        assert_eq!(pr, github_link("https://github.com/o/r/pull/42").unwrap());
        let url = "https://preview.example.com:8443/a?b=1#c";
        let preview = reported(json!({"kind": "link", "title": "Preview", "url": url}));
        assert_eq!((preview.kind, preview.url.as_str()), (LinkKind::Url, url));
        let test = reported(json!({
            "kind": "test", "title": "cargo test", "ok": true, "detail": format!("78 passed\n{}", "x".repeat(300)),
        }));
        assert_eq!(
            (test.kind, test.label.as_str(), test.ok),
            (LinkKind::Test, "cargo test", Some(true))
        );
        assert_eq!(test.detail.as_deref(), Some("78 passed"));
        let long = "y".repeat(300);
        let note = reported(json!({"kind": "note", "title": long, "url": "file:///x.png"}));
        assert_eq!((note.kind, note.url.as_str()), (LinkKind::Note, ""));
        assert_eq!(note.label.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn report_evidence_rejects_bad_args() {
        for args in [
            json!({"kind": "link", "title": "t"}),
            json!({"kind": "link", "title": "t", "url": "http://x.io"}),
            json!({"kind": "link", "title": "t", "url": "javascript:alert(1)"}),
            json!({"kind": "link", "title": "t", "url": "https://"}),
            json!({"kind": "link", "title": "t", "url": "https:///x"}),
            json!({"kind": "link", "title": "t", "url": "https://:443/x"}),
            json!({"kind": "link", "title": "t", "url": "https://x.io:99999/"}),
            json!({"kind": "link", "title": "t", "url": "https://x.io:/"}),
            json!({"kind": "link", "title": "t", "url": "https://[bad/"}),
            json!({"kind": "link", "title": "t", "url": "https://github.com@evil.io/"}),
            json!({"kind": "link", "title": "t", "url": "https://x.io/a b"}),
            json!({"kind": "link", "title": "t", "url": format!("https://x.io/{}", "a".repeat(MAX_URL_BYTES))}),
            json!({"kind": "test", "title": "t", "url": "ftp://x.io"}),
            json!({"kind": "link", "title": " ", "url": "https://x.io"}),
            json!({"kind": "note"}),
            json!({"kind": "image", "title": "t"}),
            json!({"title": "t"}),
        ] {
            let (r, e) = tool("report_evidence", args.clone());
            assert_eq!(r.unwrap()["result"]["isError"], true, "{args}");
            assert_eq!(e, None);
        }
    }

    #[test]
    fn lists_both_tools() {
        let (r, _) = call("tools/list", json!({}));
        let names: Vec<_> = r.unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].clone())
            .collect();
        assert_eq!(names, [json!("report_status"), json!("report_evidence")]);
        let (r, s) = call("tools/call", json!({"name": "rm"}));
        assert_eq!(
            (r.unwrap()["error"]["code"].clone(), s),
            (json!(-32602), None)
        );
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
