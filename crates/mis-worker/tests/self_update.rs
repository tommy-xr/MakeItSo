//! A worker on version A, connected to an orchestrator on B, downloads B's signed build from a
//! local stand-in for GitHub Releases, swaps its executable, restarts and says Hello as B. One
//! signed with another key is refused and the worker runs on. Runs a copy in a temp dir; B is
//! that copy with its version string patched. Unix only: there the restart keeps the pid.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../mis-update/tests/fixtures");

/// The built worker's version, if it is a release build (a dirty tree's isn't, and won't update).
fn version_a() -> Option<String> {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mis-worker"))
        .arg("--version")
        .output()
        .unwrap();
    let v = String::from_utf8(out.stdout).unwrap();
    let v = v.trim().strip_prefix("mis-worker ").unwrap().to_string();
    mis_update::Release::parse(&v).map(|_| v)
}

/// `exe` with every copy of version `a` swapped for `b` (same length), runnable.
fn patched(exe: &[u8], a: &str, b: &str, out: &Path) -> Vec<u8> {
    assert_eq!(a.len(), b.len());
    let mut bytes = exe.to_vec();
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(a.len()).position(|w| w == a.as_bytes()) {
        bytes[i + at..i + at + a.len()].copy_from_slice(b.as_bytes());
        i += at + a.len();
        n += 1;
    }
    assert!(n > 0, "version string not found in the binary");
    write_exe(out, &bytes);
    // Patching broke the ad-hoc signature macOS requires to run it.
    if cfg!(target_os = "macos") {
        let ok = std::process::Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(out)
            .status()
            .unwrap();
        assert!(ok.success());
        bytes = std::fs::read(out).unwrap();
    }
    bytes
}

fn write_exe(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn tar_gz(name: &str, data: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o755);
    h.set_cksum();
    b.append_data(&mut h, name, data).unwrap();
    b.into_inner().unwrap().finish().unwrap()
}

fn sign(key: &str, data: &[u8], comment: &str) -> String {
    let key = std::fs::read_to_string(format!("{FIXTURES}/{key}")).unwrap();
    let sk = minisign::SecretKeyBox::from_string(&key)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, Some(comment), None)
        .unwrap()
        .into_string()
}

/// Serves `files` under `/download/edge/`; returns the base URL and the paths asked for.
async fn release_server(files: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    let hits = Arc::new(Mutex::new(vec![]));
    let state = (Arc::new(files), hits.clone());
    type S = (Arc<HashMap<String, Vec<u8>>>, Arc<Mutex<Vec<String>>>);
    let app = axum::Router::new()
        .route(
            "/download/edge/{name}",
            axum::routing::get(
                |State((files, hits)): State<S>, UrlPath(name): UrlPath<String>| async move {
                    hits.lock().unwrap().push(name.clone());
                    files.get(&name).cloned().ok_or(StatusCode::NOT_FOUND)
                },
            ),
        )
        .with_state(state);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, hits)
}

struct Run {
    dir: PathBuf,
    exe: PathBuf,
    child: tokio::process::Child,
    orch: TcpListener,
    logs: Arc<Mutex<String>>,
}

/// The copy of worker A in a temp dir, pointed at a fake orchestrator and `releases`.
async fn spawn_a(name: &str, releases: &str) -> Run {
    let dir = std::env::temp_dir().join(format!("mis-self-update-{name}-{}", uuid::Uuid::new_v4()));
    let exe = dir.join("install/mis-worker");
    write_exe(
        &exe,
        &std::fs::read(env!("CARGO_BIN_EXE_mis-worker")).unwrap(),
    );
    write_exe(&dir.join("bin/claude"), b"#!/bin/sh\nexit 1\n");
    let orch = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = tokio::process::Command::new(&exe)
        .env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", dir.join("bin").display()),
        )
        .env("HOME", &dir)
        .env("MIS_STATE_DIR", dir.join("state"))
        .env("MIS_URL", format!("ws://{}", orch.local_addr().unwrap()))
        .env("MIS_RELEASE_URL", releases)
        .env("MIS_RELEASE_KEY", format!("{FIXTURES}/test.pub"))
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let logs = Arc::new(Mutex::new(String::new()));
    let sink = logs.clone();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            println!("worker| {line}");
            sink.lock().unwrap().push_str(&(line + "\n"));
        }
    });
    Run {
        dir,
        exe,
        child,
        orch,
        logs,
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Accept the worker's next connection; returns it and the version its Hello reports.
async fn hello(orch: &TcpListener) -> (Ws, Option<String>) {
    let (tcp, _) = orch.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    match next(&mut ws).await {
        Some(WorkerMsg::Hello { info, .. }) => (ws, info.version),
        other => panic!("expected Hello, got {other:?}"),
    }
}

async fn next(ws: &mut Ws) -> Option<WorkerMsg> {
    while let Some(Ok(m)) = ws.next().await {
        if let Message::Text(t) = m {
            return Some(serde_json::from_str(&t).unwrap());
        }
    }
    None
}

async fn welcome(ws: &mut Ws, version: &str) {
    let w = OrchMsg::Welcome {
        id: uuid::Uuid::nil(),
        version: Some(version.into()),
        channel: Some(Channel::Edge),
    };
    let w = serde_json::to_string(&w).unwrap();
    ws.send(Message::text(w)).await.unwrap();
}

/// The next `WorkerMsg::Update`, skipping stats.
async fn update(ws: &mut Ws) -> WorkerUpdate {
    loop {
        match next(ws).await {
            Some(WorkerMsg::Update { update }) => return update,
            Some(_) => continue,
            None => panic!("worker hung up before reporting its update"),
        }
    }
}

/// Worker A and a release server holding B's tarball signed with `key`.
async fn setup(name: &str, key: &str) -> Option<(Run, String, Vec<u8>, Arc<Mutex<Vec<String>>>)> {
    let Some(a) = version_a() else {
        eprintln!("skipped: this build's version has no clean commit, so it never self-updates");
        return None;
    };
    let (semver, sha) = a.split_once('+').unwrap();
    let b = format!(
        "{semver}+{}",
        if sha == "bbbbbbb" {
            "ccccccc"
        } else {
            "bbbbbbb"
        }
    );
    let tmp = std::env::temp_dir().join(format!("mis-self-update-b-{}", uuid::Uuid::new_v4()));
    let exe = std::fs::read(env!("CARGO_BIN_EXE_mis-worker")).unwrap();
    let b_exe = patched(&exe, &a, &b, &tmp.join("mis-worker"));
    let _ = std::fs::remove_dir_all(&tmp);
    let asset = mis_update::asset("", Channel::Edge, &b, mis_update::TARGET).unwrap();
    let tarball = tar_gz("mis-worker", &b_exe);
    let sig = sign(key, &tarball, &format!("mis {b} {}", asset.name));
    let files = HashMap::from([
        (format!("{}.minisig", asset.name), sig.into_bytes()),
        (asset.name.clone(), tarball),
    ]);
    let (base, hits) = release_server(files).await;
    println!("A = {a}, B = {b}, release server {base}");
    Some((spawn_a(name, &base).await, b, b_exe, hits))
}

#[tokio::test]
async fn worker_switches_to_the_orchestrators_signed_build() {
    let Some((mut run, b, b_exe, hits)) = setup("swap", "test.key").await else {
        return;
    };
    let ok = tokio::time::timeout(Duration::from_secs(60), async {
        let (mut ws, v) = hello(&run.orch).await;
        println!("orch| hello from {v:?}; welcome as {b}");
        assert_ne!(v.as_deref(), Some(b.as_str()));
        welcome(&mut ws, &b).await;
        let u = update(&mut ws).await;
        println!("orch| update: {u:?}");
        assert_eq!(u, WorkerUpdate::Pending { version: b.clone() });
        let (mut ws, v) = hello(&run.orch).await;
        println!("orch| hello from {v:?}");
        assert_eq!(v.as_deref(), Some(b.as_str()), "back on B");
        welcome(&mut ws, &b).await;
    })
    .await;
    let logs = run.logs.lock().unwrap().clone();
    assert!(ok.is_ok(), "timed out:\n{logs}");
    assert_eq!(
        std::fs::read(&run.exe).unwrap(),
        b_exe,
        "executable swapped for B"
    );
    let asked = hits.lock().unwrap().clone();
    println!("release server served: {asked:?}");
    assert_eq!(asked.len(), 2, "signature and tarball, once: {asked:?}");
    run.child.start_kill().unwrap();
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[tokio::test]
async fn worker_refuses_a_build_signed_with_another_key() {
    let Some((mut run, b, _, _)) = setup("badsig", "other.key").await else {
        return;
    };
    let a_exe = std::fs::read(&run.exe).unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(60), async {
        let (mut ws, _) = hello(&run.orch).await;
        welcome(&mut ws, &b).await;
        let u = update(&mut ws).await;
        println!("orch| update: {u:?}");
        let WorkerUpdate::Failed { version, why } = u else {
            panic!("{u:?}")
        };
        assert_eq!(version, b);
        assert!(why.contains("signature"), "{why}");
    })
    .await;
    let logs = run.logs.lock().unwrap().clone();
    assert!(ok.is_ok(), "timed out:\n{logs}");
    assert_eq!(
        std::fs::read(&run.exe).unwrap(),
        a_exe,
        "executable untouched"
    );
    run.child.start_kill().unwrap();
    let _ = std::fs::remove_dir_all(&run.dir);
}
