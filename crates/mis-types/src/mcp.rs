//! Minimal MCP server core (JSON-RPC 2.0, tools only), shared by the worker's
//! per-task server and the orchestrator's `/mcp`. Pure: the HTTP shell parses
//! the body, calls `handle`, and replies 200 with the JSON or 202 for None.

use serde_json::{json, Value};

/// Protocol versions we can answer as; a client asking for another gets the first.
pub const VERSIONS: [&str; 4] = ["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];

/// Reply to a body that isn't JSON (send with HTTP 400).
pub fn parse_error() -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})
}

/// Text tool result; `isError` tells the model the call failed.
fn tool_result(text: &str, is_error: bool) -> Value {
    let content = json!([{"type": "text", "text": text}]);
    match is_error {
        true => json!({"content": content, "isError": true}),
        false => json!({"content": content}),
    }
}

/// One JSON-RPC message in; the reply, or None for a notification. `server`
/// names us in `initialize`; `tools` is the `tools/list` payload; `call(name,
/// args)` runs a listed tool, and its Err becomes an `isError` result.
pub fn handle(
    req: &Value,
    server: &str,
    tools: &[Value],
    call: impl FnOnce(&str, &Value) -> Result<String, String>,
) -> Option<Value> {
    // Batches (arrays) and other non-2.0 messages are rejected outright.
    if req["jsonrpc"] != "2.0" {
        return Some(
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "invalid request"}}),
        );
    }
    // Notifications (e.g. notifications/initialized) need no reply.
    let id = req.get("id")?.clone();
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    let err = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
    let params = &req["params"];
    Some(match req["method"].as_str() {
        Some("initialize") => {
            let asked = params["protocolVersion"].as_str();
            let version = VERSIONS.into_iter().find(|v| Some(*v) == asked);
            ok(json!({
                "protocolVersion": version.unwrap_or(VERSIONS[0]),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": server, "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        Some("ping") => ok(json!({})),
        Some("tools/list") => ok(json!({ "tools": tools })),
        Some("tools/call") => match params["name"].as_str() {
            Some(name) if tools.iter().any(|t| t["name"] == name) => {
                match call(name, &params["arguments"]) {
                    Ok(text) => ok(tool_result(&text, false)),
                    Err(why) => ok(tool_result(&why, true)),
                }
            }
            _ => err(-32602, "unknown tool"),
        },
        Some(_) => err(-32601, "method not found"),
        None => err(-32600, "invalid request"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Value> {
        vec![json!({"name": "echo", "inputSchema": {"type": "object"}})]
    }

    fn echo(_: &str, args: &Value) -> Result<String, String> {
        args["text"]
            .as_str()
            .map(String::from)
            .ok_or("no text".into())
    }

    fn call(method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params});
        handle(&req, "t", &tools(), echo).unwrap()
    }

    #[test]
    fn handshake_and_listing() {
        let r = call("initialize", json!({"protocolVersion": "2025-11-25"}));
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(r["result"]["serverInfo"]["name"], "t");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = call("initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r["result"]["protocolVersion"], VERSIONS[0]);
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(handle(&note, "t", &tools(), echo), None);
        assert_eq!(call("ping", json!(null))["result"], json!({}));
        assert_eq!(
            call("tools/list", json!({}))["result"]["tools"],
            json!(tools())
        );
    }

    #[test]
    fn calls_map_ok_and_err() {
        let r = call(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "hi"}}),
        );
        assert_eq!(
            r["result"],
            json!({"content": [{"type": "text", "text": "hi"}]})
        );
        let r = call("tools/call", json!({"name": "echo", "arguments": {}}));
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "no text");
    }

    #[test]
    fn errors() {
        let code = |r: Value| r["error"]["code"].clone();
        assert_eq!(code(call("tools/call", json!({"name": "rm"}))), -32602);
        assert_eq!(code(call("server/discover", json!({}))), -32601);
        let no_method = json!({"jsonrpc": "2.0", "id": 1});
        assert_eq!(code(handle(&no_method, "t", &[], echo).unwrap()), -32600);
        for bad in [
            json!({}),
            json!([]),
            json!(42),
            json!({"id": 1, "method": "ping"}),
        ] {
            assert_eq!(code(handle(&bad, "t", &[], echo).unwrap()), -32600, "{bad}");
        }
        assert_eq!(code(parse_error()), -32700);
    }
}
