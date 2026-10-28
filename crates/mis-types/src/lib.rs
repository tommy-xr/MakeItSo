//! Wire types shared by orchestrator, workers, and clients.
//! Everything is JSON over WebSocket; `type` tags on the enums below.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Env var set on agent processes: the id of the task they are running.
pub const TASK_ENV: &str = "MIS_TASK";
use uuid::Uuid;

mod access;
pub use access::access_headers;

pub type WorkerId = Uuid;
pub type TaskId = Uuid;

/// How often a worker sends `WorkerMsg::Stats`; the orchestrator treats these
/// as heartbeats and drops a worker silent for `2 * STATS_INTERVAL + 5s`.
pub const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Machine load sampled by a worker. Percentages are 0-100.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkerStats {
    /// Free space on the filesystem holding the worker's first root.
    pub disk_free_gb: f32,
    pub disk_used_pct: u8,
    pub cpu_pct: u8,
    pub mem_pct: u8,
}

/// Reported by a worker on connect; the orchestrator keeps these as its ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    /// Agent harnesses this worker can run, e.g. "claude", "codex".
    pub harnesses: Vec<String>,
    /// Directories tasks may run in; a task's cwd must resolve under one.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Models the planner may pick, per harness; a harness without an entry
    /// runs its own default.
    #[serde(default)]
    pub models: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: TaskId,
    pub prompt: String,
    /// Working directory on the worker machine; None = worker default.
    pub cwd: Option<String>,
    pub harness: String,
    /// Model passed to the harness (`--model`); None = harness default.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The orchestrator's planner is choosing a harness and rewriting the prompt.
    Planning,
    Queued,
    Running,
    Done,
    Failed,
}

/// Harness-agnostic view of what an agent is doing. Visualizers build on this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Planner decided; the task is now queued with this harness and prompt.
    Planned {
        harness: String,
        prompt: String,
        #[serde(default)]
        model: Option<String>,
    },
    Started {
        session: Option<String>,
        /// Model the agent reports running, if it says.
        #[serde(default)]
        model: Option<String>,
    },
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: String,
    },
    /// One-line progress note the agent posted via the worker's `report_status` tool.
    Status {
        text: String,
    },
    /// Tokens used so far this run: cumulative, so each replaces the last.
    /// `input` includes cached prompt tokens. Sent before `Finished`.
    Usage {
        input: u64,
        output: u64,
    },
    Finished {
        ok: bool,
        summary: Option<String>,
    },
}

/// Worker -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// `id` is stable across reconnects; `running` lists tasks still in flight
    /// so the orchestrator can keep them instead of failing them.
    Hello {
        id: WorkerId,
        info: WorkerInfo,
        running: Vec<TaskId>,
    },
    Event {
        task: TaskId,
        event: AgentEvent,
    },
    /// Sent after Hello and every `STATS_INTERVAL`; doubles as the heartbeat.
    Stats(WorkerStats),
}

/// Orchestrator -> worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchMsg {
    Welcome { id: WorkerId },
    Assign { task: Task },
}

/// Client (CLI / voice app) -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Status,
    /// `harness` None lets the orchestrator's planner choose and rewrite the prompt.
    Submit {
        prompt: String,
        cwd: Option<String>,
        harness: Option<String>,
        /// None = harness default.
        #[serde(default)]
        model: Option<String>,
    },
    /// Stream all task events from now on.
    Subscribe,
    /// One task's metadata and retained transcript.
    Show {
        task: TaskId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerSummary {
    pub id: WorkerId,
    pub info: WorkerInfo,
    pub busy: bool,
    /// False while inside the reconnect grace period.
    pub online: bool,
    /// Latest `WorkerMsg::Stats`; None until the first arrives.
    pub stats: Option<WorkerStats>,
    /// Unix seconds when `stats` arrived, so clients can judge staleness.
    pub stats_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub id: TaskId,
    pub prompt: String,
    pub status: TaskStatus,
    pub worker: Option<WorkerId>,
    pub harness: String,
    /// Requested model, else the one the `Started` event reported.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent session id, from the `Started` event.
    pub session: Option<String>,
    /// From the `Finished` event.
    pub summary: Option<String>,
    /// Unix millis when submitted / when it finished.
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// Latest `Usage`; None until one arrives.
    #[serde(default)]
    pub tokens_in: Option<u64>,
    #[serde(default)]
    pub tokens_out: Option<u64>,
    /// What it is doing now, e.g. `> Bash ls`, `queued`, `done`.
    #[serde(default)]
    pub activity: String,
}

/// First non-blank line of `s`, cut to `max` chars with a trailing ellipsis.
pub fn first_line(s: &str, max: usize) -> String {
    let line = s.trim_start().lines().next().unwrap_or("");
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// `42s`, `3m04s`, `2h05m`.
pub fn fmt_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// One-line fleet summary, zero buckets left out:
/// `2 workers · 1 running (counting tests, 3m12s) · 4 done · 1 failed · 2 queued`.
/// The running clause is the most recently started running task's activity;
/// `idle` when nothing is planning, queued or running. Offline workers don't count.
pub fn headline(workers: &[WorkerSummary], tasks: &[TaskSummary], now_ms: u64) -> String {
    let mut parts = vec![match workers.iter().filter(|w| w.online).count() {
        0 => "no workers".to_string(),
        1 => "1 worker".to_string(),
        n => format!("{n} workers"),
    }];
    let count = |s| tasks.iter().filter(|t| t.status == s).count();
    let newest = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Running)
        .max_by_key(|t| t.started_at);
    if let Some(t) = newest {
        let what = t.activity.strip_prefix("status: ").unwrap_or(&t.activity);
        let since = t.started_at.map(|s| fmt_elapsed(now_ms.saturating_sub(s)));
        let detail: Vec<&str> = [Some(what), since.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();
        let detail = match detail.is_empty() {
            true => String::new(),
            false => format!(" ({})", detail.join(", ")),
        };
        parts.push(format!("{} running{detail}", count(TaskStatus::Running)));
    }
    let idle = [
        TaskStatus::Planning,
        TaskStatus::Queued,
        TaskStatus::Running,
    ]
    .iter()
    .all(|s| count(*s) == 0);
    if idle {
        parts.push("idle".into());
    }
    for (s, name) in [
        (TaskStatus::Done, "done"),
        (TaskStatus::Failed, "failed"),
        (TaskStatus::Planning, "planning"),
        (TaskStatus::Queued, "queued"),
    ] {
        let n = count(s);
        if n > 0 {
            parts.push(format!("{n} {name}"));
        }
    }
    parts.join(" · ")
}

/// Orchestrator -> client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Status {
        workers: Vec<WorkerSummary>,
        tasks: Vec<TaskSummary>,
        /// Orchestrator's Unix millis, so clients time running tasks on its
        /// clock rather than their own.
        #[serde(default)]
        now_ms: Option<u64>,
    },
    Submitted {
        task: TaskId,
    },
    /// Reply to `Show`. `truncated`: oldest events were dropped to bound memory.
    TaskDetail {
        task: TaskSummary,
        events: Vec<AgentEvent>,
        truncated: bool,
    },
    Event {
        task: TaskId,
        /// None for events from the orchestrator's planner.
        worker: Option<WorkerId>,
        event: AgentEvent,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_tagged_enums() {
        let msgs = vec![
            WorkerMsg::Hello {
                id: Uuid::nil(),
                running: vec![],
                info: WorkerInfo {
                    hostname: "h".into(),
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    harnesses: vec!["claude".into()],
                    roots: vec!["/code".into()],
                    models: [("claude".into(), vec!["sonnet".into()])].into(),
                },
            },
            WorkerMsg::Event {
                task: Uuid::nil(),
                event: AgentEvent::ToolUse {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            },
            WorkerMsg::Event {
                task: Uuid::nil(),
                event: AgentEvent::Status { text: "s".into() },
            },
            WorkerMsg::Stats(WorkerStats {
                disk_free_gb: 12.5,
                disk_used_pct: 38,
                cpu_pct: 64,
                mem_pct: 82,
            }),
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<WorkerMsg>(&s).unwrap(), m);
        }
        // Workers predating `roots` still parse.
        let old: WorkerInfo =
            serde_json::from_str(r#"{"hostname":"h","os":"o","arch":"a","harnesses":[]}"#).unwrap();
        assert!(old.roots.is_empty() && old.models.is_empty());
        // Tag sits beside the stats fields; missing fields default.
        let s: WorkerMsg = serde_json::from_str(r#"{"type":"stats","cpu_pct":7}"#).unwrap();
        assert_eq!(
            s,
            WorkerMsg::Stats(WorkerStats {
                cpu_pct: 7,
                ..Default::default()
            })
        );
        let s = serde_json::to_string(&ClientMsg::Status).unwrap();
        assert_eq!(s, r#"{"type":"status"}"#);
        // Summaries from orchestrators predating timing/tokens/activity still parse.
        let old: TaskSummary = serde_json::from_str(
            r#"{"id":"00000000-0000-0000-0000-000000000000","prompt":"p","status":"done",
                "worker":null,"harness":"claude","session":null,"summary":null}"#,
        )
        .unwrap();
        assert_eq!((old.started_at, old.tokens_in), (None, None));
        assert!(old.activity.is_empty());
    }

    fn task(status: TaskStatus, started_at: u64, activity: &str) -> TaskSummary {
        TaskSummary {
            id: Uuid::new_v4(),
            prompt: "p".into(),
            status,
            worker: None,
            harness: "claude".into(),
            model: None,
            session: None,
            summary: None,
            started_at: Some(started_at),
            finished_at: None,
            tokens_in: None,
            tokens_out: None,
            activity: activity.into(),
        }
    }

    fn worker() -> WorkerSummary {
        WorkerSummary {
            id: Uuid::new_v4(),
            info: WorkerInfo {
                hostname: "h".into(),
                os: "o".into(),
                arch: "a".into(),
                harnesses: vec![],
                roots: vec![],
                models: Default::default(),
            },
            busy: false,
            online: true,
            stats: None,
            stats_at: None,
        }
    }

    #[test]
    fn headline_counts_buckets_and_names_newest_running() {
        use TaskStatus::*;
        assert_eq!(headline(&[], &[], 0), "no workers · idle");
        let w = [worker(), worker()];
        let mut tasks = vec![
            task(Done, 0, "done"),
            task(Done, 0, "done"),
            task(Failed, 0, "FAILED"),
        ];
        assert_eq!(
            headline(&w[..1], &tasks, 0),
            "1 worker · idle · 2 done · 1 failed"
        );
        tasks.push(task(Running, 1_000, "> Bash ls"));
        tasks.push(task(Running, 8_000, "status: counting tests"));
        tasks.push(task(Queued, 9_000, "queued"));
        tasks.push(task(Planning, 9_000, "planning…"));
        assert_eq!(
            headline(&w, &tasks, 200_000),
            "2 workers · 2 running (counting tests, 3m12s) · 2 done · 1 failed · 1 planning · 1 queued"
        );
        // Queued with only an offline worker: not idle.
        let mut off = worker();
        off.online = false;
        assert_eq!(headline(&[off], &tasks[5..6], 0), "no workers · 1 queued");
        // Running with nothing known about it.
        let mut bare = task(Running, 0, "");
        bare.started_at = None;
        assert_eq!(headline(&w[..1], &[bare], 0), "1 worker · 1 running");
        assert_eq!(fmt_elapsed(7_530_000), "2h05m");
    }

    #[test]
    fn first_line_truncates() {
        assert_eq!(first_line("ab\ncd", 80), "ab");
        assert_eq!(first_line("\n  \nab", 80), "ab");
        assert_eq!(first_line("abcdef", 4), "abc…");
    }
}
