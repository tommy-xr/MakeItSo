//! Claude Code backend: `claude -p <prompt> --output-format stream-json --verbose`.

use anyhow::Context;
use mis_types::{AgentEvent, TaskId};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Agent, BoxFut};

#[derive(Default)]
pub struct ClaudeAgent {
    /// Pass `--dangerously-skip-permissions` so tool calls don't block headless.
    pub skip_permissions: bool,
}

/// Map one stream-json line to zero or more events.
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            vec![AgentEvent::Started {
                session: v["session_id"].as_str().map(String::from),
            }]
        }
        Some("assistant") => blocks(&v)
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(AgentEvent::Text {
                    text: b["text"].as_str()?.to_string(),
                }),
                "tool_use" => Some(AgentEvent::ToolUse {
                    name: b["name"].as_str()?.to_string(),
                    input: b["input"].clone(),
                }),
                _ => None,
            })
            .collect(),
        Some("user") => blocks(&v)
            .filter(|b| b["type"] == "tool_result")
            .map(|b| AgentEvent::ToolResult {
                content: tool_result_text(&b["content"]),
            })
            .collect(),
        Some("result") => vec![AgentEvent::Finished {
            ok: v["is_error"] != true && v["subtype"].as_str().is_none_or(|s| s == "success"),
            summary: v["result"].as_str().map(String::from),
        }],
        _ => vec![],
    }
}

fn blocks(v: &Value) -> impl Iterator<Item = &Value> {
    v["message"]["content"].as_array().into_iter().flatten()
}

fn tool_result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl Agent for ClaudeAgent {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn run<'a>(
        &'a self,
        task: TaskId,
        prompt: &'a str,
        cwd: Option<&'a str>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("claude");
            cmd.args(["-p", "--output-format", "stream-json", "--verbose"]);
            if self.skip_permissions {
                cmd.arg("--dangerously-skip-permissions");
            }
            cmd.arg("--").arg(prompt); // prompt may start with '-'
            if let Some(d) = cwd {
                cmd.current_dir(d);
            }
            cmd.env("MIS_TASK", task.to_string());
            // stderr inherits so it can't fill a pipe and block; kill_on_drop so a
            // dropped task can't leave claude running headless.
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            let mut child = cmd.spawn().context("spawning claude")?;
            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            let mut finished = false;
            while let Some(line) = lines.next_line().await? {
                for ev in parse_line(&line) {
                    finished |= matches!(ev, AgentEvent::Finished { .. });
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
            {
                let _ = tx
                    .send(AgentEvent::Finished {
                        ok: status.success(),
                        summary: Some(format!("claude exited: {status}")),
                    })
                    .await;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_output() {
        let init = r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#;
        assert_eq!(
            parse_line(init),
            vec![AgentEvent::Started {
                session: Some("abc".into())
            }]
        );

        let asst = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"pong"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(
            parse_line(asst),
            vec![
                AgentEvent::Text {
                    text: "pong".into()
                },
                AgentEvent::ToolUse {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"})
                },
            ]
        );

        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"}]}}"#;
        assert_eq!(
            parse_line(user),
            vec![AgentEvent::ToolResult {
                content: "a\nb".into()
            }]
        );

        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","stop_reason":"end_turn"}"#;
        assert_eq!(
            parse_line(res),
            vec![AgentEvent::Finished {
                ok: true,
                summary: Some("pong".into())
            }]
        );

        let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        assert_eq!(
            parse_line(err),
            vec![AgentEvent::Finished {
                ok: false,
                summary: None
            }]
        );

        assert!(parse_line(r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(parse_line("not json").is_empty());
    }
}
