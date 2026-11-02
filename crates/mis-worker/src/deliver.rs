//! A `SendMode::Deliver` message to a claude session open in a terminal: sent as a
//! cross-session message (`mis_agent::deliver`), then confirmed in the session's own
//! transcript, which records it as it arrives. Reading what was appended is [`seen`]'s,
//! pure; sending and polling is [`deliver`]'s.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mis_types::{first_line, AgentEvent, PEER_NAME};
use serde_json::Value;

use crate::live;
use crate::sessions::mis_peer;

/// How long a sent message may take to show up in the transcript; it takes a moment.
const CONFIRM: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(500);

/// What the session's transcript shows of one send.
#[derive(Debug, PartialEq)]
pub enum Seen {
    /// In its conversation, as this text.
    Read(String),
    /// In its inbox, taken in at its next tool round (it is mid-turn).
    Queued,
    /// Held for its user's approval: the sender's permission mode is of another class.
    Held,
}

/// What `appended` (transcript lines written since the send) says of message `msg_id`
/// with `text`. Claude records an inbox arrival and a hold without the message's id, so
/// those are told by sender name and text; only lines written after the send are given.
pub fn seen(appended: &str, msg_id: &str, text: &str) -> Option<Seen> {
    let (mut queued, mut held) = (false, false);
    let wrapped = format!("\n{}\n</cross-session-message>", text.trim());
    for v in appended
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if let Some(p) = mis_peer(&v).filter(|p| p.msg_id == msg_id) {
            return Some(Seen::Read(p.body.to_string()));
        }
        let content = v["content"].as_str().unwrap_or_default();
        let from = format!("from-name=\"{PEER_NAME}\"");
        queued |= v["type"] == "queue-operation"
            && v["operation"] == "enqueue"
            && content.contains(&from)
            && content.contains(&wrapped);
        held |= v["type"] == "system"
            && v["subtype"] == "informational"
            && content.starts_with("Held peer message")
            && content.contains(&format!("(peer claims name: {PEER_NAME})"));
    }
    match (held, queued) {
        (true, _) => Some(Seen::Held),
        (_, true) => Some(Seen::Queued),
        _ => None,
    }
}

/// Bytes `path` holds past `from`.
fn appended(path: &Path, from: u64) -> String {
    let mut s = Vec::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.seek(SeekFrom::Start(from)).is_ok() {
            let _ = f.read_to_end(&mut s);
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

/// Send `text` to session `id` (transcript at `path`, peer name `name`), open in a terminal
/// under `claude`; the event that ends the task: `Delivered` once its transcript shows the
/// message, else a failed `Finished` saying why.
pub async fn deliver(
    claude: PathBuf,
    path: PathBuf,
    id: String,
    name: &str,
    text: &str,
) -> AgentEvent {
    let fail = |why: String| AgentEvent::Finished {
        ok: false,
        summary: Some(why),
    };
    let start = std::fs::metadata(&path).map_or(0, |m| m.len());
    let sent = match mis_agent::deliver::send(name, text).await {
        Ok(s) => s,
        Err(why) => return fail(format!("couldn't deliver it to {name}: {why}")),
    };
    // Sent already, but not as written: say so rather than call it delivered.
    let differs = |got: &str| got.trim() != text.trim();
    if differs(&sent.text) {
        let got = first_line(&sent.text, 120);
        return fail(format!("sent to {name}, but changed on the way: “{got}”"));
    }
    let delivered = |held| AgentEvent::Delivered {
        msg_id: sent.msg_id.clone(),
        held,
    };
    let deadline = tokio::time::Instant::now() + CONFIRM;
    let mut held = false;
    while tokio::time::Instant::now() < deadline {
        let (p, c, i, msg) = (
            path.clone(),
            claude.clone(),
            id.clone(),
            sent.msg_id.clone(),
        );
        let t = text.to_string();
        let (got, waiting) = tokio::task::spawn_blocking(move || {
            let got = seen(&appended(&p, start), &msg, &t);
            // A hold shows in its terminal as a dialog: the process says it waits.
            let procs = live::claude_procs(&c, live::alive);
            let waiting = procs.get(&i).and_then(|p| p.status.as_deref()) == Some("waiting");
            (got, waiting)
        })
        .await
        .unwrap_or((None, false));
        match got {
            Some(Seen::Read(body)) if differs(&body) => {
                let got = first_line(&body, 120);
                return fail(format!("delivered to {name}, but as “{got}”"));
            }
            Some(Seen::Read(_) | Seen::Queued) => return delivered(false),
            Some(Seen::Held) if waiting => return delivered(true),
            Some(Seen::Held) => held = true,
            None => {}
        }
        tokio::time::sleep(RECHECK).await;
    }
    match held {
        true => delivered(true),
        false => fail(format!(
            "sent to {name}, but it hasn't shown up in that session"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(msg_id: &str, body: &str) -> String {
        serde_json::json!({"type": "user", "isMeta": true, "turnOrigin": "peer",
            "origin": {"kind": "peer", "from": "uds:/tmp/s.sock", "msg_id": msg_id,
                "name": PEER_NAME, "body": body},
            "message": {"role": "user", "content": format!("<cross-session-message>{body}</cross-session-message>")}})
        .to_string()
    }

    fn enqueue(name: &str, text: &str) -> String {
        let content = format!("<cross-session-message from=\"uds:/tmp/s.sock\" from-name=\"{name}\" from-mode=\"prompting\">\n{text}\n</cross-session-message>");
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": content})
            .to_string()
    }

    fn hold(name: &str) -> String {
        let content = format!("Held peer message — from uds:/tmp/s.sock [verified pid 1] (peer claims name: {name}); preview: «fix it» — not delivered to Claude (1 held).");
        serde_json::json!({"type": "system", "subtype": "informational", "content": content})
            .to_string()
    }

    #[test]
    fn tells_read_queued_and_held_apart() {
        let lines = |ls: &[String]| ls.join("\n") + "\n";
        assert_eq!(seen("", "m1", "fix it"), None);
        // Idle: straight into its conversation, verbatim.
        let read = lines(&[enqueue(PEER_NAME, "fix it"), peer("m1", "fix it")]);
        assert_eq!(
            seen(&read, "m1", "fix it"),
            Some(Seen::Read("fix it".into()))
        );
        // Another message's turn isn't this one's.
        assert_eq!(seen(&lines(&[peer("m2", "fix it")]), "m1", "fix it"), None);
        // Busy: in its inbox until its next tool round.
        let queued = lines(&[enqueue(PEER_NAME, "fix it")]);
        assert_eq!(seen(&queued, "m1", "fix it"), Some(Seen::Queued));
        // Someone else's, or other text, isn't ours.
        assert_eq!(
            seen(&lines(&[enqueue("other", "fix it")]), "m1", "fix it"),
            None
        );
        assert_eq!(seen(&queued, "m1", "fix"), None);
        // Held for approval in its terminal.
        let held = lines(&[hold(PEER_NAME)]);
        assert_eq!(seen(&held, "m1", "fix it"), Some(Seen::Held));
        assert_eq!(seen(&lines(&[hold("other")]), "m1", "fix it"), None);
        // Approved: in its conversation after all.
        let approved = lines(&[hold(PEER_NAME), peer("m1", "fix it")]);
        assert_eq!(
            seen(&approved, "m1", "fix it"),
            Some(Seen::Read("fix it".into()))
        );
        // Taken in mid-turn, beside a tool result.
        let absorbed = serde_json::json!({"type": "attachment", "attachment": {
            "type": "queued_command", "origin": {"kind": "peer", "msg_id": "m1",
                "name": PEER_NAME, "body": "fix it"}}});
        let mid = lines(&[enqueue(PEER_NAME, "fix it"), absorbed.to_string()]);
        assert_eq!(
            seen(&mid, "m1", "fix it"),
            Some(Seen::Read("fix it".into()))
        );
        // A cut-off last line is skipped.
        assert_eq!(
            seen(&format!("{queued}{{\"type\":"), "m1", "fix it"),
            Some(Seen::Queued)
        );
    }
}
