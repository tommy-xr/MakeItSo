//! Ask mode end to end: a fake claude calls the worker's `approve` tool, the
//! request reaches a page client through a real orchestrator, and the
//! client's answer comes back to the agent.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude: asks `approve` about one Bash call, then reports the
/// answer and the prompt tool it was given as its result.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
cfg=""; tool=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) cfg=$2; shift ;;
    --permission-prompt-tool) tool=$2; shift ;;
    --) break ;;
  esac
  shift
done
url=$(printf '%s' "$cfg" | sed 's/.*"url":"\([^"]*\)".*/\1/')
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
r=$(curl -s -H "Authorization: Bearer $MIS_MCP_TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"approve","arguments":{"tool_name":"Bash","input":{"command":"python3 make_crate.py"},"tool_use_id":"t1"}}}' "$url")
b=$(printf '%s' "$r" | sed -n 's/.*\\"behavior\\":\\"\([a-z]*\)\\".*/\1/p')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$tool said $b\"}"
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with the fake claude on PATH and a worker.toml in ask mode.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-approvals-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let claude = dir.join("bin/claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let toml = format!(
        "url = \"{url}\"\nroots = [\"{}\"]\n\n[permissions]\nmode = \"ask\"\n",
        dir.join("root").display()
    );
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    dir
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

/// The next broadcast Status in which `task` matches `want`.
async fn until(ws: &mut Ws, task: TaskId, want: impl Fn(&TaskSummary) -> bool) -> TaskSummary {
    let wait = async {
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            if let Ok(ServerMsg::Status { tasks, .. }) =
                serde_json::from_str(msg.to_text().unwrap())
            {
                if let Some(t) = tasks.into_iter().find(|t| t.id == task && want(t)) {
                    return t;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("timeout")
}

/// Run one task through orchestrator + worker; answer its approval with `decision`.
async fn run(decision: Decision) -> (TaskSummary, Approval) {
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
    send(&mut client, &ClientMsg::Subscribe).await;
    let submit = ClientMsg::Submit {
        prompt: "make a sci-fi crate".into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit).await;
    let task = loop {
        let msg = client.next().await.unwrap().unwrap();
        if let Ok(ServerMsg::Submitted { task }) = serde_json::from_str(msg.to_text().unwrap()) {
            break task;
        }
    };
    let waiting = until(&mut client, task, |t| !t.approvals.is_empty()).await;
    assert_eq!(waiting.attention, Attention::Approval);
    let approval = waiting.approvals[0].clone();
    let answer = ClientMsg::Approve {
        task,
        id: approval.id,
        decision,
    };
    send(&mut client, &answer).await;
    // Finished arrives as an event; the next Status then shows the task done.
    let finished = async {
        loop {
            let msg = client.next().await.unwrap().unwrap();
            if let Ok(ServerMsg::Event {
                task: t,
                event: AgentEvent::Finished { .. },
                ..
            }) = serde_json::from_str(msg.to_text().unwrap())
            {
                if t == task {
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), finished)
        .await
        .expect("timeout");
    send(&mut client, &ClientMsg::Status).await;
    let done = until(&mut client, task, |t| t.status == TaskStatus::Done).await;
    let _ = std::fs::remove_dir_all(&dir);
    (done, approval)
}

#[tokio::test]
async fn allowed_from_the_page_reaches_the_agent() {
    let (done, approval) = run(Decision::Allow).await;
    assert_eq!(approval.tool, "Bash");
    assert_eq!(approval.summary, "Bash: python3 make_crate.py");
    assert_eq!(
        approval.always.as_deref(),
        Some("commands starting python3")
    );
    assert_eq!(
        done.summary.as_deref(),
        Some("mcp__mis__approve said allow")
    );
    assert!(done.approvals.is_empty());
}

#[tokio::test]
async fn denied_from_the_page_reaches_the_agent() {
    let (done, _) = run(Decision::Deny).await;
    assert_eq!(done.summary.as_deref(), Some("mcp__mis__approve said deny"));
}
