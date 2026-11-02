//! Harness abstraction: run a prompt, stream normalized `AgentEvent`s back.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use mis_types::{AgentEvent, ImageFile, ImageKind, LimitWindow, Permissions, TaskId};
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
/// Tool on that server claude asks, in ask mode, about a call the rules don't allow.
pub const APPROVE_TOOL: &str = "approve";
/// How long the worker waits for the user to answer before denying.
pub const APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

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
    /// Continue a copy of `resume` rather than the session itself. None: the
    /// harness's default for replies (claude forks, codex continues).
    pub fork: Option<bool>,
    /// Absolute paths of images attached to the prompt.
    pub images: &'a [String],
    /// What tools may do unasked; must pass `check`. None leaves the harness's
    /// headless default, which denies writes, shell and MCP.
    pub permissions: Option<&'a Permissions>,
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

/// Decode `images` into `dir` (created, owner-only) as `<id>.<ext>`; their
/// absolute paths, in order. Err on bytes that aren't base64 of an accepted image.
pub fn write_images(dir: &Path, images: &[ImageFile]) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    use base64ct::{Base64, Encoding};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let dir = std::path::absolute(dir)?;
    images
        .iter()
        .map(|img| {
            let bytes = Base64::decode_vec(&img.data)
                .map_err(|e| anyhow::anyhow!("image {}: {e}", img.id))?;
            let kind = ImageKind::sniff(&bytes)
                .with_context(|| format!("image {} is not jpeg, png, webp or gif", img.id))?;
            let path = dir.join(format!("{}.{}", img.id, kind.ext()));
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            Ok(path.display().to_string())
        })
        .collect()
}

/// Agents whose CLI is on PATH.
pub fn available() -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> = Vec::new();
    if which("claude") {
        v.push(Box::new(claude::ClaudeAgent::default()));
    }
    if which("codex") {
        v.push(Box::new(codex::CodexAgent));
    }
    v
}

/// Err naming the first entry that can't become a permission rule: folders
/// must be absolute and not `/`; MCP server names are `[A-Za-z0-9_-]+` (they
/// become `mcp__<name>__*` and TOML keys); commands are plain words, e.g.
/// "python3" or "git log".
pub fn check(p: &Permissions) -> Result<(), String> {
    for f in &p.folders {
        let path = Path::new(f);
        if !path.is_absolute() || path.parent().is_none() {
            return Err(format!("folder {f:?}: must be an absolute path below /"));
        }
    }
    let word = |c: char| c.is_ascii_alphanumeric() || "-_".contains(c);
    for s in &p.mcp {
        if s.is_empty() || !s.chars().all(word) {
            return Err(format!("mcp server {s:?}: use letters, digits, - and _"));
        }
    }
    for c in &p.commands {
        let plain = |ch: char| word(ch) || "./:=@+ ".contains(ch);
        if c.trim() != c
            || c.is_empty()
            || c.starts_with('-')
            || c.contains("  ")
            || !c.chars().all(plain)
        {
            return Err(format!(
                "command {c:?}: use words like \"python3\" or \"git log\""
            ));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64ct::{Base64, Encoding};

    #[test]
    fn check_refuses_entries_that_widen_or_inject_rules() {
        let ok = Permissions {
            // Absolute on this platform: `/Users/…` is relative on Windows.
            folders: vec![if cfg!(windows) {
                r"C:\Users\me\assets"
            } else {
                "/Users/me/assets"
            }
            .into()],
            mcp: vec!["blender".into(), "my-server_2".into()],
            commands: vec!["python3".into(), "git log".into(), "./make.sh".into()],
            ..Default::default()
        };
        assert_eq!(check(&ok), Ok(()));
        let bad = |p: Permissions| check(&p).unwrap_err();
        for f in ["/", "assets", "~/assets", ""] {
            bad(Permissions {
                folders: vec![f.into()],
                ..Default::default()
            });
        }
        for s in ["", "*", "a__*", "blender)", "a.b", "a b"] {
            bad(Permissions {
                mcp: vec![s.into()],
                ..Default::default()
            });
        }
        for c in [
            "",
            "*",
            "git *",
            "rm -rf /;",
            "python3 (x)",
            " python3",
            "-x",
            "a,b",
            "a  b",
        ] {
            bad(Permissions {
                commands: vec![c.into()],
                ..Default::default()
            });
        }
    }

    #[test]
    fn writes_images_by_sniffed_kind_and_refuses_others() {
        let dir = std::env::temp_dir().join(format!("mis-agent-test-{}", uuid::Uuid::new_v4()));
        let png = b"\x89PNG\r\n\x1a\n rest".to_vec();
        let ok = ImageFile {
            id: uuid::Uuid::new_v4(),
            data: Base64::encode_string(&png),
        };
        let paths = write_images(&dir, std::slice::from_ref(&ok)).unwrap();
        assert_eq!(
            paths,
            [dir.join(format!("{}.png", ok.id)).display().to_string()]
        );
        assert_eq!(std::fs::read(&paths[0]).unwrap(), png);
        let html = ImageFile {
            data: Base64::encode_string(b"<html>"),
            ..ok.clone()
        };
        assert!(write_images(&dir, &[html]).is_err());
        let garbage = ImageFile {
            data: "not base64!".into(),
            ..ok
        };
        assert!(write_images(&dir, &[garbage]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
