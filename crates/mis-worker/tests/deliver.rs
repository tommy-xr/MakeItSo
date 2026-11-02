//! A message into a claude session open in a terminal, end to end through a real
//! orchestrator and worker: the worker's helper (a fake claude here) sends it, the
//! session's transcript shows it arrive (delivered, the worker free), and the session's
//! next reply, read by the live feed, answers it (done, with the reply as summary).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude as MIS's delivery helper: notes its argv, then does what
/// claude's cross-session messaging would: the message lands in the open session's
/// transcript, and SendMessage says so.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/helper-args"
case "$*" in *--tools=SendMessage*) ;; *) exit 1;; esac
echo '{"type":"user","isMeta":true,"turnOrigin":"peer","origin":{"kind":"peer","from":"uds:/tmp/mis-e2e.sock","msg_id":"m-e2e","name":"mis-phone","body":"add a test"},"message":{"content":"<cross-session-message>add a test</cross-session-message>"}}' >> "$TRANSCRIPT"
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"SendMessage","input":{"to":"demo-fd","message":"add a test"}}]}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"{\"success\":true,\"msg_id\":\"m-e2e\"}"}]}]}}'
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with the fake claude on PATH, a worker.toml, and claude session `id` open
/// in a "terminal" (this test process holds its record): its transcript's path.
fn sandbox(url: &str, id: uuid::Uuid) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mis-deliver-{}", uuid::Uuid::new_v4()));
    for d in [
        "bin",
        "state",
        "root",
        ".claude/sessions",
        ".claude/projects/-root",
    ] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let path = dir.join("bin/claude");
    std::fs::write(&path, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = dir.join("root");
    let toml = format!("url = \"{url}\"\nroots = [\"{}\"]\n", root.display());
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    let prompt = serde_json::json!({"type": "user", "cwd": root, "entrypoint": "cli",
        "turnOrigin": "human", "message": {"content": "fix the build"}});
    let reply = serde_json::json!({"type": "assistant", "message": {"model": "claude-x",
        "content": [{"type": "text", "text": "Fixed it."}], "stop_reason": "end_turn"}});
    let transcript = dir.join(format!(".claude/projects/-root/{id}.jsonl"));
    std::fs::write(&transcript, format!("{prompt}\n{reply}\n")).unwrap();
    // A start time not in `ps` form: a live pid is enough.
    let pid = std::process::id();
    let record = serde_json::json!({"pid": pid, "sessionId": id, "procStart": "x",
        "name": "demo-fd", "status": "idle", "statusUpdatedAt": 1, "peerToken": "SECRET"});
    std::fs::write(
        dir.join(format!(".claude/sessions/{pid}.json")),
        record.to_string(),
    )
    .unwrap();
    (dir, transcript)
}

fn worker(dir: &Path, transcript: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mis-worker"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", dir.join("bin").display()),
        )
        .env("HOME", dir)
        .env("MIS_STATE_DIR", dir.join("state"))
        .env("TRANSCRIPT", transcript)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Ask for Status until `want` holds.
async fn status_until(
    ws: &mut Ws,
    want: impl Fn(&[WorkerSummary], &[TaskSummary]) -> bool,
) -> (Vec<WorkerSummary>, Vec<TaskSummary>) {
    let wait = async {
        loop {
            let msg = serde_json::to_string(&ClientMsg::Status).unwrap();
            ws.send(Message::Text(msg.into())).await.unwrap();
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                if let Ok(ServerMsg::Status { workers, tasks, .. }) =
                    serde_json::from_str(msg.to_text().unwrap())
                {
                    if want(&workers, &tasks) {
                        return (workers, tasks);
                    }
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    // The live feed is read every 10 s.
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("timeout")
}

async fn until_status(ws: &mut Ws, task: TaskId, status: TaskStatus) -> TaskSummary {
    let (_, tasks) = status_until(ws, |_, ts| {
        ts.iter().any(|t| t.id == task && t.status == status)
    })
    .await;
    tasks.into_iter().find(|t| t.id == task).unwrap()
}

async fn post(base: &str, path: &str, body: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (head[9..12].parse().unwrap(), body.to_string())
}

#[tokio::test]
async fn a_message_goes_into_the_open_session_and_its_reply_answers_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(mis_orchestrator::serve(
        listener,
        mis_orchestrator::App::new(None),
    ));
    let session = uuid::Uuid::new_v4();
    let (dir, transcript) = sandbox(&base, session);
    let _worker = worker(&dir, &transcript);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("{base}/ws/client"))
        .await
        .unwrap();
    let (workers, _) = status_until(&mut client, |ws, _| ws.len() == 1).await;
    assert!(workers[0].info.deliver);

    let path = format!(
        "/api/workers/{}/sessions/claude/{session}/messages",
        workers[0].id
    );
    let (code, body) = post(&base, &path, r#"{"text":"add a test"}"#).await;
    assert_eq!(code, 200, "{body}");
    let task: TaskId = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["task"].clone(),
    )
    .unwrap();

    // Delivered once the session's transcript shows it; the worker is done with it.
    let t = until_status(&mut client, task, TaskStatus::Delivered).await;
    assert_eq!(t.activity, "delivered");
    assert_eq!(t.session.as_deref(), Some(session.to_string().as_str()));
    let args = std::fs::read_to_string(dir.join("helper-args")).unwrap();
    for flag in [
        "--tools=SendMessage",
        "--permission-mode=dontAsk",
        "--name=mis-phone",
    ] {
        assert!(args.lines().any(|l| l == flag), "{flag} in {args}");
    }
    assert!(args.contains("`to` set to \"demo-fd\""), "{args}");
    assert!(!args.contains("SECRET"));

    // The session answers; the feed reads it and the message is answered.
    let answer = serde_json::json!({"type": "assistant", "message": {"model": "claude-x",
        "content": [{"type": "text", "text": "Added a test.\nIt passes."}], "stop_reason": "end_turn"}});
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    std::io::Write::write_all(&mut f, format!("{answer}\n").as_bytes()).unwrap();
    let t = until_status(&mut client, task, TaskStatus::Done).await;
    assert_eq!(t.summary.as_deref(), Some("Added a test."));
    std::fs::remove_dir_all(dir).unwrap();
}
