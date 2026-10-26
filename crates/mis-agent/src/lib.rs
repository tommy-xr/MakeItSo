//! Harness abstraction: run a prompt, stream normalized `AgentEvent`s back.

use std::future::Future;
use std::pin::Pin;

use mis_types::{AgentEvent, TaskId};
use tokio::sync::mpsc;

pub mod claude;
pub mod codex;

pub type BoxFut<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

pub trait Agent: Send + Sync {
    /// Harness name as advertised to the orchestrator, e.g. "claude".
    fn name(&self) -> &'static str;
    /// Run to completion. Must end with exactly one `AgentEvent::Finished`.
    /// The agent process sees `MIS_TASK=<task>` so nested `mis task` calls can be refused.
    /// `model` None leaves the harness's default.
    fn run<'a>(
        &'a self,
        task: TaskId,
        prompt: &'a str,
        cwd: Option<&'a str>,
        model: Option<&'a str>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a>;
}

/// Every harness this crate can run.
pub const HARNESSES: [&str; 2] = ["claude", "codex"];

/// Models the planner may pick for `harness`, cheapest first. Codex model ids
/// change too often to pin here, so codex always runs its configured default.
pub fn models(harness: &str) -> &'static [&'static str] {
    match harness {
        "claude" => &["haiku", "sonnet", "opus"],
        _ => &[],
    }
}

/// `Agent::run` that always ends with a Finished: an Err or a panic inside the
/// agent becomes `Finished { ok: false }` instead of a task stuck running.
pub async fn run_guarded(
    agent: &dyn Agent,
    task: TaskId,
    prompt: &str,
    cwd: Option<&str>,
    model: Option<&str>,
    tx: mpsc::Sender<AgentEvent>,
) {
    let run = agent.run(task, prompt, cwd, model, tx.clone());
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

pub fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}
