//! Codex's shared app-server daemon, which hosts each plain `codex` terminal's session:
//! the sessions it has loaded (open in a terminal, or closed in the last ~30 s), and a
//! message started as a turn in one. JSON-RPC over a WebSocket on its control socket,
//! `<codex home>/app-server-control/app-server-control.sock`; the socket's permissions
//! are its only guard. Building and reading messages is pure; [`Client`] does the I/O,
//! bounded by [`TIMEOUT`]. A daemon that isn't running is never started.
// Windows has no client (the daemon's socket is unix-only here): its builders go unused.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mis_types::PEER_NAME;
use serde_json::{json, Value};

/// Longest a connection may take to set up and list; the daemon answers in milliseconds.
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// Longest a message may take to be taken in once sent.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// What became of a message sent with `Client::send`.
#[derive(Debug, PartialEq)]
pub enum Sent {
    /// In its conversation: a new turn, or the running one.
    Taken,
    /// The daemon hasn't loaded the session (closed since it was listed).
    NotLoaded,
    /// The daemon refused it, saying why.
    Refused(String),
    /// Sent, but no answer came (why): it may or may not be in.
    Unconfirmed(String),
}

/// Where the daemon listens.
pub fn socket(codex_home: &Path) -> PathBuf {
    codex_home.join("app-server-control/app-server-control.sock")
}

/// The id a delivered message carries (`clientUserMessageId`), which the session's
/// rollout records with it: `mis-phone-<task>`.
pub fn msg_id(task: uuid::Uuid) -> String {
    format!("{PEER_NAME}-{task}")
}

/// A user message's client id names it as MIS's ([`msg_id`]).
pub fn is_mis(client_id: &str) -> bool {
    client_id
        .strip_prefix(PEER_NAME)
        .is_some_and(|r| r.starts_with('-'))
}

/// One JSON-RPC request.
pub fn request(id: u64, method: &str, params: Value) -> String {
    json!({"id": id, "method": method, "params": params}).to_string()
}

pub fn initialize() -> Value {
    json!({"clientInfo": {"name": "mis-worker", "version": env!("CARGO_PKG_VERSION")}})
}

/// A user message as a turn on `thread`. Mid-turn, the daemon adds it to the running
/// turn (as `turn/steer` would), so this one call serves an idle session and a busy one.
pub fn turn_start(thread: &str, text: &str, msg_id: &str) -> Value {
    json!({"threadId": thread, "clientUserMessageId": msg_id,
        "input": [{"type": "text", "text": text}]})
}

/// `msg` if it answers request `id`: its result, or its error's message. Notifications
/// and the daemon's own requests are None.
pub fn reply(msg: &str, id: u64) -> Option<Result<Value, String>> {
    let v: Value = serde_json::from_str(msg).ok()?;
    if v.get("method").is_some() || v["id"].as_u64() != Some(id) {
        return None;
    }
    Some(match v.get("error") {
        Some(e) => Err(e["message"].as_str().unwrap_or("app-server error").into()),
        None => Ok(v["result"].clone()),
    })
}

/// `thread/loaded/list`'s ids.
pub fn loaded_ids(result: &Value) -> HashSet<String> {
    let ids = result["data"].as_array().into_iter().flatten();
    ids.filter_map(|i| i.as_str().map(String::from)).collect()
}

/// The error `turn/start` gives for a session the daemon hasn't loaded.
pub fn not_loaded(err: &str) -> bool {
    err.starts_with("thread not found")
}

#[cfg(unix)]
pub use client::Client;

#[cfg(unix)]
mod client {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Instant;

    use serde_json::Value;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message, WebSocket};

    use super::*;

    pub struct Client {
        ws: WebSocket<UnixStream>,
        next: u64,
        deadline: Instant,
    }

    impl Client {
        /// Connected and initialized; Err when no daemon listens (or it takes too long).
        pub fn connect(codex_home: &Path) -> Result<Self, String> {
            let deadline = Instant::now() + TIMEOUT;
            // A link to the real socket, whose path fits the ~100-byte limit where this may not.
            let path = socket(codex_home);
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            let s = UnixStream::connect(path).map_err(|e| e.to_string())?;
            s.set_read_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            s.set_write_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            let req = "ws://localhost/"
                .into_client_request()
                .map_err(|e| e.to_string())?;
            let (ws, _) = tokio_tungstenite::tungstenite::client(req, s)
                .map_err(|e| format!("app-server handshake: {e}"))?;
            let mut c = Self {
                ws,
                next: 0,
                deadline,
            };
            c.call("initialize", initialize())??;
            let note = serde_json::json!({"method": "initialized"}).to_string();
            c.ws.send(Message::text(note)).map_err(|e| e.to_string())?;
            Ok(c)
        }

        /// Send one request and wait for its reply, passing over notifications. Err: no
        /// reply came; Ok(Err): the daemon's error.
        fn call(&mut self, method: &str, params: Value) -> Result<Result<Value, String>, String> {
            let id = self.next;
            self.next += 1;
            let msg = Message::text(request(id, method, params));
            self.ws.send(msg).map_err(|e| e.to_string())?;
            loop {
                let left = self.deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(format!("app-server: no reply to {method}"));
                }
                let _ = self.ws.get_ref().set_read_timeout(Some(left));
                let msg = self.ws.read().map_err(|e| format!("app-server: {e}"))?;
                if let Some(r) = msg.to_text().ok().and_then(|m| reply(m, id)) {
                    return Ok(r);
                }
            }
        }

        pub fn loaded(&mut self) -> Result<HashSet<String>, String> {
            let r = self.call("thread/loaded/list", serde_json::json!({}))??;
            Ok(loaded_ids(&r))
        }

        /// `text` into loaded session `thread` as `msg_id`.
        pub fn send(&mut self, thread: &str, text: &str, msg_id: &str) -> Sent {
            self.deadline = Instant::now() + SEND_TIMEOUT;
            match self.call("turn/start", turn_start(thread, text, msg_id)) {
                Ok(Ok(_)) => Sent::Taken,
                Ok(Err(e)) if not_loaded(&e) => Sent::NotLoaded,
                Ok(Err(e)) => Sent::Refused(e),
                Err(e) => Sent::Unconfirmed(e),
            }
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.ws.close(None);
            let _ = self.ws.flush();
        }
    }
}

/// The sessions the daemon has loaded; none when it isn't running (or on Windows).
pub fn loaded(codex_home: &Path) -> HashSet<String> {
    #[cfg(unix)]
    match Client::connect(codex_home).and_then(|mut c| c.loaded()) {
        Ok(ids) => return ids,
        Err(e) => tracing::debug!("no codex app-server: {e}"),
    }
    let _ = codex_home;
    HashSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_requests_and_reads_their_replies() {
        let req: Value = serde_json::from_str(&request(3, "turn/start", json!({}))).unwrap();
        assert_eq!(req, json!({"id": 3, "method": "turn/start", "params": {}}));
        let id = msg_id(uuid::Uuid::nil());
        assert_eq!(id, "mis-phone-00000000-0000-0000-0000-000000000000");
        assert!(is_mis(&id));
        assert!(!is_mis("mis-phoney") && !is_mis("other-1") && !is_mis("mis-phone"));
        assert_eq!(
            turn_start("t1", "fix it", &id),
            json!({"threadId": "t1", "clientUserMessageId": id,
                "input": [{"type": "text", "text": "fix it"}]})
        );
        // Notifications, server requests and other ids aren't the reply.
        let note = r#"{"method":"thread/status/changed","params":{"threadId":"t1"}}"#;
        assert_eq!(reply(note, 1), None);
        let ask = r#"{"id":1,"method":"item/commandExecution/requestApproval","params":{}}"#;
        assert_eq!(reply(ask, 1), None);
        assert_eq!(reply(r#"{"id":2,"result":{}}"#, 1), None);
        assert_eq!(reply("not json", 1), None);
        let ok = r#"{"id":1,"result":{"data":["a","b"],"nextCursor":null}}"#;
        let got = reply(ok, 1).unwrap().unwrap();
        assert_eq!(loaded_ids(&got), HashSet::from(["a".into(), "b".into()]));
        let err = r#"{"error":{"code":-32600,"message":"thread not found: t9"},"id":1}"#;
        let err = reply(err, 1).unwrap().unwrap_err();
        assert!(not_loaded(&err), "{err}");
        assert!(loaded_ids(&json!({})).is_empty());
    }
}
