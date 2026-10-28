//! Live PR state for the PR links of tasks this worker ran, read with `gh` here
//! so the orchestrator needs no GitHub credentials. Parsing and the poll
//! schedule are pure; `gh` runs behind `gh_view`.

use std::ffi::OsStr;
use std::time::Duration;

use mis_types::*;
use serde_json::Value;
use tokio::sync::mpsc;

/// `gh pr view --json` fields `parse` reads.
const FIELDS: &str =
    "number,title,state,isDraft,mergeable,reviewDecision,statusCheckRollup,headRefOid";
/// PRs checked per task; the orchestrator sends the most recent first.
const MAX_PRS: usize = 5;
const MAX_FAILING: usize = 5;
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// Pending checks are re-read every minute for half an hour, then every five
/// minutes for six hours.
const FAST: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(30 * 60));
const SLOW: (Duration, Duration) = (Duration::from_secs(5 * 60), Duration::from_secs(6 * 3600));

/// `gh` arguments for one PR.
fn args(url: &str) -> [&str; 5] {
    ["pr", "view", url, "--json", FIELDS]
}

/// `urls` that are GitHub PRs, canonical, at most `MAX_PRS`; anything else is
/// dropped before it can reach `gh`.
fn pr_urls(urls: &[String]) -> Vec<String> {
    let prs = urls.iter().filter_map(|u| github_link(u));
    let prs = prs.filter(|l| l.kind == LinkKind::Pr);
    prs.take(MAX_PRS).map(|l| l.url).collect()
}

/// `gh pr view --json FIELDS` output as a `PrStatus` read at `observed_at`.
fn parse(json: &str, observed_at: u64) -> Result<PrStatus, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad gh output: {e}"))?;
    let state = match (v["state"].as_str(), v["isDraft"].as_bool()) {
        (Some("MERGED"), _) => PrState::Merged,
        (Some("CLOSED"), _) => PrState::Closed,
        (Some("OPEN"), Some(true)) => PrState::Draft,
        (Some("OPEN"), _) => PrState::Open,
        (s, _) => return Err(format!("unknown PR state {s:?}")),
    };
    let (checks, failing) = rollup(v["statusCheckRollup"].as_array().map_or(&[], |a| a));
    Ok(PrStatus {
        state,
        checks,
        failing,
        mergeable: match v["mergeable"].as_str() {
            Some("MERGEABLE") => Some(true),
            Some("CONFLICTING") => Some(false),
            _ => None,
        },
        review: v["reviewDecision"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase),
        head: v["headRefOid"].as_str().unwrap_or("").into(),
        observed_at,
    })
}

/// Any failed check fails the PR; else any unfinished one keeps it pending;
/// else it passes (skipped and neutral count). Returns the failed check names.
fn rollup(items: &[Value]) -> (Checks, Vec<String>) {
    let mut failing = vec![];
    let mut pending = false;
    for c in items {
        // A CheckRun has status + conclusion; a StatusContext has state.
        let verdict = match c["status"].as_str() {
            Some(s) if s != "COMPLETED" => "PENDING",
            Some(_) => c["conclusion"].as_str().unwrap_or(""),
            None => c["state"].as_str().unwrap_or(""),
        };
        match verdict {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => {
                let name = c["name"].as_str().or(c["context"].as_str()).unwrap_or("?");
                if failing.len() < MAX_FAILING && !failing.iter().any(|f| f == name) {
                    failing.push(name.to_string());
                }
            }
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "" => {
                pending = true
            }
            _ => {}
        }
    }
    let checks = match () {
        _ if !failing.is_empty() => Checks::Failing,
        _ if pending => Checks::Pending,
        _ if items.is_empty() => Checks::None,
        _ => Checks::Passing,
    };
    (checks, failing)
}

/// How long until the next read, None to stop. Keeps going while any PR is
/// open with pending checks, or couldn't be read (None).
fn next_poll(elapsed: Duration, last: &[Option<PrStatus>]) -> Option<Duration> {
    let waiting = last.iter().any(|s| match s {
        None => true,
        Some(s) => matches!(s.state, PrState::Open | PrState::Draft) && s.checks == Checks::Pending,
    });
    match () {
        _ if !waiting => None,
        _ if elapsed < FAST.1 => Some(FAST.0),
        _ if elapsed < SLOW.1 => Some(SLOW.0),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
enum GhError {
    /// `gh` is missing or signed out: no point retrying.
    Unavailable(String),
    /// This read failed (network, unknown PR); the next poll retries.
    Failed(String),
}

/// Run `program pr view <url> --json …`; its stdout.
async fn gh_view(program: &OsStr, url: &str) -> Result<String, GhError> {
    let child = tokio::process::Command::new(program)
        .args(args(url))
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(GH_TIMEOUT, child).await {
        Err(_) => return Err(GhError::Failed("gh timed out".into())),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GhError::Unavailable("gh not found".into()))
        }
        Ok(Err(e)) => return Err(GhError::Failed(e.to_string())),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = first_line(err.trim(), 200);
    // gh exits 4 when it needs `gh auth login`.
    match out.status.code() == Some(4) || err.contains("gh auth login") {
        true => Err(GhError::Unavailable("gh is not signed in".into())),
        false => Err(GhError::Failed(err)),
    }
}

fn unix_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| d.as_millis() as u64)
}

/// Read each PR now and then on `next_poll`'s schedule, sending each read.
/// Stops for good, after one `PrUnavailable`, if `gh` can't be used.
pub async fn watch(program: &OsStr, task: TaskId, urls: &[String], tx: mpsc::Sender<WorkerMsg>) {
    let urls = pr_urls(urls);
    let start = tokio::time::Instant::now();
    loop {
        let mut last = vec![];
        for url in &urls {
            let status = match gh_view(program, url).await {
                Ok(json) => parse(&json, unix_ms()).map_err(GhError::Failed),
                Err(e) => Err(e),
            };
            let msg = match status {
                Ok(status) => {
                    last.push(Some(status.clone()));
                    let url = url.clone();
                    WorkerMsg::PrStatus { task, url, status }
                }
                Err(GhError::Unavailable(why)) => {
                    tracing::warn!(%task, "can't read PR status: {why}");
                    let _ = tx.send(WorkerMsg::PrUnavailable { task, why }).await;
                    return;
                }
                Err(GhError::Failed(why)) => {
                    tracing::debug!(%task, url, "PR status: {why}");
                    last.push(None);
                    continue;
                }
            };
            if tx.send(msg).await.is_err() {
                return;
            }
        }
        let Some(wait) = next_poll(start.elapsed(), &last) else {
            return;
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `gh pr view <url> --json FIELDS` on tommy-xr/MakeItSo, check fields trimmed.
    const PENDING: &str = include_str!("fixtures/pr45.json");
    const FAILING: &str = include_str!("fixtures/pr46.json");
    const PASSING: &str = include_str!("fixtures/pr39.json");
    const NO_CHECKS: &str = include_str!("fixtures/pr7.json");

    #[test]
    fn parses_recorded_prs() {
        let s = parse(PENDING, 7).unwrap();
        assert_eq!(
            (
                s.state,
                s.checks,
                s.mergeable,
                s.review.as_deref(),
                s.observed_at
            ),
            (PrState::Open, Checks::Pending, Some(true), None, 7)
        );
        assert_eq!(s.head, "86a2184b36bee166d6cdc47d51964c726a19ae6d");
        let s = parse(FAILING, 0).unwrap();
        assert_eq!(s.checks, Checks::Failing);
        // Cancelled counts as failed; skipped doesn't.
        assert_eq!(
            s.failing,
            ["check (ubuntu-latest)", "check (windows-latest)"]
        );
        let s = parse(PASSING, 0).unwrap();
        assert_eq!(
            (s.state, s.checks, s.mergeable),
            (PrState::Merged, Checks::Passing, None)
        );
        assert!(s.failing.is_empty());
        let s = parse(NO_CHECKS, 0).unwrap();
        assert_eq!((s.state, s.checks), (PrState::Merged, Checks::None));
        assert!(parse("not json", 0).is_err());
        assert!(parse(r#"{"state":"WEIRD"}"#, 0).is_err());
    }

    #[test]
    fn draft_review_and_status_contexts() {
        let json = r#"{"state":"OPEN","isDraft":true,"mergeable":"CONFLICTING",
            "reviewDecision":"CHANGES_REQUESTED","headRefOid":"abc","statusCheckRollup":[
            {"__typename":"StatusContext","context":"ci/legacy","state":"PENDING"},
            {"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"NEUTRAL"}]}"#;
        let s = parse(json, 0).unwrap();
        assert_eq!(
            (s.state, s.checks, s.mergeable, s.review.as_deref()),
            (
                PrState::Draft,
                Checks::Pending,
                Some(false),
                Some("changes_requested")
            )
        );
        let ctx = |state: &str| serde_json::json!({"context": "ci", "state": state});
        assert_eq!(rollup(&[ctx("SUCCESS")]).0, Checks::Passing);
        assert_eq!(rollup(&[ctx("EXPECTED")]).0, Checks::Pending);
        assert_eq!(
            rollup(&[ctx("ERROR"), ctx("PENDING")]),
            (Checks::Failing, vec!["ci".into()])
        );
        let many: Vec<Value> = (0..9)
            .map(|i| serde_json::json!({"name": format!("c{i}"), "status": "COMPLETED", "conclusion": "TIMED_OUT"}))
            .collect();
        assert_eq!(rollup(&many).1.len(), MAX_FAILING);
    }

    #[test]
    fn polls_while_pending_then_slower_then_stops() {
        let pending = parse(PENDING, 0).unwrap();
        let failing = parse(FAILING, 0).unwrap();
        let merged = PrStatus {
            state: PrState::Merged,
            ..pending.clone()
        };
        let min = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(next_poll(min(0), &[Some(failing.clone())]), None);
        assert_eq!(next_poll(min(0), &[Some(merged)]), None);
        let mixed = [Some(failing), Some(pending.clone())];
        assert_eq!(next_poll(min(29), &mixed), Some(min(1)));
        assert_eq!(next_poll(min(31), &mixed), Some(min(5)));
        assert_eq!(next_poll(min(6 * 60), &mixed), None);
        // An unread PR is retried.
        assert_eq!(next_poll(min(0), &[None]), Some(min(1)));
        assert_eq!(next_poll(min(0), &[]), None);
    }

    #[test]
    fn only_canonical_pr_urls_reach_gh() {
        let urls: Vec<String> = [
            "https://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/2/files",
            "https://github.com/o/r/issues/3",
            "--repo=evil",
            "https://evil.io/o/r/pull/4",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            pr_urls(&urls),
            [
                "https://github.com/o/r/pull/1",
                "https://github.com/o/r/pull/2"
            ]
        );
        let many: Vec<String> = (0..9)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        assert_eq!(pr_urls(&many), many[..MAX_PRS]);
        assert_eq!(args("u"), ["pr", "view", "u", "--json", FIELDS]);
    }

    /// Writes an executable `gh` stub printing `body` and exiting `code`.
    fn stub(dir: &std::path::Path, body: &str, code: i32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let out = dir.join("out.json");
        std::fs::write(&out, body).unwrap();
        let gh = dir.join("gh");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{}/args'\ncat '{}'\n[ {code} = 0 ] || echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\nexit {code}\n",
            dir.display(),
            out.display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mis-pr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watch_reads_each_pr_with_gh_and_stops_when_settled() {
        let dir = tempdir("ok");
        let gh = stub(&dir, FAILING, 0);
        let (tx, mut rx) = mpsc::channel(8);
        let task = TaskId::nil();
        let urls = [
            "https://github.com/o/r/pull/46".to_string(),
            "--evil".into(),
        ];
        watch(gh.as_os_str(), task, &urls, tx).await;
        let Some(WorkerMsg::PrStatus { url, status, .. }) = rx.recv().await else {
            panic!()
        };
        assert_eq!(
            (url.as_str(), status.checks),
            ("https://github.com/o/r/pull/46", Checks::Failing)
        );
        assert!(status.observed_at > 0);
        assert!(
            rx.recv().await.is_none(),
            "failing checks: no further polls"
        );
        let called = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(
            called.trim(),
            format!("pr view https://github.com/o/r/pull/46 --json {FIELDS}")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watch_reports_missing_or_signed_out_gh_once() {
        let dir = tempdir("auth");
        let gh = stub(&dir, "", 4);
        let urls: Vec<String> = (1..=3)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        for program in [gh.as_os_str(), OsStr::new("/nonexistent/gh")] {
            let (tx, mut rx) = mpsc::channel(8);
            watch(program, TaskId::nil(), &urls, tx).await;
            let Some(WorkerMsg::PrUnavailable { why, .. }) = rx.recv().await else {
                panic!()
            };
            assert!(why.starts_with("gh "), "{why}");
            assert!(rx.recv().await.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
