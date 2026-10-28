//! Codex CLI backend: `codex exec --json <prompt>`.

use anyhow::Context;
use mis_types::{AgentEvent, TaskId, TASK_ENV};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Agent, BoxFut, RunOpts, MCP_SERVER, MCP_TOKEN_ENV};

#[derive(Default)]
pub struct CodexAgent {
    /// Pass `--dangerously-bypass-approvals-and-sandbox`.
    pub skip_permissions: bool,
}

/// Slugs from codex's `models_cache.json` with visibility "list", by priority,
/// after the default `model` set at the top level of `config.toml` (if any).
/// Either file failing to parse is an error.
pub fn codex_models(cache_json: &str, config_toml: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(cache_json).map_err(|e| e.to_string())?;
    let mut listed: Vec<(i64, &str)> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            Some((
                m["priority"].as_i64().unwrap_or(i64::MAX),
                m["slug"].as_str()?,
            ))
        })
        .collect();
    listed.sort_by_key(|(p, _)| *p);
    let config: toml::Table =
        toml::from_str(config_toml).map_err(|e| format!("config.toml: {e}"))?;
    let default = config
        .get("model")
        .and_then(|m| m.as_str())
        .map(String::from);
    let mut out: Vec<String> = default.into_iter().collect();
    for (_, slug) in listed {
        if !out.iter().any(|o| o == slug) {
            out.push(slug.to_string());
        }
    }
    Ok(out)
}

/// `-c` overrides offering `url` as MCP server `MCP_SERVER`, its bearer token read
/// from `MCP_TOKEN_ENV`, its tools approved (else `exec` refuses to call them).
pub fn mcp_args(url: &str) -> Vec<String> {
    let key = format!("mcp_servers.{MCP_SERVER}");
    [
        format!("{key}.url=\"{url}\""),
        format!("{key}.bearer_token_env_var=\"{MCP_TOKEN_ENV}\""),
        format!("{key}.default_tools_approval_mode=\"approve\""),
    ]
    .into_iter()
    .flat_map(|kv| ["-c".to_string(), kv])
    .collect()
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
                // `thread.started` carries no model.
                model: None,
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
        // One turn per `exec`, so its usage is the run's total. `input_tokens`
        // already includes `cached_input_tokens`.
        (Some("turn.completed"), _) => {
            let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            vec![
                AgentEvent::Usage {
                    input: n("input_tokens"),
                    output: n("output_tokens"),
                },
                AgentEvent::Finished {
                    ok: true,
                    summary: None,
                },
            ]
        }
        (Some("turn.failed"), _) => vec![AgentEvent::Finished {
            ok: false,
            summary: v["error"]["message"].as_str().map(String::from),
        }],
        _ => vec![],
    }
}

impl CodexAgent {
    /// `codex` argv for one run: `exec [opts] -- <prompt>`, or
    /// `exec resume [opts] -- <session> <prompt>` to continue a session.
    pub fn argv(&self, prompt: &str, o: &RunOpts) -> Vec<String> {
        let mut a: Vec<String> = vec!["exec".into()];
        if o.resume.is_some() {
            a.push("resume".into());
        }
        a.extend(["--json", "--skip-git-repo-check"].map(String::from));
        if self.skip_permissions {
            a.push("--dangerously-bypass-approvals-and-sandbox".into());
        }
        // One `=` token, so a model starting with '-' can't be read as a flag.
        if let Some(m) = o.model {
            a.push(format!("--model={m}"));
        }
        if let Some(m) = o.mcp {
            a.extend(mcp_args(&m.url));
        }
        a.push("--".into());
        a.extend(o.resume.map(String::from));
        a.push(prompt.into());
        a
    }
}

impl Agent for CodexAgent {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn models(&self) -> Vec<String> {
        let dir = std::env::var_os("CODEX_HOME")
            .map_or_else(|| crate::home().map(|h| h.join(".codex")), |d| Ok(d.into()));
        let models = dir.and_then(|dir| {
            // No config just means no default to put first.
            let config = dir.join("config.toml");
            let config = match std::fs::read_to_string(&config) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                r => r.map_err(|e| format!("{}: {e}", config.display()))?,
            };
            crate::parse_file(&dir.join("models_cache.json"), |c| codex_models(c, &config))
        });
        models.unwrap_or_else(|e| {
            tracing::warn!("{e}; codex will run its default model");
            vec![]
        })
    }

    fn run<'a>(
        &'a self,
        task: TaskId,
        prompt: &'a str,
        opts: RunOpts<'a>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("codex");
            cmd.args(self.argv(prompt, &opts));
            if let Some(m) = opts.mcp {
                cmd.env(MCP_TOKEN_ENV, &m.token);
            }
            if let Some(d) = opts.cwd {
                cmd.current_dir(d);
            }
            cmd.env(TASK_ENV, task.to_string());
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
                session: Some("t1".into()),
                model: None,
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
            parse_line(
                r#"{"type":"turn.completed","usage":{"input_tokens":21015,"cached_input_tokens":12160,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0}}"#
            ),
            vec![
                AgentEvent::Usage {
                    input: 21015,
                    output: 5
                },
                AgentEvent::Finished {
                    ok: true,
                    summary: None
                }
            ]
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

    #[test]
    fn argv_new_and_resumed() {
        let agent = CodexAgent {
            skip_permissions: true,
        };
        let o = RunOpts {
            model: Some("gpt-5"),
            ..Default::default()
        };
        assert_eq!(
            agent.argv("hi", &o),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model=gpt-5",
                "--",
                "hi"
            ]
        );
        let o = RunOpts {
            resume: Some("t-1"),
            ..Default::default()
        };
        assert_eq!(
            CodexAgent::default().argv("-more", &o),
            [
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--",
                "t-1",
                "-more"
            ]
        );
    }

    #[test]
    fn mcp_overrides_url_token_env_and_approval() {
        assert_eq!(
            mcp_args("http://127.0.0.1:9/mcp"),
            [
                "-c",
                r#"mcp_servers.mis.url="http://127.0.0.1:9/mcp""#,
                "-c",
                r#"mcp_servers.mis.bearer_token_env_var="MIS_MCP_TOKEN""#,
                "-c",
                r#"mcp_servers.mis.default_tools_approval_mode="approve""#,
            ]
        );
    }

    #[test]
    fn models_listed_by_priority_after_configured_default() {
        let cache = r#"{"fetched_at":"x","models":[
            {"slug":"gpt-b","visibility":"list","priority":4},
            {"slug":"gpt-hidden","visibility":"hide","priority":0},
            {"slug":"gpt-a","visibility":"list","priority":1},
            {"slug":"gpt-c","visibility":"list","priority":2}]}"#;
        let config = "model = \"gpt-c\"\nmodel_reasoning_effort = \"high\"\n\
                      [projects.\"/x\"]\nmodel = \"gpt-b\"\n";
        assert_eq!(
            codex_models(cache, config).unwrap(),
            ["gpt-c", "gpt-a", "gpt-b"]
        );
        assert_eq!(
            codex_models(cache, "").unwrap(),
            ["gpt-a", "gpt-c", "gpt-b"]
        );
        assert!(codex_models(cache, "model = ")
            .unwrap_err()
            .contains("config.toml"));
        assert!(codex_models("{", "").is_err());
    }
}
