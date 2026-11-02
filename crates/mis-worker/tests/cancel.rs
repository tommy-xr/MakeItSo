//! Cancel end to end through a real orchestrator and worker: a queued task is
//! dropped; a running one's agent dies with the `sleep` it left in a process group
//! of its own (as claude runs Bash); a message waiting for its session to close
//! stops waiting; each time the worker takes the next task.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude: starts, leaves a `sleep` in its own process group (its pid
/// in `sleep-<task>` in the task's folder), and waits on it.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
set -m
sleep 300 &
echo $! > "$PWD/sleep-$MIS_TASK"
wait
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with fake claude and codex on PATH, and a worker.toml.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-cancel-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    for (name, script) in [("claude", FAKE_CLAUDE), ("codex", "#!/bin/sh\nexit 1\n")] {
        let path = dir.join("bin").join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
    let root = dir.join("root");
    let toml = format!("url = \"{url}\"\nroots = [\"{}\"]\n", root.display());
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    dir
}

/// A codex session in `root`, just written, so the worker counts it open.
fn open_codex_session(dir: &Path) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let day = dir.join(".codex/sessions/2026/11/01");
    std::fs::create_dir_all(&day).unwrap();
    let meta = serde_json::json!({"type": "session_meta", "payload": {
        "id": id, "cwd": dir.join("root"), "originator": "codex-tui", "source": "cli"}});
    let user = serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "user", "content": [{"type": "input_text", "text": "port the renderer"}]}});
    let path = day.join(format!("rollout-2026-11-01T09-00-00-{id}.jsonl"));
    std::fs::write(path, format!("{meta}\n{user}\n")).unwrap();
    id
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

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Ask for Status until `want` holds for some worker list and task list.
async fn status_until(
    ws: &mut Ws,
    want: impl Fn(&[WorkerSummary], &[TaskSummary]) -> bool,
) -> (Vec<WorkerSummary>, Vec<TaskSummary>) {
    let wait = async {
        loop {
            send(ws, &ClientMsg::Status).await;
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
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("timeout")
}

fn find(tasks: &[TaskSummary], id: TaskId) -> Option<&TaskSummary> {
    tasks.iter().find(|t| t.id == id)
}

async fn until_status(ws: &mut Ws, task: TaskId, status: TaskStatus) -> TaskSummary {
    let (_, tasks) = status_until(ws, |_, ts| {
        find(ts, task).is_some_and(|t| t.status == status)
    })
    .await;
    find(&tasks, task).unwrap().clone()
}

async fn submit(ws: &mut Ws, prompt: &str) -> TaskId {
    let submit = ClientMsg::Submit {
        prompt: prompt.into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(ws, &submit).await;
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        if let Ok(ServerMsg::Submitted { task }) = serde_json::from_str(msg.to_text().unwrap()) {
            return task;
        }
    }
}

/// POST `body` to `path`: status code and body.
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

/// Cancel `task`: the status code and the status it was left in (if 200).
async fn cancel(base: &str, task: TaskId) -> (u16, Option<TaskStatus>) {
    let (code, body) = post(base, &format!("/api/tasks/{task}/cancel"), "").await;
    let status = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| serde_json::from_value(v["status"].clone()).ok());
    (code, status)
}

/// The pid of the `sleep` the fake claude left for `task`, once it has.
async fn sleep_pid(dir: &Path, task: TaskId) -> String {
    let path = dir.join(format!("root/sleep-{task}"));
    for _ in 0..200 {
        let pid = std::fs::read_to_string(&path).unwrap_or_default();
        if !pid.trim().is_empty() {
            return pid.trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("task {task} never started its sleep");
}

/// Whether `pid` still runs; a zombie awaiting its reaper doesn't count.
fn alive(pid: &str) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

async fn dies(pid: &str) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn cancel_drops_queued_kills_running_and_stops_a_wait() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(mis_orchestrator::serve(
        listener,
        mis_orchestrator::App::new(None),
    ));
    let dir = sandbox(&base);
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("{base}/ws/client"))
        .await
        .unwrap();

    // One running, one queued behind it.
    let running = submit(&mut client, "long one").await;
    let sleep = sleep_pid(&dir, running).await;
    assert!(alive(&sleep));
    let queued = submit(&mut client, "behind it").await;
    until_status(&mut client, queued, TaskStatus::Queued).await;

    assert_eq!(
        cancel(&base, queued).await,
        (200, Some(TaskStatus::Cancelled))
    );
    let t = until_status(&mut client, queued, TaskStatus::Cancelled).await;
    assert_eq!(t.activity, "cancelled");

    // Running: the worker stops it; its sleep dies with it.
    assert_eq!(
        cancel(&base, running).await,
        (200, Some(TaskStatus::Running))
    );
    until_status(&mut client, running, TaskStatus::Cancelled).await;
    assert!(dies(&sleep).await, "sleep {sleep} outlived its task");
    assert_eq!(cancel(&base, running).await.0, 409, "already cancelled");
    assert_eq!(cancel(&base, uuid::Uuid::new_v4()).await.0, 404);

    // The worker is free: the next task starts.
    let next = submit(&mut client, "next").await;
    let sleep = sleep_pid(&dir, next).await;
    cancel(&base, next).await;
    until_status(&mut client, next, TaskStatus::Cancelled).await;
    assert!(dies(&sleep).await);

    // A message queued until its session closes, cancelled while it waits.
    let session = open_codex_session(&dir);
    let (workers, _) = status_until(&mut client, |ws, _| ws.len() == 1).await;
    let path = format!(
        "/api/workers/{}/sessions/codex/{session}/messages",
        workers[0].id
    );
    let (code, body) = post(&base, &path, r#"{"text":"hi","mode":"when_closed"}"#).await;
    assert_eq!(code, 200, "{body}");
    let waiting: TaskId = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["task"].clone(),
    )
    .unwrap();
    status_until(&mut client, |_, ts| {
        find(ts, waiting).is_some_and(|t| t.activity == "status: waiting for the session to close")
    })
    .await;
    assert_eq!(
        cancel(&base, waiting).await,
        (200, Some(TaskStatus::Running))
    );
    until_status(&mut client, waiting, TaskStatus::Cancelled).await;

    // Free again.
    let last = submit(&mut client, "last").await;
    let sleep = sleep_pid(&dir, last).await;
    cancel(&base, last).await;
    until_status(&mut client, last, TaskStatus::Cancelled).await;
    assert!(dies(&sleep).await);
    let _ = std::fs::remove_dir_all(&dir);
}
