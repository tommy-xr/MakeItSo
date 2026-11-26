//! Script routines: the prompt is a shell script, run as is with no agent. The
//! output becomes one `Text` block (fenced), its exit status the `Finished`.

use std::time::Duration;

use anyhow::Context;
use mis_types::evidence::{bound_output, MAX_OUTPUT_BYTES};
use mis_types::{AgentEvent, TaskId, SCRIPT_HARNESS, TASK_ENV};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Agent, BoxFut, RunOpts};

/// How long output is still read once the script exits: a process it left
/// running may hold its pipes open for good.
const DRAIN: Duration = Duration::from_secs(2);

/// Runs scripts. Not in [`crate::available`]: every worker runs it, and only routines start it.
pub struct Script;

impl Agent for Script {
    fn name(&self) -> &'static str {
        SCRIPT_HARNESS
    }

    fn models(&self) -> Vec<String> {
        vec![]
    }

    /// Writes the script to an owner-only temp dir and runs it in `opts.cwd`:
    /// directly if it starts with `#!` (unix), else with `sh`. Only `cwd` and `env`
    /// of `opts` apply.
    fn run<'a>(
        &'a self,
        task: TaskId,
        script: &'a str,
        opts: RunOpts<'a>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let dir = crate::owner_dir(&std::env::temp_dir().join(format!("mis-script-{task}")))?;
            let _cleanup = Remove(dir.clone());
            let path = dir.join("script");
            std::fs::write(&path, script).context("writing the script")?;
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
            // Inline, as `run_guarded` shows only the outermost context.
            let mut child =
                (cmd.spawn()).map_err(|e| anyhow::anyhow!("starting the script: {e}"))?;
            // Declared after `child`, so it drops first, while the tree still hangs off it.
            let mut tree = crate::Tree::new(child.id());
            let _ = tx
                .send(AgentEvent::Started {
                    session: None,
                    model: None,
                })
                .await;
            // stdout and stderr interleaved as they arrive, as a terminal shows them.
            let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
            let (mut obuf, mut ebuf) = ([0u8; 8192], [0u8; 8192]);
            let (mut out_open, mut err_open) = (true, true);
            let (mut status, mut stop) = (None, None);
            let mut kept = Output::default();
            while out_open || err_open || status.is_none() {
                tokio::select! {
                    n = out.read(&mut obuf), if out_open => match n? {
                        0 => out_open = false,
                        n => kept.push(&obuf[..n]),
                    },
                    n = err.read(&mut ebuf), if err_open => match n? {
                        0 => err_open = false,
                        n => kept.push(&ebuf[..n]),
                    },
                    s = child.wait(), if status.is_none() => {
                        status = Some(s.context("waiting for the script")?);
                        tree.disarm();
                        stop = Some(tokio::time::Instant::now() + DRAIN);
                    }
                    () = tokio::time::sleep_until(stop.unwrap_or_else(tokio::time::Instant::now)),
                        if stop.is_some() => break,
                }
            }
            let (text, cut) = kept.text();
            let code = status.and_then(|s| s.code());
            for ev in finish(&text, cut, code) {
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    }
}

/// Deletes the script's dir when the run ends, however it ends.
struct Remove(std::path::PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Output as read, bounded: once over twice `MAX_OUTPUT_BYTES`, only the last
/// `MAX_OUTPUT_BYTES` stay, so endless output (even one endless line) can't fill memory.
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    cut: bool,
}

impl Output {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > 2 * MAX_OUTPUT_BYTES {
            self.bytes.drain(..self.bytes.len() - MAX_OUTPUT_BYTES);
            self.cut = true;
        }
    }

    /// Its tail as command output is kept (see `bound_output`), and whether any was cut.
    fn text(&self) -> (String, bool) {
        let (text, cut) = bound_output(&String::from_utf8_lossy(&self.bytes));
        (text, cut || self.cut)
    }
}

/// Events ending a run: the output fenced as one `Text` (none if it printed nothing),
/// then `Finished`, ok on exit 0, its summary the last line printed or the exit status.
/// `code` None: killed by a signal.
fn finish(text: &str, cut: bool, code: Option<i32>) -> Vec<AgentEvent> {
    let mut evs = vec![];
    let body = text.trim_end();
    if !body.is_empty() {
        // A line opening a fence would close ours; a zero-width space keeps it text.
        let lines: Vec<String> = (body.lines())
            .map(|l| match l.trim_start().starts_with("```") {
                true => format!("\u{200b}{l}"),
                false => l.to_string(),
            })
            .collect();
        let cut = if cut { "(earlier output cut)\n" } else { "" };
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

    #[test]
    fn output_is_fenced_and_the_last_line_sums_it_up() {
        let mut out = Output::default();
        out.push(b"cloned a\n```\nfetched b\r\n");
        let (text, cut) = out.text();
        assert_eq!(
            finish(&text, cut, Some(0)),
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
            finish("fatal: no repo\n", false, Some(128))[1],
            AgentEvent::Finished {
                ok: false,
                summary: Some("exit 128: fatal: no repo".into())
            }
        );
        assert_eq!(
            finish("", false, None),
            vec![AgentEvent::Finished {
                ok: false,
                summary: Some("killed".into())
            }]
        );
        assert_eq!(
            finish("", false, Some(0))[0],
            AgentEvent::Finished {
                ok: true,
                summary: Some("done".into())
            }
        );
        let AgentEvent::Text { text } = &finish("x\n", true, Some(0))[0] else {
            panic!()
        };
        assert!(text.starts_with("(earlier output cut)\n```"));
    }

    #[test]
    fn endless_output_stays_bounded_even_mid_character_or_line() {
        let mut out = Output::default();
        // 7-byte lines of 3-byte characters: cuts land mid-character.
        (0..100_000).for_each(|_| out.push("日本\n".as_bytes()));
        assert!(out.bytes.len() <= 2 * MAX_OUTPUT_BYTES);
        let (text, cut) = out.text();
        assert!(cut && text.lines().all(|l| l == "日本"), "whole lines only");
        // One line with no end: its tail.
        let mut out = Output::default();
        (0..100_000).for_each(|_| out.push(b"progress "));
        assert!(out.bytes.len() <= 2 * MAX_OUTPUT_BYTES);
        let (text, cut) = out.text();
        assert!(cut && text.len() <= MAX_OUTPUT_BYTES && text.ends_with("progress "));
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
        // A process left holding its output doesn't keep the run going.
        let t = std::time::Instant::now();
        let evs = run("sleep 30 &\necho left\n").await;
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert!(
            matches!(&evs[2], AgentEvent::Finished { ok: true, .. }),
            "{evs:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
