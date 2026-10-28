//! Harness abstraction: run a prompt, stream normalized `AgentEvent`s back.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use mis_types::{AgentEvent, LimitWindow, TaskId};
use tokio::sync::mpsc;

pub mod claude;
pub mod codex;

pub type BoxFut<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Env var carrying the bearer token for the worker's MCP server, so the
/// secret stays out of the agent's argv (visible to `ps`). The agent's own
/// commands inherit it; acceptable, as it only works on loopback for this task.
pub const MCP_TOKEN_ENV: &str = "MIS_MCP_TOKEN";
/// Name the worker's MCP server is registered under in each harness.
pub const MCP_SERVER: &str = "mis";

/// Loopback MCP server the agent may call for the duration of one run.
#[derive(Debug, Clone)]
pub struct Mcp {
    /// e.g. `http://127.0.0.1:53817/mcp`.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>`; passed via `MCP_TOKEN_ENV`.
    pub token: String,
}

/// How to run one prompt; `Default` is the harness's defaults, nothing extra.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts<'a> {
    /// Working directory; None = this process's.
    pub cwd: Option<&'a str>,
    /// None leaves the harness's default model.
    pub model: Option<&'a str>,
    /// Offered to the agent as server `MCP_SERVER`.
    pub mcp: Option<&'a Mcp>,
    /// Session to continue (claude `--resume`, codex `exec resume`).
    pub resume: Option<&'a str>,
}

pub trait Agent: Send + Sync {
    /// Harness name as advertised to the orchestrator, e.g. "claude".
    fn name(&self) -> &'static str;
    /// Models the planner may pick for this harness, read from the harness's own
    /// files. Never fails: falls back (with a warning) when they can't be read.
    fn models(&self) -> Vec<String>;
    /// Usage-limit windows read from the harness's own files, polled with each
    /// stats beat; None when it keeps none (claude reports them during runs).
    fn limits(&self) -> Option<Vec<LimitWindow>> {
        None
    }
    /// Run to completion. Must end with exactly one `AgentEvent::Finished`.
    /// The agent process sees `MIS_TASK=<task>` so nested `mis task` calls can be refused.
    fn run<'a>(
        &'a self,
        task: TaskId,
        prompt: &'a str,
        opts: RunOpts<'a>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a>;
}

/// Every harness this crate can run.
pub const HARNESSES: [&str; 2] = ["claude", "codex"];

/// `Agent::run` that always ends with a Finished: an Err or a panic inside the
/// agent becomes `Finished { ok: false }` instead of a task stuck running.
pub async fn run_guarded(
    agent: &dyn Agent,
    task: TaskId,
    prompt: &str,
    opts: RunOpts<'_>,
    tx: mpsc::Sender<AgentEvent>,
) {
    let run = agent.run(task, prompt, opts, tx.clone());
    let res = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(run)).await;
    let why = match res {
        Ok(Ok(())) => return,
        Ok(Err(e)) => {
            tracing::error!(%task, "agent failed: {e:#}");
            e.to_string()
        }
        Err(_) => {
            tracing::error!(%task, "agent panicked");
            "agent panicked".into()
        }
    };
    let _ = tx
        .send(AgentEvent::Finished {
            ok: false,
            summary: Some(why),
        })
        .await;
}

/// Agents whose CLI is on PATH.
pub fn available(skip_permissions: bool) -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> = Vec::new();
    if which("claude") {
        v.push(Box::new(claude::ClaudeAgent {
            skip_permissions,
            extra_args: vec![],
        }));
    }
    if which("codex") {
        v.push(Box::new(codex::CodexAgent { skip_permissions }));
    }
    v
}

fn home() -> Result<PathBuf, String> {
    std::env::home_dir().ok_or_else(|| "no home directory".into())
}

/// Read `path` and `parse` it; either error names the file.
fn parse_file<T>(path: &Path, parse: impl FnOnce(&str) -> Result<T, String>) -> Result<T, String> {
    std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| parse(&s))
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}
