//! A message into a codex session open in a terminal on Codex's app-server daemon, end
//! to end through a real orchestrator and worker: a fake daemon on the control socket
//! lists the session as loaded and takes the message as a turn (delivered, the worker
//! free), and the turn's reply in the rollout, read by the live feed, answers it (done).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with a codex on PATH (never run for a delivery), a worker.toml, and codex
/// session `id`'s rollout: (HOME, rollout).
fn sandbox(url: &str, id: uuid::Uuid) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mis-codex-deliver-{}", uuid::Uuid::new_v4()));
    let day = ".codex/sessions/2026/11/02";
    for d in ["bin", "state", "root", day, ".codex/app-server-control"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let codex = dir.join("bin/codex");
    std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&codex, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = dir.join("root");
    let toml = format!("url = \"{url}\"\nroots = [\"{}\"]\n", root.display());
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    let lines = [
        json!({"type": "session_meta", "payload": {"id": id, "cwd": root, "originator": "codex-tui"}}),
        json!({"type": "turn_context", "payload": {"model": "gpt-x"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "rename the crate"}]}}),
        json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": "Renamed."}}),
    ];
    let rollout = dir.join(format!("{day}/rollout-2026-11-02T10-00-00-{id}.jsonl"));
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&rollout, text).unwrap();
    (dir, rollout)
}

fn append(path: &Path, line: Value) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()).unwrap();
}

/// Stands in for the app-server daemon on `<home>/.codex`'s control socket (a link to a
/// short path: socket paths are capped near 100 bytes): `session` is loaded, and a
/// `turn/start` lands in its rollout as the daemon records it. Returns the turn/starts.
fn daemon(home: &Path, session: uuid::Uuid, rollout: PathBuf) -> Arc<Mutex<Vec<Value>>> {
    let sock = PathBuf::from(format!(
        "/tmp/mis-e2e-{}.sock",
        &uuid::Uuid::new_v4().to_string()[..8]
    ));
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let link = home.join(".codex/app-server-control/app-server-control.sock");
    std::os::unix::fs::symlink(&sock, link).unwrap();
    let starts = Arc::new(Mutex::new(vec![]));
    let seen = starts.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            let (seen, rollout) = (seen.clone(), rollout.clone());
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    let Ok(v) = serde_json::from_str::<Value>(msg.to_text().unwrap_or("")) else {
                        continue;
                    };
                    let (Some(id), Some(method)) = (v.get("id"), v["method"].as_str()) else {
                        continue;
                    };
                    let p = &v["params"];
                    let result = match method {
                        "initialize" => json!({"userAgent": "fake"}),
                        "thread/loaded/list" => json!({"data": [session.to_string()]}),
                        "turn/start" => {
                            seen.lock().unwrap().push(p.clone());
                            append(
                                &rollout,
                                json!({"type": "event_msg", "payload": {"type": "task_started"}}),
                            );
                            append(
                                &rollout,
                                json!({"type": "event_msg", "payload": {"type": "item_completed", "turn_id": "turn-1",
                                "item": {"type": "UserMessage", "client_id": p["clientUserMessageId"],
                                    "content": [{"type": "text", "text": p["input"][0]["text"]}]}}}),
                            );
                            json!({"turn": {"id": "turn-1", "status": "inProgress"}})
                        }
                        _ => json!({}),
                    };
                    let reply = json!({"id": id, "result": result}).to_string();
                    let note = json!({"method": "thread/status/changed", "params": {}}).to_string();
                    // A notification first: the client must wait for its own reply.
                    for m in [note, reply] {
                        if ws.send(Message::text(m)).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    starts
}

fn worker(dir: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mis-worker"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", dir.join("bin").display()),
        )
        .env("HOME", dir)
        .env("MIS_STATE_DIR", dir.join("state"))
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
async fn a_message_goes_into_the_codex_session_on_the_daemon_and_its_reply_answers_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(mis_orchestrator::serve(
        listener,
        mis_orchestrator::App::new(None),
    ));
    let session = uuid::Uuid::new_v4();
    let (dir, rollout) = sandbox(&base, session);
    let starts = daemon(&dir, session, rollout.clone());
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("{base}/ws/client"))
        .await
        .unwrap();
    let (workers, _) = status_until(&mut client, |ws, _| ws.len() == 1).await;

    let path = format!(
        "/api/workers/{}/sessions/codex/{session}/messages",
        workers[0].id
    );
    let (code, body) = post(&base, &path, r#"{"text":"add a test"}"#).await;
    assert_eq!(code, 200, "{body}");
    let task: TaskId =
        serde_json::from_value(serde_json::from_str::<Value>(&body).unwrap()["task"].clone())
            .unwrap();

    // Delivered once the daemon takes the turn; the worker is done with it.
    let t = until_status(&mut client, task, TaskStatus::Delivered).await;
    assert_eq!(t.activity, "delivered");
    let msg_id = format!("{PEER_NAME}-{task}");
    let sent = starts.lock().unwrap().clone();
    assert_eq!(
        sent,
        [
            json!({"threadId": session.to_string(), "clientUserMessageId": msg_id,
            "input": [{"type": "text", "text": "add a test"}]})
        ]
    );

    // The turn ends with its reply; the feed reads it and the message is answered.
    append(
        &rollout,
        json!({"type": "event_msg", "payload": {"type": "task_complete",
        "turn_id": "turn-1", "last_agent_message": "Added a test.\nIt passes."}}),
    );
    let t = until_status(&mut client, task, TaskStatus::Done).await;
    assert_eq!(t.summary.as_deref(), Some("Added a test."));
    let link = dir.join(".codex/app-server-control/app-server-control.sock");
    let _ = std::fs::remove_file(std::fs::read_link(&link).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}
