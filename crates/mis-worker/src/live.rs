//! Which sessions are open right now, so they aren't resumed in place underneath a
//! terminal. Claude records each running process in `<claude>/sessions/<pid>.json`
//! (`sessionId`, `pid`, `procStart`); only those `.json` files are read. Codex keeps
//! no such record, so it falls back to a recent write.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

/// Session ids named by `<claude>/sessions/*.json` whose process `alive(pid, procStart)`.
pub fn claude_open(claude: &Path, alive: impl Fn(u32, &str) -> bool) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(claude.join("sessions")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice::<Value>(&std::fs::read(p).ok()?).ok())
        .filter_map(|v| {
            let pid = u32::try_from(v["pid"].as_u64()?).ok()?;
            let alive = alive(pid, v["procStart"].as_str()?);
            alive.then(|| v["sessionId"].as_str().map(String::from))?
        })
        .collect()
}

/// `pid` runs and started at `start`, as `ps -o lstart` prints it in UTC (claude's
/// `procStart`), so a reused pid doesn't count.
#[cfg(unix)]
pub fn alive(pid: u32, start: &str) -> bool {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    proc_start(pid).is_some_and(|s| s == words(start))
}

/// Can't tell on Windows: nothing counts as open there.
#[cfg(not(unix))]
pub fn alive(_pid: u32, _start: &str) -> bool {
    false
}

#[cfg(unix)]
fn proc_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (out.status.success() && !s.is_empty()).then_some(s)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_live_processes_with_their_own_start_hold_a_session() {
        let dir = std::env::temp_dir().join(format!("mis-live-{}", uuid::Uuid::new_v4()));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let me = std::process::id();
        let started = proc_start(me).expect("ps reports this process");
        let file = |pid: u32, session: &str, start: &str| {
            let v = serde_json::json!({"pid": pid, "sessionId": session, "procStart": start});
            std::fs::write(
                sessions.join(format!("{pid}-{session}.json")),
                v.to_string(),
            )
            .unwrap();
        };
        file(me, "live", &format!("  {started} "));
        file(me, "reused-pid", "Mon Jan  1 00:00:00 2001");
        file(u32::MAX >> 1, "dead", &started);
        // Never opened: not JSON, and not ours to read.
        std::fs::write(sessions.join(format!("{me}.abc.key")), "{").unwrap();
        assert_eq!(
            claude_open(&dir, alive),
            HashSet::from(["live".to_string()])
        );
        assert!(claude_open(&dir.join("none"), alive).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
