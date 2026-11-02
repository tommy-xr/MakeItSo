//! Questions the orchestrator asks one worker on demand (`OrchMsg::Query`), and
//! their answers (`WorkerMsg::Answer`): Claude and Codex sessions on its disk.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Query {
    /// The newest sessions, answered with `Answer::Sessions`; headless ones only if asked.
    ListSessions {
        #[serde(default)]
        headless: bool,
    },
    /// The last `limit` turns of one session, answered with `Answer::Transcript`
    /// or `Answer::NotFound`.
    ReadSession {
        harness: String,
        id: Uuid,
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Sessions {
        sessions: Vec<Session>,
    },
    Transcript {
        /// Oldest first.
        turns: Vec<Turn>,
        /// Older turns exist than those sent.
        more: bool,
        /// As in `Session`, so a viewer knows whether to keep refreshing.
        updated_ms: u64,
        active: bool,
        #[serde(default)]
        open: bool,
    },
    NotFound,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// A tool call (`Bash: ls`) or its result (`→ first line`), one line each.
    Tool,
}

/// Most turns `Query::ReadSession` returns.
pub const MAX_TURNS: usize = 200;

/// One entry of a session's transcript, clipped for a phone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// When it was written, as the harness recorded it (RFC 3339).
    pub at: Option<String>,
}

/// A Claude or Codex session found in the harness's own transcript files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Session {
    /// "claude" or "codex".
    pub harness: String,
    /// What `claude --resume` / `codex exec resume` take.
    pub id: String,
    pub cwd: Option<String>,
    /// Its title if it has one, else the first thing the user asked; one line.
    pub title: Option<String>,
    pub model: Option<String>,
    /// Unix millis of its last write.
    pub updated_ms: u64,
    /// Written in the last couple of minutes: likely still running.
    pub active: bool,
    /// Started from a terminal UI, not headless (`claude -p`, `codex exec`).
    pub interactive: bool,
    /// In use right now, so it mustn't be resumed in place. Claude: a live
    /// process holds it. Codex: no such signal; same as `active`.
    pub open: bool,
}

/// A session on a worker's disk that a task messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendTo {
    /// Its harness is the task's.
    pub session: Uuid,
    /// As listed, for the task's heading.
    #[serde(default)]
    pub title: Option<String>,
    pub mode: SendMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Continue the session itself; refused while it is open.
    InPlace,
    /// Continue a copy (claude `--fork-session`, `codex exec fork`); the original is
    /// untouched. No longer offered; kept so older tasks still load.
    Fork,
    /// Wait on the worker until it closes, then continue it in place.
    WhenClosed,
    /// Into the session as it is: one open in a claude terminal gets it as a cross-session
    /// message (`AgentEvent::Delivered`); a closed one is continued in place; an open codex
    /// one once it goes quiet. Only to workers with `WorkerInfo::deliver`.
    Deliver,
}

/// The peer name a delivered message comes from, so its session's transcript tells
/// MIS's messages from other sessions'.
pub const PEER_NAME: &str = "mis-phone";

/// What a live session is doing, as its files show.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    /// Mid-turn and writing.
    Working,
    /// Its agent waits on the user: a permission prompt or dialog (claude says so), or
    /// it replied and its terminal is idle.
    Waiting,
    /// Mid-turn, but nothing written for `STALL_MS`.
    Stalled,
    #[default]
    Idle,
    /// Claude's terminal is in shell mode (`!`).
    Shell,
}

/// No write for this long mid-turn: stalled.
pub const STALL_MS: u64 = 10 * 60 * 1000;
/// Sessions written this recently are live even when not open.
pub const LIVE_WINDOW_MS: u64 = 6 * 3600 * 1000;
/// Most live sessions a worker reports.
pub const MAX_LIVE: usize = 30;

/// An interactive session open now or written within `LIVE_WINDOW_MS`, as its worker
/// reports it (`WorkerMsg::LiveSessions`). Headless runs (MIS's own tasks) aren't live.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LiveSession {
    /// "claude" or "codex".
    pub harness: String,
    pub id: String,
    /// As `Session::title`.
    pub title: Option<String>,
    /// Claude's peer name for it (`functor2-fd`), from its process record.
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// A live claude process holds it. Codex records no such thing: always false.
    pub open: bool,
    pub state: LiveState,
    /// What it waits on, in claude's words (`permission prompt`, `dialog open`).
    pub waiting_for: Option<String>,
    /// Unix millis of its latest write (claude: its subagents' too).
    pub updated_ms: u64,
    /// First line of its latest assistant text.
    pub last: Option<String>,
    /// Messages MIS delivered into its conversation (`PEER_NAME`), oldest first.
    pub delivered: Vec<Delivered>,
}

/// A message delivered into a live claude session, as its transcript shows it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Delivered {
    /// As `AgentEvent::Delivered`.
    pub msg_id: String,
    /// First line of the session's answer: its next reply, or what it sent back.
    pub reply: Option<String>,
}

/// Most `LiveSession::delivered` a worker reports per session.
pub const MAX_DELIVERED: usize = 8;

/// A worker's live session as `GET /api/sessions` lists it, with the MIS tasks that messaged it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRow {
    pub worker: Uuid,
    /// The worker's hostname.
    pub worker_name: String,
    /// Offline: the sessions it last reported.
    pub online: bool,
    #[serde(flatten)]
    pub session: LiveSession,
    /// Oldest first.
    #[serde(default)]
    pub messages: Vec<SessionMessage>,
}

/// A task that messaged a session (`Task::send_to`), shown inside its session's row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMessage {
    pub task: Uuid,
    pub status: crate::TaskStatus,
    /// First line of what was sent.
    pub text: String,
    /// The session replied: the task finished done.
    pub answered: bool,
    /// Delivered, but its terminal holds it for the user's approval.
    #[serde(default)]
    pub held: bool,
    /// Unix millis it was sent.
    pub at: u64,
}
