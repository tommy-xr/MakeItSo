//! Task diffs end to end: a fake claude edits a git repo that already had a
//! change of the user's, then a reply edits more; the worker reads git, a real
//! orchestrator keeps the diff, and `/api/tasks/<id>/diff` serves the thread's
//! changes added up, the user's earlier change told apart.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::diff::{Before, FileStatus};
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude: edits files, then more when resumed.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
case "$*" in
  *--resume=*)
    printf 'two\n' > 'added file.txt'
    ;;
  *)
    printf 'a\nB\nc\n' > keep.txt
    rm gone.txt
    printf 'new\n' > 'added file.txt'
    printf 'x\000\001' > pic.bin
    ;;
esac
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
echo '{"type":"result","subtype":"success","result":"done"}'
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@e"])
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// Temp HOME with the fake claude on PATH and a root that is a git repo with a
/// commit, a GitHub remote and one uncommitted change of the user's.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-diff-e2e-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let claude = dir.join("bin/claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = dir.join("root");
    git(&root, &["init", "-q", "-b", "main"]);
    for (f, text) in [
        ("keep.txt", "a\nb\nc\n"),
        ("gone.txt", "bye\n"),
        ("mine.txt", "x\n"),
    ] {
        std::fs::write(root.join(f), text).unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(
        &root,
        &["remote", "add", "origin", "https://github.com/demo/r.git"],
    );
    std::fs::write(root.join("mine.txt"), "x\nmine\n").unwrap();
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

/// GET `path`: status code and body.
async fn get(addr: &str, path: &str) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = vec![];
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let code = String::from_utf8_lossy(&raw[9..12]).parse().unwrap();
    (code, raw[split + 4..].to_vec())
}

/// Submit, then wait for the task to finish; its summary.
async fn run(client: &mut Ws, reply_to: Option<TaskId>) -> TaskSummary {
    let submit = ClientMsg::Submit {
        prompt: "edit".into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to,
        images: vec![],
    };
    send(client, &submit).await;
    let wait = async {
        let mut id = None;
        loop {
            let msg = client.next().await.unwrap().unwrap();
            match serde_json::from_str(msg.to_text().unwrap()) {
                Ok(ServerMsg::Submitted { task }) => id = Some(task),
                Ok(ServerMsg::Event {
                    task,
                    event: AgentEvent::Finished { .. },
                    ..
                }) if Some(task) == id => send(client, &ClientMsg::Status).await,
                Ok(ServerMsg::Status { tasks, .. }) => {
                    let done = tasks
                        .into_iter()
                        .find(|t| Some(t.id) == id && t.status.finished());
                    if let Some(t) = done {
                        return t;
                    }
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("timeout")
}

#[tokio::test]
async fn a_threads_changes_reach_the_api_with_earlier_ones_told_apart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let store = std::env::temp_dir().join(format!("mis-diff-store-{}", uuid::Uuid::new_v4()));
    let app = mis_orchestrator::App::new(None)
        .with_evidence(mis_orchestrator::evidence::Store::open(store.clone()).unwrap());
    tokio::spawn(mis_orchestrator::serve(listener, app));
    let dir = sandbox(&format!("ws://{addr}"));
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/client"))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;

    let first = run(&mut client, None).await;
    assert_eq!(first.status, TaskStatus::Done);
    let stat = first.diff.clone().expect("a diff");
    // keep +1 -1, gone -1, added file +1, pic.bin; mine.txt was the user's.
    assert_eq!(
        (stat.files, stat.added, stat.removed, stat.before),
        (4, 2, 2, 1)
    );
    assert_eq!(stat.repo.branch.as_deref(), Some("main"));
    assert!(stat.repo.github);

    let fetch = |id: TaskId| {
        let addr = addr.clone();
        async move {
            let (code, body) = get(&addr, &format!("/api/tasks/{id}/diff")).await;
            assert_eq!(code, 200);
            serde_json::from_slice::<TaskDiff>(&body).unwrap()
        }
    };
    let d = fetch(first.id).await;
    assert_eq!(
        d.base,
        DiffBase::default(),
        "the base stays in the orchestrator"
    );
    let rows: Vec<_> = (d.files.iter())
        .map(|f| (f.path.as_str(), f.status, f.before, f.untracked))
        .collect();
    assert_eq!(
        rows,
        [
            ("added file.txt", FileStatus::Added, Before::Clean, true),
            ("gone.txt", FileStatus::Deleted, Before::Clean, false),
            ("keep.txt", FileStatus::Modified, Before::Clean, false),
            ("pic.bin", FileStatus::Added, Before::Clean, true),
            ("mine.txt", FileStatus::Modified, Before::Only, false),
        ]
    );
    let keep = d.files[2].patch.as_deref().unwrap();
    assert!(keep.contains("-b\n+B\n"), "{keep}");

    // The reply's diff counts from where the thread started.
    let reply = run(&mut client, Some(first.id)).await;
    assert_eq!(reply.parent, Some(first.id));
    let d = fetch(reply.id).await;
    let added = d.files.iter().find(|f| f.path == "added file.txt").unwrap();
    assert_eq!(added.patch.as_deref(), Some("@@ -0,0 +1,1 @@\n+two\n"));
    assert_eq!(d.files.len(), 5);
    assert_eq!(
        get(&addr, &format!("/api/tasks/{}/diff", uuid::Uuid::new_v4()))
            .await
            .0,
        404
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&store);
}
