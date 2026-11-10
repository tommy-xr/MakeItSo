//! Evidence files end to end: a fake claude attaches an image, a before/after
//! compare, a table and output, and reports its result; the worker reads and
//! uploads the files, a real orchestrator stores them, and `/api/evidence`
//! serves them back with headers that keep browsers from running them.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::evidence::{EvidenceItem, FileKind, Table};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude: calls the reporting tools, saving each reply in its folder.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
cfg=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) cfg=$2; shift ;;
    --) break ;;
  esac
  shift
done
url=$(printf '%s' "$cfg" | sed 's/.*"url":"\([^"]*\)".*/\1/')
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
call() {
  curl -s -H "Authorization: Bearer $MIS_MCP_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" \
    "$url" > "reply-$3.json"
}
call report_evidence '{"kind":"image","title":"Home","path":"after.png"}' image
call report_evidence '{"kind":"compare","title":"Home, before/after","before":"before.png","after":"after.png"}' compare
call report_evidence '{"kind":"table","title":"Bench","path":"bench.csv","before":"before","after":"after"}' table
call report_evidence '{"kind":"output","title":"cargo bench","text":"\u001b[32mok\u001b[0m 3 passed\n"}' output
call report_evidence '{"kind":"image","title":"Leak","path":"leak.png"}' leak
call report_evidence '{"kind":"image","title":"Fake","path":"fake.png"}' fake
call report_result '{"headline":"Made the button blue","points":["contrast 4.8:1","bench 20% faster"]}' result
echo '{"type":"result","subtype":"success","result":"done"}'
"#;

const BEFORE: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR before";
const AFTER: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR after";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Temp HOME with the fake claude on PATH, a root holding the files it attaches,
/// and a secret outside the root that a symlink in it points at.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-evidence-e2e-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let claude = dir.join("bin/claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = dir.join("root");
    std::fs::write(root.join("before.png"), BEFORE).unwrap();
    std::fs::write(root.join("after.png"), AFTER).unwrap();
    std::fs::write(
        root.join("bench.csv"),
        "case,before,after\ncold,120 ms,96 ms\n",
    )
    .unwrap();
    std::fs::write(root.join("fake.png"), "<svg onload=alert(1)>").unwrap();
    std::fs::write(dir.join("secret.png"), BEFORE).unwrap();
    std::os::unix::fs::symlink(dir.join("secret.png"), root.join("leak.png")).unwrap();
    let toml = format!("url = \"{url}\"\nroots = [\"{}\"]\n", root.display());
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

/// GET `path`: status code, lowercased header lines, body.
async fn get(addr: &str, path: &str) -> (u16, Vec<String>, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = vec![];
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let mut lines = head.lines();
    let code = lines.next().unwrap()[9..12].parse().unwrap();
    let headers = lines.map(|l| l.to_ascii_lowercase()).collect();
    (code, headers, raw[split + 4..].to_vec())
}

#[tokio::test]
async fn attached_files_are_stored_listed_and_served_inert() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let store = std::env::temp_dir().join(format!("mis-evidence-store-{}", uuid::Uuid::new_v4()));
    let app = mis_orchestrator::App::new(None)
        .with_evidence(mis_orchestrator::evidence::Store::open(store.clone()).unwrap());
    tokio::spawn(mis_orchestrator::serve(listener, app));
    let dir = sandbox(&format!("ws://{addr}"));
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/client"))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let submit = ClientMsg::Submit {
        prompt: "make the button blue".into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
        worker: None,
    };
    send(&mut client, &submit).await;
    // Finished arrives as an event; then ask for the task's status.
    let wait = async {
        loop {
            let msg = client.next().await.unwrap().unwrap();
            let msg = serde_json::from_str(msg.to_text().unwrap());
            if let Ok(ServerMsg::Event {
                event: AgentEvent::Finished { .. },
                ..
            }) = msg
            {
                send(&mut client, &ClientMsg::Status).await;
            }
            if let Ok(ServerMsg::Status { tasks, .. }) = msg {
                if let Some(t) = tasks.into_iter().find(|t| t.status == TaskStatus::Done) {
                    return t;
                }
            }
        }
    };
    let done = tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("timeout");

    // What the agent was told: files outside the roots and fake images refused.
    let reply = |name: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(format!("root/reply-{name}.json"))).unwrap();
        serde_json::from_str(&text).unwrap()
    };
    for ok in ["image", "compare", "table", "output", "result"] {
        assert_eq!(reply(ok)["result"]["content"][0]["text"], "ok", "{ok}");
    }
    let refused = |name: &str| {
        reply(name)["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(refused("leak").contains("outside"), "{}", refused("leak"));
    assert!(refused("fake").contains("not a PNG"), "{}", refused("fake"));

    let result = done.result.clone().expect("result");
    assert_eq!(result.headline, "Made the button blue");
    assert_eq!(
        done.summary.as_deref(),
        Some("Made the button blue · contrast 4.8:1 · bench 20% faster")
    );
    let titles: Vec<_> = done.evidence.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Home", "Home, before/after", "Bench", "cargo bench"]
    );

    let fetch = |id: uuid::Uuid| {
        let addr = addr.clone();
        async move {
            let (code, headers, body) = get(&addr, &format!("/api/evidence/{id}")).await;
            assert_eq!(code, 200);
            for h in [
                "x-content-type-options: nosniff",
                "content-security-policy: default-src 'none'",
                "content-disposition: inline",
            ] {
                assert!(headers.iter().any(|l| l == h), "{h} in {headers:?}");
            }
            (headers, body)
        }
    };
    let EvidenceItem::Compare { before, after } = &done.evidence[1].item else {
        panic!("{:?}", done.evidence[1])
    };
    assert_eq!((before.mime, after.mime), (FileKind::Png, FileKind::Png));
    let (headers, body) = fetch(before.id).await;
    assert!(headers.contains(&"content-type: image/png".to_string()));
    assert_eq!(body, BEFORE);
    assert_eq!(fetch(after.id).await.1, AFTER);
    let EvidenceItem::Table { file, rows, cols } = &done.evidence[2].item else {
        panic!()
    };
    assert_eq!((rows, cols), (&1, &3));
    let (headers, body) = fetch(file.id).await;
    assert!(headers.contains(&"content-type: application/json".to_string()));
    let table: Table = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        (table.before, table.after, table.rows[0][2].num),
        (Some(1), Some(2), Some(96.0))
    );
    let EvidenceItem::Output { file, .. } = &done.evidence[3].item else {
        panic!()
    };
    let (headers, body) = fetch(file.id).await;
    assert!(headers.contains(&"content-type: text/plain; charset=utf-8".to_string()));
    assert_eq!(body, b"\x1b[32mok\x1b[0m 3 passed\n");
    assert_eq!(
        get(&addr, &format!("/api/evidence/{}", uuid::Uuid::new_v4()))
            .await
            .0,
        404
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&store);
}
