//! Streaming replies end to end: a fake claude writes its answer in small pieces;
//! the worker batches them, a real orchestrator relays them to a watching client,
//! and only the final text reaches the transcript and the saved history.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude with `--include-partial-messages`: 24 pieces ~30 ms apart,
/// then the block whole, as claude writes them.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do
  echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"word'$i' "}},"parent_tool_use_id":null}'
  sleep 0.03
done
text=""
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do text="${text}word$i "; done
echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"'"$text"'"}]}}'
echo '{"type":"result","subtype":"success","result":"'"$text"'"}'
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with the fake claude on PATH and a worker config pointing at `url`.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-stream-e2e-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root", "ledger"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let claude = dir.join("bin/claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let toml = format!(
        "url = \"{url}\"\nroots = [\"{}\"]\n",
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

async fn next(ws: &mut Ws) -> ServerMsg {
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        if let Ok(msg) = serde_json::from_str(msg.to_text().unwrap()) {
            return msg;
        }
    }
}

#[tokio::test]
async fn deltas_reach_a_watching_client_batched_and_are_never_kept() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let dir = sandbox(&format!("ws://{addr}"));
    let app = mis_orchestrator::App::new(None).with_ledger_dir(dir.join("ledger"));
    tokio::spawn(mis_orchestrator::serve(listener, app));
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/client"))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let submit = ClientMsg::Submit {
        prompt: "count".into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit).await;
    let watch = async {
        let (mut deltas, mut text) = (vec![], None);
        loop {
            match next(&mut client).await {
                ServerMsg::TextDelta { text, seq, .. } => deltas.push((seq, text)),
                ServerMsg::Event { task, event, .. } => match event {
                    AgentEvent::Text { text: t } => text = Some(t),
                    AgentEvent::Finished { .. } => return (task, deltas, text.unwrap()),
                    _ => {}
                },
                _ => {}
            }
        }
    };
    let (task, deltas, text) = tokio::time::timeout(Duration::from_secs(30), watch)
        .await
        .expect("timeout");
    let all = (1..=24).map(|i| format!("word{i} ")).collect::<String>();
    assert_eq!(text, all);
    // ~720 ms of pieces in ~100 ms batches, numbered from 1; the last batch may be superseded by the Text.
    assert!((2..24).contains(&deltas.len()), "{deltas:?}");
    let seqs: Vec<u64> = deltas.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, (1..=deltas.len() as u64).collect::<Vec<_>>());
    let streamed: String = deltas.into_iter().map(|(_, t)| t).collect();
    assert!(all.starts_with(&streamed), "{streamed:?}");

    send(&mut client, &ClientMsg::Show { task }).await;
    let detail = loop {
        if let msg @ ServerMsg::TaskDetail { .. } = next(&mut client).await {
            break msg;
        }
    };
    let ServerMsg::TaskDetail {
        events, partial, ..
    } = detail
    else {
        unreachable!()
    };
    assert_eq!(partial, None);
    assert!(events.contains(&AgentEvent::Text { text: all }));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta { .. })));
    // Saved every second: nothing saved holds a delta.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ledger = dir.join("ledger");
    let transcript = ledger.join(format!("transcripts/{task}.json"));
    let saved =
        [ledger.join("ledger.json"), transcript].map(|p| std::fs::read_to_string(p).unwrap());
    assert!(saved[1].contains("word24"), "transcript saved");
    let saved = saved.concat();
    assert!(!saved.contains("text_delta"));
    let _ = std::fs::remove_dir_all(&dir);
}
