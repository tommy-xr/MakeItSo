//! Script routines: the prompt is a shell script, run as is with no agent. The
//! output becomes one `Text` block (fenced), its exit status the `Finished`.

use anyhow::Context;
use mis_types::{AgentEvent, TaskId, SCRIPT_HARNESS, TASK_ENV};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Agent, BoxFut, RunOpts};

/// Output kept for the transcript: the last this many bytes.
const KEEP_BYTES: usize = 64 * 1024;

/// Runs scripts. Not in [`crate::available`]: every worker runs it, and only routines start it.
pub struct Script;

impl Agent for Script {
    fn name(&self) -> &'static str {
        SCRIPT_HARNESS
    }

    fn models(&self) -> Vec<String> {
        vec![]
    }

    /// Writes the script to a temp file and runs it in `opts.cwd`: directly if it
    /// starts with `#!` (unix), else with `sh`. Only `cwd` and `env` of `opts` apply.
    fn run<'a>(
        &'a self,
        task: TaskId,
        script: &'a str,
        opts: RunOpts<'a>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let path = std::env::temp_dir().join(format!("mis-script-{task}"));
            std::fs::write(&path, script).context("writing the script")?;
            let _cleanup = Remove(path.clone());
            let shebang = cfg!(unix) && script.starts_with("#!");
            let mut cmd = match shebang {
                true => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = std::fs::Permissions::from_mode(0o700);
                        std::fs::set_permissions(&path, mode).context("chmod the script")?;
                    }
                    Command::new(&path)
                }
                false => {
                    let mut c = Command::new("sh");
                    c.arg(&path);
                    c
                }
            };
            if let Some(d) = opts.cwd {
                cmd.current_dir(d);
            }
            cmd.env(TASK_ENV, task.to_string())
                .envs(opts.env.iter().copied())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let mut child = cmd.spawn().context("starting the script")?;
            // Declared after `child`, so it drops first, while the tree still hangs off it.
            let mut tree = crate::Tree::new(child.id());
            let _ = tx
                .send(AgentEvent::Started {
                    session: None,
                    model: None,
                })
                .await;
            // stdout and stderr interleaved by line, as a terminal shows them.
            let mut out = BufReader::new(child.stdout.take().unwrap()).split(b'\n');
            let mut err = BufReader::new(child.stderr.take().unwrap()).split(b'\n');
            let (mut out_open, mut err_open) = (true, true);
            let mut kept = Tail::default();
            while out_open || err_open {
                let line = tokio::select! {
                    l = out.next_segment(), if out_open => l?.or_else(|| { out_open = false; None }),
                    l = err.next_segment(), if err_open => l?.or_else(|| { err_open = false; None }),
                };
                if let Some(l) = line {
                    kept.push(&String::from_utf8_lossy(&l));
                }
            }
            let status = child.wait().await;
            tree.disarm();
            let status = status.context("waiting for the script")?;
            for ev in finish(&kept, status.code()) {
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    }
}

/// Deletes the script file when the run ends, however it ends.
struct Remove(std::path::PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The last [`KEEP_BYTES`] of output, whole lines, and whether earlier ones were cut.
#[derive(Default)]
struct Tail {
    text: String,
    cut: bool,
}

impl Tail {
    fn push(&mut self, line: &str) {
        self.text.push_str(line.trim_end_matches('\r'));
        self.text.push('\n');
        if self.text.len() > KEEP_BYTES {
            let over = self.text.len() - KEEP_BYTES;
            // Drop whole lines from the front, at least `over` bytes' worth.
            let at = (self.text[over..].find('\n')).map_or(self.text.len(), |i| over + i + 1);
            self.text.drain(..at);
            self.cut = true;
        }
    }
}

/// Events ending a run: the output fenced as one `Text` (none if it printed nothing),
/// then `Finished`, ok on exit 0, its summary the last line printed or the exit status.
/// `code` None: killed by a signal.
fn finish(out: &Tail, code: Option<i32>) -> Vec<AgentEvent> {
    let mut evs = vec![];
    let body = out.text.trim_end();
    if !body.is_empty() {
        // A line opening a fence would close ours; a zero-width space keeps it text.
        let lines: Vec<String> = (body.lines())
            .map(|l| match l.trim_start().starts_with("```") {
                true => format!("\u{200b}{l}"),
                false => l.to_string(),
            })
            .collect();
        let cut = if out.cut {
            "(earlier output cut)\n"
        } else {
            ""
        };
        evs.push(AgentEvent::Text {
            text: format!("{cut}```\n{}\n```", lines.join("\n")),
        });
    }
    let last = body.lines().rev().map(str::trim).find(|l| !l.is_empty());
    let status = match code {
        Some(0) => None,
        Some(c) => Some(format!("exit {c}")),
        None => Some("killed".to_string()),
    };
    let summary = match (status, last) {
        (None, Some(l)) => l.to_string(),
        (None, None) => "done".to_string(),
        (Some(s), Some(l)) => format!("{s}: {l}"),
        (Some(s), None) => s,
    };
    evs.push(AgentEvent::Finished {
        ok: code == Some(0),
        summary: Some(summary),
    });
    evs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(lines: &[&str]) -> Tail {
        let mut t = Tail::default();
        lines.iter().for_each(|l| t.push(l));
        t
    }

    #[test]
    fn output_is_fenced_and_the_last_line_sums_it_up() {
        let evs = finish(&tail(&["cloned a", "```", "fetched b\r", ""]), Some(0));
        assert_eq!(
            evs,
            vec![
                AgentEvent::Text {
                    text: "```\ncloned a\n\u{200b}```\nfetched b\n```".into()
                },
                AgentEvent::Finished {
                    ok: true,
                    summary: Some("fetched b".into())
                },
            ]
        );
    }

    #[test]
    fn failures_say_their_exit_and_silence_adds_no_text() {
        assert_eq!(
            finish(&tail(&["fatal: no repo"]), Some(128))[1],
            AgentEvent::Finished {
                ok: false,
                summary: Some("exit 128: fatal: no repo".into())
            }
        );
        assert_eq!(
            finish(&tail(&[]), None),
            vec![AgentEvent::Finished {
                ok: false,
                summary: Some("killed".into())
            }]
        );
        assert_eq!(
            finish(&tail(&[]), Some(0))[0],
            AgentEvent::Finished {
                ok: true,
                summary: Some("done".into())
            }
        );
    }

    #[test]
    fn long_output_keeps_whole_lines_from_the_end() {
        let line = "x".repeat(1000);
        let t = tail(&vec![line.as_str(); 100]);
        assert!(t.cut && t.text.len() <= KEEP_BYTES);
        assert!(t.text.lines().all(|l| l == line));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_its_folder_merging_stderr_and_failing_on_exit() {
        let dir = std::env::temp_dir().join(format!("mis-script-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_str().unwrap();
        let run = |script: &'static str| async move {
            let (tx, mut rx) = mpsc::channel(16);
            let opts = RunOpts {
                cwd: Some(cwd),
                ..Default::default()
            };
            Script
                .run(uuid::Uuid::nil(), script, opts, tx)
                .await
                .unwrap();
            let mut evs = vec![];
            while let Some(e) = rx.recv().await {
                evs.push(e);
            }
            evs
        };
        let evs = run("touch made\necho out\necho err >&2\nexit 3\n").await;
        assert!(dir.join("made").exists());
        let AgentEvent::Text { text } = &evs[1] else {
            panic!("{evs:?}")
        };
        assert!(text.contains("out") && text.contains("err"), "{text}");
        assert!(
            matches!(&evs[2], AgentEvent::Finished { ok: false, summary: Some(s) } if s.starts_with("exit 3"))
        );
        // A shebang picks its interpreter.
        let evs = run("#!/bin/sh\necho \"$MIS_TASK\"\n").await;
        assert!(
            matches!(&evs[2], AgentEvent::Finished { ok: true, summary: Some(s) } if s == &uuid::Uuid::nil().to_string())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
