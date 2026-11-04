//! `merge_pr` end to end: a routine with a merge policy runs on a real
//! orchestrator and worker; a fake claude calls `merge_pr`, and a fake `gh`
//! records its argv and answers with a fixture PR. Dry runs, live merges (with
//! and without the phone), refusals and denials.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

/// Stands in for claude: records its argv, calls `merge_pr` on the PR its prompt
/// names (`Merge PR <n>.`), and ends with the tool's answer as its result.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/claude-args"
cfg=""
while [ $# -gt 1 ]; do
  case "$1" in --mcp-config) cfg=$2; shift ;; esac
  shift
done
n=$(printf '%s' "$1" | sed -n 's/.*Merge PR \([0-9][0-9]*\)\..*/\1/p')
url=$(printf '%s' "$cfg" | sed 's/.*"url":"\([^"]*\)".*/\1/')
echo '{"type":"system","subtype":"init","session_id":"s1","model":"fake"}'
r=$(curl -s -H "Authorization: Bearer $MIS_MCP_TOKEN" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"merge_pr\",\"arguments\":{\"repo\":\"o/r\",\"number\":$n}}}" "$url")
t=$(printf '%s' "$r" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$t\"}"
"#;

/// Stands in for gh: records each argv as a line; `pr view <n>` prints
/// `$HOME/pr-<n>.json`, `pr merge` marks it merged there.
const FAKE_GH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-args"
case "$1 $2" in
  "pr view") cat "$HOME/pr-$3.json" ;;
  "pr merge") sed -i.bak 's/"state":"OPEN"/"state":"MERGED"/' "$HOME/pr-$3.json" ;;
  *) exit 1 ;;
esac
"#;

const HEAD: &str = "1111111111111111111111111111111111111111";

/// An eligible dependabot PR `n` on o/r.
fn pr(n: u64) -> Value {
    json!({
        "number": n, "title": "chore(deps): bump serde", "state": "OPEN",
        "author": {"login": "app/dependabot", "is_bot": true},
        "files": [{"path": "Cargo.lock", "additions": 3, "deletions": 3, "changeType": "MODIFIED"}],
        "additions": 3, "deletions": 3, "changedFiles": 1, "isDraft": false,
        "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "labels": [],
        "headRefOid": HEAD,
        "statusCheckRollup": [{"__typename": "CheckRun", "name": "check", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        "baseRefName": "main",
    })
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// Temp HOME with fake claude and gh on PATH, PR fixture `n`, and routine `deps`
/// (merge policy for o/r, `approve`) asking to merge it.
fn sandbox(url: &str, n: u64, pr: &Value, approve: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mis-merge-{}", uuid::Uuid::new_v4()));
    for d in ["bin", "state", "root", "routines"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    script(&dir.join("bin/claude"), FAKE_CLAUDE);
    script(&dir.join("bin/gh"), FAKE_GH);
    std::fs::write(dir.join(format!("pr-{n}.json")), pr.to_string()).unwrap();
    let toml = format!(
        "url = \"{url}\"\nroots = [\"{}\"]\n",
        dir.join("root").display()
    );
    std::fs::write(dir.join("state/worker.toml"), toml).unwrap();
    let routine = format!(
        "---\nevery: 1d\nact: true\nmerge:\n  repos: [o/r]\n  authors: [app/dependabot]\n  \
         max_files: 10\n  max_lines: 300\n  approve: {approve}\n---\nMerge PR {n}.\n"
    );
    std::fs::write(dir.join("routines/deps.md"), routine).unwrap();
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

/// `method path` on the orchestrator at `addr`; the status and body.
async fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Tasks from the first Status that passes `want`, asked for every 50 ms: not
/// every change is broadcast (a worker joining, a task finishing).
async fn polled(
    ws: &mut Ws,
    want: impl Fn(&[TaskSummary], &[WorkerSummary]) -> bool,
) -> Vec<TaskSummary> {
    let wait = async {
        loop {
            send(ws, &ClientMsg::Status).await;
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                if let Ok(ServerMsg::Status { tasks, workers, .. }) =
                    serde_json::from_str(msg.to_text().unwrap())
                {
                    if want(&tasks, &workers) {
                        return tasks;
                    }
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("timeout")
}

/// The phone approval's answer, and what happens to the fixture (its path) first.
type Answer = (Decision, fn(&Path));

struct Run {
    task: TaskSummary,
    /// The approval card, if one was asked.
    approval: Option<Approval>,
    /// Each `gh pr` call's argv, space-joined.
    gh: Vec<String>,
    claude: Vec<String>,
}

/// Run routine `deps` once on PR `n` (`pr` as gh shows it). `answer`: the phone
/// approval's answer, after `meanwhile` (the fixture's path) runs.
async fn run(n: u64, pr: Value, approve: &str, live: bool, answer: Option<Answer>) -> Run {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let dir = sandbox(&format!("ws://{addr}"), n, &pr, approve);
    let app = mis_orchestrator::App::new(None)
        .with_routines(dir.join("routines"), Duration::from_secs(3600));
    tokio::spawn(mis_orchestrator::serve(listener, app));
    let _worker = worker(&dir);
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/client"))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    polled(&mut client, |_, w| w.iter().any(|w| w.online)).await;
    if live {
        let (_, list) = http(&addr, "GET", "/api/routines", "").await;
        let list: Vec<RoutineView> = serde_json::from_str(&list).unwrap();
        let pin = json!({ "pin": list[0].pin }).to_string();
        let path = "/api/routines/deps/live";
        assert_eq!(http(&addr, "POST", path, &pin).await.0, 204);
    }
    let (status, body) = http(&addr, "POST", "/api/routines/deps/run", "").await;
    assert_eq!(status, 200, "{body}");
    let task: TaskId = serde_json::from_str::<Value>(&body).unwrap()["task"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut approval = None;
    if let Some((decision, meanwhile)) = answer {
        let tasks = polled(&mut client, |t, _| {
            t.iter().any(|t| t.id == task && !t.approvals.is_empty())
        })
        .await;
        let a = tasks.into_iter().find(|t| t.id == task).unwrap().approvals[0].clone();
        meanwhile(&dir.join(format!("pr-{n}.json")));
        send(
            &mut client,
            &ClientMsg::Approve {
                task,
                id: a.id,
                decision,
            },
        )
        .await;
        approval = Some(a);
    }
    let tasks = polled(&mut client, |t, _| {
        t.iter().any(|t| t.id == task && t.status.finished())
    })
    .await;
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let lines = |f: &str| read(f).lines().map(String::from).collect::<Vec<_>>();
    let out = Run {
        task: tasks.into_iter().find(|t| t.id == task).unwrap(),
        approval,
        // Less the worker's inventory probe (`gh --version`).
        gh: lines("gh-args")
            .into_iter()
            .filter(|l| l.starts_with("pr "))
            .collect(),
        claude: lines("claude-args"),
    };
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn view(n: u64) -> String {
    format!("pr view {n} -R o/r --json number,title,state,author,files,additions,deletions,changedFiles,isDraft,mergeable,mergeStateStatus,labels,headRefOid,statusCheckRollup,baseRefName")
}

fn merge(n: u64) -> String {
    format!("pr merge {n} -R o/r --squash --match-head-commit {HEAD}")
}

/// The task's one evidence row: its verdict and detail.
fn row(r: &Run) -> (Option<bool>, String) {
    let [l] = &r.task.links[..] else {
        panic!("{:?}", r.task.links)
    };
    assert_eq!((l.kind, l.label.as_str()), (LinkKind::Test, "merge o/r#7"));
    (l.ok, l.detail.clone().unwrap_or_default())
}

fn keep(_: &Path) {}

#[tokio::test]
async fn a_dry_run_says_it_would_merge_and_merges_nothing() {
    let r = run(7, pr(7), "none", false, None).await;
    assert_eq!(r.task.routine_mode, Some(RoutineMode::DryRun));
    assert_eq!(r.gh, [view(7)]);
    assert_eq!(
        row(&r),
        (None, "dry run: would merge (squash at 1111111)".into())
    );
    assert!(r.task.summary.unwrap().contains("would merge o/r#7"));
    assert!(
        !r.claude.contains(&"--disallowedTools".to_string()),
        "read-only already"
    );
}

#[tokio::test]
async fn live_merges_an_eligible_pr_pinned_to_the_head_it_checked() {
    let r = run(7, pr(7), "none", true, None).await;
    assert_eq!(r.task.routine_mode, Some(RoutineMode::Live));
    // Read back: merged, not merely queued.
    assert_eq!(r.gh, [view(7), merge(7), view(7)]);
    assert_eq!(row(&r), (Some(true), "merged (squash at 1111111)".into()));
    let deny = r
        .claude
        .iter()
        .position(|a| a == "--disallowedTools")
        .unwrap();
    assert_eq!(r.claude[deny + 1], "Bash(*gh pr merge*)");
}

#[tokio::test]
async fn live_refuses_an_ineligible_pr_by_its_rule() {
    let mut draft = pr(7);
    draft["isDraft"] = json!(true);
    let r = run(7, draft, "none", true, None).await;
    assert_eq!(r.gh, [view(7)]);
    assert_eq!(row(&r), (Some(false), "refused: it is a draft".into()));
    assert!(r
        .task
        .summary
        .unwrap()
        .contains("fails this routine's merge rules"));
}

#[tokio::test]
async fn phone_approval_shows_the_pr_and_allow_merges_it() {
    let r = run(7, pr(7), "phone", true, Some((Decision::Allow, keep))).await;
    let a = r.approval.clone().unwrap();
    assert_eq!((a.tool.as_str(), a.always), ("merge_pr", None));
    assert_eq!(a.summary, "Merge o/r#7: chore(deps): bump serde");
    let f = a.merge.unwrap();
    assert_eq!(
        (
            f.author.as_str(),
            f.files,
            f.checks.as_str(),
            f.head.as_str()
        ),
        ("app/dependabot", 1, "1 passed", HEAD)
    );
    // Read again after the answer, then merged.
    assert_eq!(r.gh, [view(7), view(7), merge(7), view(7)]);
    assert_eq!(row(&r).0, Some(true));
}

#[tokio::test]
async fn phone_deny_merges_nothing() {
    let r = run(7, pr(7), "phone", true, Some((Decision::Deny, keep))).await;
    assert_eq!(r.gh, [view(7)]);
    assert_eq!(
        row(&r),
        (Some(false), "not merged: the user denied it".into())
    );
}

#[tokio::test]
async fn a_push_while_the_phone_asks_is_refused() {
    fn pushed(fixture: &Path) {
        let mut v: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
        v["headRefOid"] = json!("2222222222222222222222222222222222222222");
        std::fs::write(fixture, v.to_string()).unwrap();
    }
    let r = run(7, pr(7), "phone", true, Some((Decision::Allow, pushed))).await;
    assert_eq!(r.gh, [view(7), view(7)]);
    assert_eq!(
        row(&r),
        (
            Some(false),
            "refused: it was pushed to or retargeted after you were asked".into()
        )
    );
}
