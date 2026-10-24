//! Wire types shared by orchestrator, workers, and clients.
//! Everything is JSON over WebSocket; `type` tags on the enums below.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    Started { session: Option<String> },
    Text { text: String },
    ToolUse { name: String, input: serde_json::Value },
    ToolResult { content: String },
    Finished { ok: bool, summary: Option<String> },
}

/// Worker -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    Hello { info: WorkerInfo },
    Event { task: TaskId, event: AgentEvent },
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
    Submit { prompt: String, cwd: Option<String>, harness: Option<String> },
    /// Stream all task events from now on.
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerSummary {
    pub id: WorkerId,
    pub info: WorkerInfo,
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub id: TaskId,
    pub prompt: String,
    pub status: TaskStatus,
    pub worker: Option<WorkerId>,
}

/// Orchestrator -> client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Status { workers: Vec<WorkerSummary>, tasks: Vec<TaskSummary> },
    Submitted { task: TaskId },
    Event { task: TaskId, worker: WorkerId, event: AgentEvent },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_tagged_enums() {
        let msgs = vec![
            WorkerMsg::Hello {
                info: WorkerInfo {
                    hostname: "h".into(),
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    harnesses: vec!["claude".into()],
                },
            },
            WorkerMsg::Event {
                task: Uuid::nil(),
                event: AgentEvent::ToolUse { name: "Bash".into(), input: serde_json::json!({"command": "ls"}) },
            },
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<WorkerMsg>(&s).unwrap(), m);
        }
        let s = serde_json::to_string(&ClientMsg::Status).unwrap();
        assert_eq!(s, r#"{"type":"status"}"#);
    }
}
