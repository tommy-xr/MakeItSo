//! Harness abstraction: run a prompt, stream normalized `AgentEvent`s back.

use std::future::Future;
use std::pin::Pin;

use mis_types::AgentEvent;
use tokio::sync::mpsc;

pub mod claude;
pub mod codex;

pub type BoxFut<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

pub trait Agent: Send + Sync {
    /// Harness name as advertised to the orchestrator, e.g. "claude".
    fn name(&self) -> &'static str;
    /// Run to completion. Must end with exactly one `AgentEvent::Finished`.
    fn run<'a>(&'a self, prompt: &'a str, cwd: Option<&'a str>, tx: mpsc::Sender<AgentEvent>) -> BoxFut<'a>;
}

/// Agents whose CLI is on PATH.
pub fn available(skip_permissions: bool) -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> = Vec::new();
    if which("claude") {
        v.push(Box::new(claude::ClaudeAgent { skip_permissions }));
    }
    if which("codex") {
        v.push(Box::new(codex::CodexAgent { skip_permissions }));
    }
    v
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}
