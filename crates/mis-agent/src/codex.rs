//! Codex CLI backend: `codex exec --json <prompt>`.

use anyhow::Context;
use mis_types::AgentEvent;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Agent, BoxFut};

#[derive(Default)]
pub struct CodexAgent {
    /// Pass `--dangerously-bypass-approvals-and-sandbox`.
    pub skip_permissions: bool,
}

/// Map one `--json` line to zero or more events.
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let item = &v["item"];
    match (v["type"].as_str(), item["type"].as_str()) {
        (Some("thread.started"), _) => {
            vec![AgentEvent::Started {
                session: v["thread_id"].as_str().map(String::from),
            }]
        }
        (Some("item.completed"), Some("agent_message")) => {
            vec![AgentEvent::Text {
                text: item["text"].as_str().unwrap_or("").trim_end().to_string(),
            }]
        }
        (Some("item.completed"), Some("error")) => {
            vec![AgentEvent::Text {
                text: format!("error: {}", item["message"].as_str().unwrap_or("")),
            }]
        }
        (Some("item.started"), Some("command_execution")) => vec![AgentEvent::ToolUse {
            name: "shell".into(),
            input: serde_json::json!({ "command": item["command"] }),
        }],
        (Some("item.completed"), Some("command_execution")) => vec![AgentEvent::ToolResult {
            content: item["aggregated_output"].as_str().unwrap_or("").to_string(),
        }],
        (Some("turn.completed"), _) => vec![AgentEvent::Finished {
            ok: true,
            summary: None,
        }],
        (Some("turn.failed"), _) => vec![AgentEvent::Finished {
            ok: false,
            summary: v["error"]["message"].as_str().map(String::from),
        }],
        _ => vec![],
    }
}

impl Agent for CodexAgent {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn run<'a>(
        &'a self,
        prompt: &'a str,
        cwd: Option<&'a str>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("codex");
            cmd.args(["exec", "--json", "--skip-git-repo-check"]);
            if self.skip_permissions {
                cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            }
            cmd.arg("--").arg(prompt);
            if let Some(d) = cwd {
                cmd.current_dir(d);
            }
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            let mut child = cmd.spawn().context("spawning codex")?;
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            // Codex has no result payload; the last assistant message is the summary.
            let mut last_text = None;
            let mut finished = false;
            while let Some(line) = lines.next_line().await? {
                for mut ev in parse_line(&line) {
                    match &mut ev {
                        AgentEvent::Text { text } => last_text = Some(text.clone()),
                        AgentEvent::Finished { summary, .. } => {
                            finished = true;
                            if summary.is_none() {
                                *summary = last_text.take();
                            }
                        }
                        _ => {}
                    }
                    if tx.send(ev).await.is_err() {
                        let _ = child.kill().await;
                        return Ok(());
                    }
                }
            }
            let status = child.wait().await;
            if finished {
                return Ok(());
            }
            let status = status?;
            let _ = tx
                .send(AgentEvent::Finished {
                    ok: status.success(),
                    summary: Some(format!("codex exited: {status}")),
                })
                .await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_output() {
        assert_eq!(
            parse_line(r#"{"type":"thread.started","thread_id":"t1"}"#),
            vec![AgentEvent::Started {
                session: Some("t1".into())
            }]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"I’ll run the command.\n"}}"#
            ),
            vec![AgentEvent::Text {
                text: "I’ll run the command.".into()
            }]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.started","item":{"id":"item_3","type":"command_execution","command":"/bin/zsh -lc 'echo hi'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#
            ),
            vec![AgentEvent::ToolUse {
                name: "shell".into(),
                input: serde_json::json!({"command": "/bin/zsh -lc 'echo hi'"})
            }]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","command":"x","aggregated_output":"hi\n","exit_code":0,"status":"completed"}}"#
            ),
            vec![AgentEvent::ToolResult {
                content: "hi\n".into()
            }]
        );
        assert_eq!(
            parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#),
            vec![AgentEvent::Finished {
                ok: true,
                summary: None
            }]
        );
        assert_eq!(
            parse_line(r#"{"type":"turn.failed","error":{"message":"boom"}}"#),
            vec![AgentEvent::Finished {
                ok: false,
                summary: Some("boom".into())
            }]
        );
        assert!(parse_line(r#"{"type":"turn.started"}"#).is_empty());
    }
}
