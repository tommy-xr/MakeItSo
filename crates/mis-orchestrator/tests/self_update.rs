//! An orchestrator on version A, following edge, finds B in a local stand-in for GitHub
//! Releases, self-tests it, swaps its executable, restarts and serves as B. A build that fails
//! its self-test is refused and A serves on. Runs a copy in a temp dir; B is that copy with its
//! version string patched. Unix only: there the restart keeps the pid.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mis_update::testing::*;

struct Run {
    dir: PathBuf,
    exe: PathBuf,
    child: tokio::process::Child,
    url: String,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A copy of orchestrator A in a temp dir, following edge at `releases`.
fn spawn_a(name: &str, releases: &str) -> Run {
    let dir = std::env::temp_dir().join(format!("mis-orch-update-{name}-{}", uuid::Uuid::new_v4()));
    let exe = dir.join("install/mis-orchestrator");
    write_exe(
        &exe,
        &std::fs::read(env!("CARGO_BIN_EXE_mis-orchestrator")).unwrap(),
    );
    let port = free_port();
    let mut child = tokio::process::Command::new(&exe)
        .args([
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--no-planner",
            "--no-summary",
        ])
        .args(["--update-channel", "edge", "--update-every", "1"])
        .env_clear()
        .env("PATH", "/bin:/usr/bin")
        .env("HOME", &dir)
        .env("MIS_STATE_DIR", dir.join("state"))
        .env("MIS_RELEASE_URL", releases)
        .env("MIS_RELEASE_KEY", format!("{FIXTURES}/test.pub"))
        .env("RUST_LOG", "info,mis_orchestrator=info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Logs on stdout, banner on stderr; drain both so neither pipe fills.
    drain(child.stdout.take().unwrap());
    drain(child.stderr.take().unwrap());
    Run {
        dir,
        exe,
        child,
        url: format!("http://127.0.0.1:{port}"),
    }
}

fn drain(out: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(out).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.starts_with(' ') && !line.is_empty() {
                println!("orch| {line}");
            }
        }
    });
}

async fn get(url: &str) -> Option<serde_json::Value> {
    reqwest::get(url)
        .await
        .ok()?
        .text()
        .await
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Poll `url` until `pred` holds.
async fn until(url: &str, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    loop {
        if let Some(v) = get(url).await.filter(|v| pred(v)) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn version_a() -> Option<String> {
    let built = Path::new(env!("CARGO_BIN_EXE_mis-orchestrator"));
    let v = release_version(built, "mis-orchestrator");
    if v.is_none() {
        eprintln!("skipped: this build's version has no clean commit, so it never self-updates");
    }
    v
}

#[tokio::test]
async fn orchestrator_restarts_on_a_newer_edge_build() {
    let Some(a) = version_a() else { return };
    let b = other_version(&a);
    let tmp = std::env::temp_dir().join(format!("mis-orch-b-{}", uuid::Uuid::new_v4()));
    let built = std::fs::read(env!("CARGO_BIN_EXE_mis-orchestrator")).unwrap();
    let b_exe = patched(&built, &a, &b, &tmp.join("mis-orchestrator"));
    let _ = std::fs::remove_dir_all(&tmp);
    let (base, hits) =
        release_server(edge_release("mis-orchestrator", &b_exe, &b, "test.key")).await;
    println!("A = {a}, B = {b}, release server {base}");
    let mut run = spawn_a("swap", &base);
    let info = format!("{}/api/info", run.url);
    let ok = tokio::time::timeout(Duration::from_secs(60), async {
        let v = until(&info, |_| true).await;
        println!("test| serving {}", v["version"]);
        assert_eq!(v["version"], a.as_str());
        until(&info, |v| v["version"] == b.as_str()).await;
        println!("test| serving {b}");
    })
    .await;
    let asked = hits.lock().unwrap().clone();
    println!("release server served: {asked:?}");
    assert!(ok.is_ok(), "never came back as B");
    assert_eq!(
        std::fs::read(&run.exe).unwrap(),
        b_exe,
        "executable swapped for B"
    );
    assert!(
        run.child.try_wait().unwrap().is_none(),
        "same process, re-exec'd"
    );
    run.child.start_kill().unwrap();
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[tokio::test]
async fn a_build_failing_its_self_test_is_refused() {
    let Some(a) = version_a() else { return };
    let b = other_version(&a);
    // Says the right version, fails everything else.
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'mis-orchestrator {b}'; exit 0; }}\necho 'cannot read the ledger' >&2; exit 3\n"
    );
    let (base, _) = release_server(edge_release(
        "mis-orchestrator",
        script.as_bytes(),
        &b,
        "test.key",
    ))
    .await;
    let mut run = spawn_a("selftest", &base);
    let a_exe = std::fs::read(&run.exe).unwrap();
    let update = format!("{}/api/update", run.url);
    let ok = tokio::time::timeout(Duration::from_secs(60), async {
        let u = until(&update, |v| v["update"]["state"] == "failed").await;
        println!("test| /api/update: {u}");
        assert!(u["update"]["subject"]
            .as_str()
            .unwrap()
            .contains("cannot read the ledger"));
    })
    .await;
    assert!(ok.is_ok(), "no failure reported");
    let v = get(&format!("{}/api/info", run.url)).await.unwrap();
    assert_eq!(v["version"], a.as_str(), "A serves on");
    assert_eq!(
        std::fs::read(&run.exe).unwrap(),
        a_exe,
        "executable untouched"
    );
    run.child.start_kill().unwrap();
    let _ = std::fs::remove_dir_all(&run.dir);
}
