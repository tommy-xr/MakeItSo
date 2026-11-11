//! `export` on one machine, `import` on another: an install moved whole. Each machine is a temp
//! HOME; the binary runs with only it, PATH and `MIS_HOSTNAME=demo-mac`. Synthetic data only.
//! Unix: modes are checked.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_orchestrator::push::Push;
use mis_orchestrator::records_db::Records;
use mis_orchestrator::store::Store;
use mis_types::{ClientMsg, Mutation, ServerMsg, Space};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const TOKEN: &str = "t-demo-1";
/// 2023-11-14: an age that images expire and captures dedupe by.
fn long_ago() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// A machine: a HOME with the default install paths under it.
struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("mis-transfer-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn state(&self) -> PathBuf {
        self.0.join(".local/state/mis")
    }
    fn routines(&self) -> PathBuf {
        self.0.join(".config/mis/routines")
    }
    fn env(&self) -> PathBuf {
        self.0.join(".config/mis/orchestrator.env")
    }
    /// `mis-orchestrator args` on this machine.
    fn orch(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).output().unwrap();
        println!("$ mis-orchestrator {}", args.join(" "));
        print!("{}", String::from_utf8_lossy(&out.stdout));
        print!("{}", String::from_utf8_lossy(&out.stderr));
        out
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mis-orchestrator"));
        c.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &self.0)
            .env("MIS_HOSTNAME", "demo-mac")
            .env("NO_COLOR", "1");
        c
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn put(id: &str, data: serde_json::Value) -> Mutation {
    Mutation::Put {
        space: Space::Shared,
        collection: "notes".into(),
        id: id.into(),
        rev: 0,
        data,
    }
}

/// The old machine's install: two tasks, a VAPID key, app `pool` with its rows only in its
/// WAL, a routine, an env file, and what an export leaves out. The VAPID public key.
fn old_install(home: &Home) -> String {
    let state = home.state();
    let (mut store, mut ledger, _) = Store::open(&state, Duration::ZERO, 0).unwrap();
    for prompt in ["water the plants", "renew the passport"] {
        ledger.submit(prompt.into(), None, Some("claude".into()), None, vec![]);
    }
    assert!(store.save(ledger.snapshot(store.written())));
    drop(store);
    let vapid = Push::open(state.clone(), "mailto:demo@example.com".into()).unwrap();
    std::fs::create_dir_all(state.join("apps/pool")).unwrap();
    let records = Records::new(state.join("apps"));
    let row = serde_json::json!({"chlorine": 3});
    records
        .mutate("pool", &put("a", row), "me", 1)
        .unwrap()
        .unwrap();
    // Never closed, as if the orchestrator was killed: the rows stay in the WAL.
    std::mem::forget(records);
    let wal = state.join("apps/pool/data.sqlite-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    for left_out in [
        "builds/abc/mis-worker",
        "notes/README.md",
        "ledger.json.bad-1",
        "x.tmp",
    ] {
        let p = state.join(left_out);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "left out").unwrap();
    }
    std::fs::create_dir_all(home.routines()).unwrap();
    std::fs::write(home.routines().join("weekly.md"), "every week").unwrap();
    let f = std::fs::File::options()
        .write(true)
        .open(home.routines().join("weekly.md"));
    f.unwrap().set_modified(long_ago()).unwrap();
    // The old home's state dir: missing on the new machine.
    let env = format!("MIS_TOKEN={TOKEN}\nMIS_STATE_DIR={}\n", state.display());
    std::fs::write(home.env(), env).unwrap();
    vapid.public_key()
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test]
async fn an_install_moves_whole_and_serves_the_same() {
    let (old, new) = (Home::new("old"), Home::new("new"));
    let vapid = old_install(&old);
    let archive = old.0.join("mis.tar.gz");
    let env = old.env();
    let out = old.orch(&[
        "export",
        archive.to_str().unwrap(),
        "--env-file",
        env.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    assert!(text(&out).contains("Exported 8 files"), "{}", text(&out));
    assert!(text(&out).contains("Never run the tunnel on both machines at once"));
    assert_eq!(mode(&archive), 0o600);
    // Carried to the other machine; the old one is gone.
    let moved = new.0.join("mis.tar.gz");
    std::fs::copy(&archive, &moved).unwrap();
    let state_was = old.state();
    drop(old);

    let out = new.orch(&["import", moved.to_str().unwrap()]);
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    assert!(shown.contains("Imported 2 tasks, 8 files"), "{shown}");
    let stale = format!(
        "warning: MIS_STATE_DIR={}: not on this machine",
        state_was.display()
    );
    assert!(shown.contains(&stale), "{shown}");
    assert!(shown.contains("Run a worker on this machine") && shown.contains("worker.toml"));
    assert!(shown.contains("mis-orchestrator doctor:\n") && shown.contains("state dir"));
    let state = new.state();
    for (p, m) in [
        (state.clone(), 0o700),
        (state.join("apps/pool"), 0o700),
        (state.join("ledger.json"), 0o600),
        (state.join("apps/pool/data.sqlite-wal"), 0o600),
        (new.env(), 0o600),
        (new.routines().join("weekly.md"), 0o600),
    ] {
        assert_eq!(mode(&p), m, "{}", p.display());
    }
    let modified = std::fs::metadata(new.routines().join("weekly.md"))
        .unwrap()
        .modified();
    assert_eq!(modified.unwrap(), long_ago(), "ages kept");
    for gone in [
        "builds",
        "notes",
        "ledger.json.bad-1",
        "x.tmp",
        "apps/pool/data.sqlite-shm",
    ] {
        assert!(!state.join(gone).exists(), "{gone}");
    }

    // Served from the new install, its env file fixed by the process env.
    let port = free_port();
    let mut serve = tokio::process::Command::from(new.cmd(&[
        "--bind",
        &format!("127.0.0.1:{port}"),
        "--no-planner",
        "--no-summary",
        "--update-channel",
        "off",
    ]));
    let child = serve
        .env("MIS_ENV_FILE", new.env())
        .env("MIS_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let info = loop {
        match reqwest::get(format!("{base}/api/info")).await {
            Ok(r) => break r.text().await.unwrap(),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(
        info["vapid"], vapid,
        "same VAPID key: subscriptions still work"
    );
    let mut req = format!("ws://127.0.0.1:{port}/ws/client")
        .into_client_request()
        .unwrap();
    let auth = format!("Bearer {TOKEN}").parse().unwrap();
    req.headers_mut().insert("authorization", auth);
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let status = serde_json::to_string(&ClientMsg::Status).unwrap();
    ws.send(Message::Text(status.into())).await.unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let ServerMsg::Status { tasks, .. } = serde_json::from_str(msg.to_text().unwrap()).unwrap()
    else {
        panic!("{msg}")
    };
    let mut prompts: Vec<_> = tasks.iter().map(|t| t.prompt.as_str()).collect();
    prompts.sort();
    assert_eq!(prompts, ["renew the passport", "water the plants"]);
    drop(child);
    let records = Records::new(state.join("apps"));
    let row = records
        .get("pool", Space::Shared, "", "notes", "a")
        .unwrap();
    assert_eq!(row.unwrap().data, serde_json::json!({"chlorine": 3}));
}

#[test]
fn export_and_import_refuse_to_overwrite_or_race() {
    let (old, new) = (Home::new("old"), Home::new("new"));
    old_install(&old);
    let archive = old.0.join("mis.tar.gz");
    let path = archive.to_str().unwrap();
    // A running orchestrator holds the state dir.
    let running = Store::open(&old.state(), Duration::ZERO, 0).unwrap();
    let out = old.orch(&["export", path]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("another orchestrator is using"),
        "{}",
        text(&out)
    );
    assert!(!archive.exists());
    drop(running);
    let inside = old.state().join("mis.tar.gz");
    let out = old.orch(&["export", inside.to_str().unwrap()]);
    assert!(
        text(&out).contains("is inside the state dir"),
        "{}",
        text(&out)
    );
    assert!(old.orch(&["export", path]).status.success());
    let out = old.orch(&["export", path]);
    assert!(
        text(&out).contains("File exists"),
        "never overwritten: {}",
        text(&out)
    );

    // A target that already has a state dir, or an env file.
    std::fs::create_dir_all(new.state()).unwrap();
    std::fs::write(new.state().join("ledger.json"), "mine").unwrap();
    let out = new.orch(&["import", path]);
    assert!(text(&out).contains("isn't empty"), "{}", text(&out));
    std::fs::remove_dir_all(new.state()).unwrap();
    std::fs::create_dir_all(new.env().parent().unwrap()).unwrap();
    std::fs::write(new.env(), "MIS_TOKEN=mine\n").unwrap();
    let out = new.orch(&["import", path]);
    assert!(
        text(&out).contains("exists; move it aside"),
        "{}",
        text(&out)
    );
    assert_eq!(
        std::fs::read_to_string(new.env()).unwrap(),
        "MIS_TOKEN=mine\n"
    );
    assert!(!new.state().exists(), "nothing written");
}

/// A `.tar.gz` of a manifest this build reads, then `entry`.
fn archive_with(entry: tar::Header, body: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let manifest = format!(
        r#"{{"version":"x","ledger_version":{}}}"#,
        mis_orchestrator::ledger::persist::VERSION
    );
    let mut h = tar::Header::new_gnu();
    h.set_size(manifest.len() as u64);
    b.append_data(&mut h, "manifest.json", manifest.as_bytes())
        .unwrap();
    b.append(&entry, body).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &b.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

/// A file entry named `raw`, as written: tar's own builder refuses `..` and absolute paths.
fn named(raw: &str) -> tar::Header {
    let mut h = tar::Header::new_old();
    h.as_old_mut().name[..raw.len()].copy_from_slice(raw.as_bytes());
    h.set_size(1);
    h.set_mode(0o644);
    h.set_cksum();
    h
}

#[test]
fn a_malicious_archive_writes_nothing() {
    let new = Home::new("new");
    let mut link = tar::Header::new_gnu();
    link.set_path("state/ledger.json").unwrap();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_link_name("/etc/passwd").unwrap();
    link.set_size(0);
    link.set_cksum();
    for (entry, want) in [
        (link, "Symlink entries aren't allowed"),
        (named("state/../../.ssh/authorized_keys"), "`..` in path"),
        (named("/etc/evil"), "absolute path"),
        (named("config/worker.toml"), "not part of an export"),
    ] {
        let file = new.0.join("evil.tar.gz");
        std::fs::write(&file, archive_with(entry, b"x")).unwrap();
        let out = new.orch(&["import", file.to_str().unwrap()]);
        assert!(!out.status.success());
        assert!(text(&out).contains(want), "{want}: {}", text(&out));
        assert!(!new.state().exists() && !new.env().exists(), "{want}");
        assert!(!new.0.join(".ssh").exists());
    }
}
