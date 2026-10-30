//! The worker binary sends the Cloudflare Access service token on its socket
//! upgrade, takes the secret from env only, and never logs it.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

const SECRET: &str = "shh-access-secret-0123";

/// Temp HOME holding a fake `claude` so the worker finds a harness.
fn sandbox() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-worker-access-{}", uuid::Uuid::new_v4()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    dir
}

fn worker(dir: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mis-worker"));
    cmd.env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", dir.join("bin").display()),
        )
        .env("HOME", dir)
        .env("MIS_STATE_DIR", dir.join("state"))
        .env("RUST_LOG", "trace")
        .current_dir(dir)
        .kill_on_drop(true);
    cmd
}

#[tokio::test]
#[allow(clippy::result_large_err)] // the handshake callback's signature, not ours
async fn access_headers_ride_the_upgrade_and_secret_stays_out_of_logs() {
    let dir = sandbox();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let mut child = worker(&dir)
        .env("MIS_URL", &url)
        .env("CF_ACCESS_CLIENT_ID", "abc.access")
        .env("CF_ACCESS_CLIENT_SECRET", SECRET)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let (tcp, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("worker never connected")
        .unwrap();
    let seen = Arc::new(Mutex::new(None));
    let capture = seen.clone();
    let mut ws = tokio_tungstenite::accept_hdr_async(tcp, move |req: &Request, resp: Response| {
        *capture.lock().unwrap() = Some((req.uri().path().to_string(), req.headers().clone()));
        Ok(resp)
    })
    .await
    .unwrap();
    // Wait for Hello so the worker has logged its startup.
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(hello.contains("\"hello\""), "{hello}");

    let (path, headers) = seen.lock().unwrap().take().unwrap();
    assert_eq!(path, "/ws/worker");
    assert_eq!(headers["cf-access-client-id"], "abc.access");
    assert_eq!(headers["cf-access-client-secret"], SECRET);

    child.start_kill().unwrap();
    let out = child.wait_with_output().await.unwrap();
    let logs = String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout);
    assert!(logs.contains("connecting to"), "{logs}");
    assert!(!logs.contains(SECRET), "secret leaked into logs:\n{logs}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn secret_is_not_a_flag() {
    let dir = sandbox();
    for args in [
        vec!["--cf-access-client-secret", SECRET],
        vec!["setup", "--cf-access-client-secret", SECRET],
    ] {
        let out = worker(&dir).args(&args).output().await.unwrap();
        assert!(!out.status.success(), "{args:?} was accepted");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("unexpected argument"), "{err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
