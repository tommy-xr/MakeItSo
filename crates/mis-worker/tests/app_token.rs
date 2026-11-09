//! An app task's token end to end through a real orchestrator and worker: the agent finds
//! it as `MIS_TOKEN` (never the worker's own) beside `MIS_URL`, can stage a release of
//! its app with it, and loses it when the task ends.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Stands in for claude: writes what it was given to `env-<task>` in its folder, then waits.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
printf '%s\n%s\n' "$MIS_URL" "$MIS_TOKEN" > "$PWD/env-$MIS_TASK"
exec sleep 300
"#;

/// Temp HOME with fake claude on PATH, the default apps repo (`~/code/mis-notes`) as the
/// worker's root, and a worker.toml holding the full token.
fn sandbox(url: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-app-token-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "code/mis-notes"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let path = dir.join("bin/claude");
    std::fs::write(&path, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = dir.join("code/mis-notes");
    let toml = format!("url = \"{url}\"\nroots = [\"{}\"]\n", root.display());
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    dir
}

/// The worker, given the full token through its env, which its agents would inherit.
fn worker(dir: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mis-worker"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", dir.join("bin").display()),
        )
        .env("HOME", dir)
        .env("MIS_STATE_DIR", dir.join("state"))
        .env("MIS_TOKEN", "s")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// `method path` with bearer `token` and `body`: status code and body.
async fn http(base: &str, method: &str, path: &str, token: &str, body: &[u8]) -> (u16, String) {
    let addr = base.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (head[9..12].parse().unwrap(), body.to_string())
}

/// A minimal app bundle.
fn bundle() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, bytes) in [
        (
            "app.json",
            &br#"{"title":"Pool","clients":{"web":"index.html"}}"#[..],
        ),
        ("index.html", b"hi"),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, path, bytes).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &b.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

/// What the fake agent of `task` was given: `MIS_URL` and `MIS_TOKEN`.
async fn agent_env(dir: &Path, task: &str) -> (String, String) {
    let path = dir.join(format!("code/mis-notes/env-{task}"));
    for _ in 0..200 {
        let got = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some((url, token)) = got.trim_end().split_once('\n') {
            return (url.into(), token.into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("task {task} never started");
}

#[tokio::test]
async fn the_agent_holds_its_task_token_not_the_workers_until_the_task_ends() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    let apps_dir =
        std::env::temp_dir().join(format!("mis-app-token-apps-{}", uuid::Uuid::new_v4()));
    let apps = mis_orchestrator::apps::Apps::new(apps_dir.clone(), None);
    let app = mis_orchestrator::App::new(Some("s".into())).with_apps(std::sync::Arc::new(apps));
    tokio::spawn(mis_orchestrator::serve(listener, app));
    let (status, _) = http(&base, "POST", "/api/apps/pool/releases", "s", &bundle()).await;
    assert_eq!(status, 201);
    let dir = sandbox(&base);
    let _worker = worker(&dir);

    let change = br#"{"request":"Add a chart.","slug":"pool","change":true,"harness":"claude"}"#;
    // The worker may still be connecting: no worker runs claude yet.
    let mut created = (0, String::new());
    for _ in 0..200 {
        created = http(&base, "POST", "/api/apps/tasks", "s", change).await;
        if created.0 == 201 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(created.0, 201, "{}", created.1);
    let task = serde_json::from_str::<serde_json::Value>(&created.1).unwrap()["task"]
        .as_str()
        .unwrap()
        .to_string();

    let (url, token) = agent_env(&dir, &task).await;
    assert_eq!(url, base);
    assert!(token.starts_with("mis-app-"), "{token}");
    let (status, body) = http(&url, "POST", "/api/apps/pool/releases", &token, &bundle()).await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":2}"#));

    // Cancelled: the token is refused.
    let (status, _) = http(
        &base,
        "POST",
        &format!("/api/tasks/{task}/cancel"),
        "s",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let mut status = 0;
    for _ in 0..100 {
        status = http(&base, "POST", "/api/apps/pool/releases", &token, &bundle())
            .await
            .0;
        if status == 401 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, 401);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&apps_dir);
}
