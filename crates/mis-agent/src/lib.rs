//! Harness abstraction: run a prompt, stream normalized `AgentEvent`s back.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use mis_types::{AgentEvent, AttachedFile, ImageFile, ImageKind, LimitWindow, Permissions, TaskId};
use tokio::sync::mpsc;

pub mod claude;
pub mod codex;
pub mod deliver;

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
/// How long a harness lets one call to the worker's MCP server take: `APPROVAL_WAIT`
/// plus `merge_pr`'s `gh` calls around it.
pub const MCP_CALL_WAIT: std::time::Duration = std::time::Duration::from_secs(35 * 60);

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
    /// Narrow `permissions` to reads (see `claude::read_only_args`). Codex ignores
    /// it: its read-only sandbox also cuts the network `gh` needs.
    pub read_only: bool,
    /// Deny the usual ways around `merge_pr` (see `claude::NO_MERGE`): a live
    /// routine run. Codex ignores it, as it has no deny rules; its prompt says so.
    pub no_merge: bool,
    /// Extra environment for the agent process, e.g. an app task's `MIS_TOKEN`.
    pub env: &'a [(&'a str, &'a str)],
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

/// Kills a process and everything it started when dropped, unless disarmed. Held over
/// a running agent so a cancelled (dropped) run takes its tools' processes with it:
/// claude starts each Bash command in a process group of its own, so killing the
/// agent's group would miss them.
pub(crate) struct Tree(Option<u32>);

impl Tree {
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    /// The process exited and was reaped: its pid may be reused, so leave it be.
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_tree(pid);
        }
    }
}

/// SIGKILL (on Windows, terminate) `pid` and its descendants, found by parent pid.
/// Best effort: a process forked after the snapshot survives.
fn kill_tree(pid: u32) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut tree = vec![Pid::from_u32(pid)];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        let born = sys.process(parent).map_or(0, |p| p.start_time());
        // Started after its parent: Windows keeps a dead parent's pid on its children,
        // so an older process naming a reused pid as parent isn't a descendant.
        let kids = (sys.processes().iter())
            .filter(|(_, p)| p.parent() == Some(parent) && p.start_time() >= born);
        tree.extend(kids.map(|(pid, _)| *pid));
        i += 1;
    }
    for pid in tree {
        if let Some(p) = sys.process(pid) {
            p.kill();
        }
    }
}

/// Create `dir`, owner-only; its absolute path.
fn owner_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(std::path::absolute(dir)?)
}

/// Decode `images` into `dir` (created, owner-only) as `<id>.<ext>`; their
/// absolute paths, in order. Err on bytes that aren't base64 of an accepted image.
pub fn write_images(dir: &Path, images: &[ImageFile]) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    use base64ct::{Base64, Encoding};
    let dir = owner_dir(dir)?;
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

/// Decode `files` into `dir` (created, owner-only) as `<id>-<name>`; their absolute paths, in
/// order. Err on a name that isn't one plain file name, or data that isn't base64.
pub fn write_files(dir: &Path, files: &[AttachedFile]) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    use base64ct::{Base64, Encoding};
    let dir = owner_dir(dir)?;
    files
        .iter()
        .map(|f| {
            let plain = Path::new(&f.name).file_name() == Some(f.name.as_ref());
            anyhow::ensure!(plain && !f.name.starts_with('.'), "file {}: bad name", f.id);
            let bytes =
                Base64::decode_vec(&f.data).map_err(|e| anyhow::anyhow!("file {}: {e}", f.id))?;
            let path = dir.join(format!("{}-{}", f.id, f.name));
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            Ok(path.display().to_string())
        })
        .collect()
}

/// `prompt`, naming the attached files at `paths` for the agent to read.
pub fn with_files(prompt: &str, paths: &[String]) -> String {
    match paths {
        [] => prompt.to_string(),
        // One per line: names may hold commas and spaces.
        _ => format!(
            "{prompt}\n\nAttached files (read them):\n{}",
            paths.join("\n")
        ),
    }
}

/// Agents whose CLI is on PATH.
pub fn available() -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> = Vec::new();
    if which("claude") {
        v.push(Box::new(claude::ClaudeAgent::default()));
    }
    if which("codex") {
        let plan_tool = codex::probe_plan_tool();
        v.push(Box::new(codex::CodexAgent { plan_tool }));
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
    find(bin).is_some()
}

/// `bin`'s path on `PATH`, as found (symlinks not followed).
pub fn find(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64ct::{Base64, Encoding};

    /// Whether `pid` still runs (a zombie waiting to be reaped doesn't count).
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
        let mut sys = System::new();
        let pid = Pid::from_u32(pid);
        let kind = ProcessRefreshKind::nothing();
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
        sys.process(pid)
            .is_some_and(|p| p.status() != ProcessStatus::Zombie)
    }

    #[cfg(unix)]
    #[test]
    fn dropped_tree_kills_grandchildren_in_their_own_process_group() {
        use std::io::BufRead;
        // `set -m`: the background sleep gets a process group of its own, like claude's Bash calls.
        let mut sh = std::process::Command::new("sh")
            .args(["-c", "set -m; sleep 37 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(sh.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let sleep: u32 = line.trim().parse().unwrap();
        assert!(alive(sleep));
        let mut kept = Tree::new(Some(sh.id()));
        kept.disarm();
        drop(kept);
        assert!(alive(sleep), "a disarmed tree is left alone");
        drop(Tree::new(Some(sh.id())));
        sh.wait().unwrap();
        let gone = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            !alive(sleep)
        });
        assert!(gone, "sleep {sleep} outlived its tree");
    }

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

    #[test]
    fn writes_files_under_their_names_and_the_prompt_lists_them() {
        let dir = std::env::temp_dir().join(format!("mis-agent-test-{}", uuid::Uuid::new_v4()));
        let ok = AttachedFile {
            id: uuid::Uuid::new_v4(),
            name: "Chore list.pdf".into(),
            data: Base64::encode_string(b"%PDF-1.4"),
        };
        let paths = write_files(&dir, std::slice::from_ref(&ok)).unwrap();
        let want = dir.join(format!("{}-Chore list.pdf", ok.id));
        assert_eq!(paths, [want.display().to_string()]);
        assert_eq!(std::fs::read(&want).unwrap(), b"%PDF-1.4");
        for name in ["../x.pdf", "a/b.md", ".env", ""] {
            let bad = AttachedFile {
                name: name.into(),
                ..ok.clone()
            };
            assert!(write_files(&dir, &[bad]).is_err(), "{name}");
        }
        assert_eq!(with_files("p", &[]), "p");
        assert_eq!(
            with_files("p", &["/t/a.pdf".into(), "/t/b.md".into()]),
            "p\n\nAttached files (read them):\n/t/a.pdf\n/t/b.md"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
