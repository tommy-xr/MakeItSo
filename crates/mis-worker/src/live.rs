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

/// `pid` runs and, where claude wrote `start` as `ps -o lstart` prints it in UTC
/// (macOS), started then, so a reused pid doesn't count. A `start` in another form
/// can't be compared: a live pid is enough, as a missed open session is the worse error.
#[cfg(unix)]
pub fn alive(pid: u32, start: &str) -> bool {
    let start = words(start);
    let lstart = start.split(' ').count() == 5 && start.matches(':').count() == 2;
    proc_start(pid).is_some_and(|s| s == start || !lstart)
}

/// Can't tell on Windows: every recorded session counts as open, so it is sent to a copy.
#[cfg(not(unix))]
pub fn alive(_pid: u32, _start: &str) -> bool {
    true
}

#[cfg(unix)]
fn words(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// When `pid` started, `Sat Oct 31 17:14:04 2026` in UTC; None if it isn't running.
#[cfg(unix)]
fn proc_start(pid: u32) -> Option<String> {
    let mut ps = std::process::Command::new("ps");
    ps.args(["-o", "lstart=", "-p", &pid.to_string()]);
    ps.env("TZ", "UTC").env("LC_ALL", "C");
    let s = words(&crate::inventory::output_within(ps)?);
    (!s.is_empty()).then_some(s)
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
        // Not in `ps` form (another platform's): a live pid is enough.
        file(me, "other-form", "1793570000");
        file(u32::MAX >> 1, "dead", &started);
        // Never opened: not JSON, and not ours to read.
        std::fs::write(sessions.join(format!("{me}.abc.key")), "{").unwrap();
        assert_eq!(
            claude_open(&dir, alive),
            HashSet::from(["live".to_string(), "other-form".to_string()])
        );
        assert!(claude_open(&dir.join("none"), alive).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
