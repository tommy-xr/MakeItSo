//! Wire types shared by orchestrator, workers, and clients.
//! Everything is JSON over WebSocket; `type` tags on the enums below.

use serde::{Deserialize, Serialize};

/// Env var set on agent processes: the id of the task they are running.
pub const TASK_ENV: &str = "MIS_TASK";
use uuid::Uuid;

mod access;
pub use access::access_headers;

pub type WorkerId = Uuid;
pub type TaskId = Uuid;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: TaskId,
    pub prompt: String,
    /// Working directory on the worker machine; None = worker default.
    pub cwd: Option<String>,
    pub harness: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
}

/// Harness-agnostic view of what an agent is doing. Visualizers build on this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Started {
        session: Option<String>,
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
    Submit {
        prompt: String,
        cwd: Option<String>,
        harness: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub id: TaskId,
    pub prompt: String,
    pub status: TaskStatus,
    pub worker: Option<WorkerId>,
    pub harness: String,
    /// Agent session id, from the `Started` event.
    pub session: Option<String>,
    /// From the `Finished` event.
    pub summary: Option<String>,
}

/// Orchestrator -> client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Status {
        workers: Vec<WorkerSummary>,
        tasks: Vec<TaskSummary>,
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
        worker: WorkerId,
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
                },
            },
            WorkerMsg::Event {
                task: Uuid::nil(),
                event: AgentEvent::ToolUse {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            },
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<WorkerMsg>(&s).unwrap(), m);
        }
        // Workers predating `roots` still parse.
        let old: WorkerInfo =
            serde_json::from_str(r#"{"hostname":"h","os":"o","arch":"a","harnesses":[]}"#).unwrap();
        assert!(old.roots.is_empty());
        let s = serde_json::to_string(&ClientMsg::Status).unwrap();
        assert_eq!(s, r#"{"type":"status"}"#);
    }
}
