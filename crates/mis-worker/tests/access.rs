//! The worker binary sends the Cloudflare Access service token on its socket
//! upgrade, takes the secret from env only, and never logs it.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

const SECRET: &str = "shh-access-secret-0123";
/// Rides an orchestrator frame; must not show up in trace logs either.
const PAYLOAD: &str = "frame-marker-4567";

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

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
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
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Logs go to stdout. Drain from the start: trace output would fill the pipe and stall the worker.
    let (reg_tx, reg_rx) = tokio::sync::oneshot::channel();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let drain = tokio::spawn(async move {
        let mut logs = String::new();
        let mut reg_tx = Some(reg_tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("registered") {
                reg_tx.take().map(|t| t.send(()));
            }
            logs.push_str(&line);
            logs.push('\n');
        }
        logs
    });

    let seen = Arc::new(Mutex::new(None));
    let capture = seen.clone();
    let run = async {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws =
            tokio_tungstenite::accept_hdr_async(tcp, move |req: &Request, resp: Response| {
                *capture.lock().unwrap() =
                    Some((req.uri().path().to_string(), req.headers().clone()));
                Ok(resp)
            })
            .await
            .unwrap();
        let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
        assert!(hello.contains("\"hello\""), "{hello}");
        // A frame carrying the marker, then a Welcome; once the worker logs
        // "registered" it has read (and could have traced) both.
        ws.send(Message::text(PAYLOAD)).await.unwrap();
        let welcome = format!(r#"{{"type":"welcome","id":"{}"}}"#, uuid::Uuid::nil());
        ws.send(Message::text(welcome)).await.unwrap();
        reg_rx.await.expect("worker exited before registering");
    };
    let ok = tokio::time::timeout(Duration::from_secs(10), run).await;
    child.start_kill().unwrap();
    let logs = drain.await.unwrap();
    assert!(ok.is_ok(), "worker never registered:\n{logs}");

    let (path, headers) = seen.lock().unwrap().take().unwrap();
    assert_eq!(path, "/ws/worker");
    assert_eq!(headers["cf-access-client-id"], "abc.access");
    assert_eq!(headers["cf-access-client-secret"], SECRET);
    assert!(logs.contains("connecting to"), "{logs}");
    for s in [SECRET, PAYLOAD] {
        assert!(
            !logs.contains(s) && !logs.contains(&hex(s)),
            "{s} leaked:\n{logs}"
        );
    }
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
