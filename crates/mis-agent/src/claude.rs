//! Claude Code backend: `claude -p <prompt> --output-format stream-json --verbose`.

use anyhow::Context;
use mis_types::{AgentEvent, TaskId, TASK_ENV};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use std::collections::HashSet;

use crate::{Agent, BoxFut, Mcp, MCP_SERVER, MCP_TOKEN_ENV};

#[derive(Default)]
pub struct ClaudeAgent {
    /// Pass `--dangerously-skip-permissions` so tool calls don't block headless.
    pub skip_permissions: bool,
    /// Extra CLI flags, e.g. `--tools ""` for a tool-less planner call.
    pub extra_args: Vec<String>,
}

/// Model aliases the claude CLI documents for `--model`.
pub const ALIASES: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];

/// `ALIASES` plus the extra models this account offers, from
/// `additionalModelOptionsCache[].value` in `~/.claude.json` (claude has no
/// command to list models).
pub fn claude_models(claude_json: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(claude_json).map_err(|e| e.to_string())?;
    let mut out: Vec<String> = ALIASES.map(String::from).to_vec();
    let extra = v["additionalModelOptionsCache"]
        .as_array()
        .into_iter()
        .flatten();
    for m in extra.filter_map(|o| o["value"].as_str()) {
        if !out.iter().any(|o| o == m) {
            out.push(m.to_string());
        }
    }
    Ok(out)
}

/// Flags offering `url` as MCP server `MCP_SERVER` and nothing else. The bearer
/// token is read from `MCP_TOKEN_ENV` (claude expands `${VAR}` in the config).
/// Its tools are pre-approved so they work without `--dangerously-skip-permissions`.
pub fn mcp_args(url: &str) -> Vec<String> {
    let config = serde_json::json!({"mcpServers": {MCP_SERVER: {
        "type": "http",
        "url": url,
        "headers": {"Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}")},
    }}});
    vec![
        "--mcp-config".into(),
        config.to_string(),
        "--strict-mcp-config".into(),
        "--allowedTools".into(),
        format!("mcp__{MCP_SERVER}"),
    ]
}

/// Map one stream-json line to zero or more events, without token usage
/// (that needs the whole stream; see `Parser`).
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    let mut hidden = Default::default();
    serde_json::from_str::<Value>(line).map_or(vec![], |v| events(&v, &mut hidden))
}

/// `parse_line` plus a running `Usage`. Each assistant API message repeats its
/// usage on every content block line, so it counts once per message id (latest
/// line wins); the `result` line's session totals replace the tally.
#[derive(Default)]
pub struct Parser {
    /// Latest usage per message id; lines of different messages may interleave.
    messages: std::collections::HashMap<String, (u64, u64)>,
    sent: Option<(u64, u64)>,
    /// Ids of tool calls `events` hid, so it can hide their results too.
    hidden: HashSet<String>,
}

impl Parser {
    pub fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        let mut out = events(&v, &mut self.hidden);
        let total = match v["type"].as_str() {
            Some("assistant") => usage(&v["message"]["usage"]).map(|u| {
                let id = v["message"]["id"].as_str().unwrap_or_default();
                self.messages.insert(id.to_string(), u);
                self.messages
                    .values()
                    .fold((0, 0), |a, m| (a.0 + m.0, a.1 + m.1))
            }),
            Some("result") => usage(&v["usage"]),
            _ => None,
        };
        if let Some((input, output)) = total.filter(|t| self.sent != Some(*t)) {
            self.sent = total;
            // Before any Finished: the orchestrator drops events after it.
            out.insert(0, AgentEvent::Usage { input, output });
        }
        out
    }
}

/// (input incl. cache reads/writes, output) from a usage object.
fn usage(u: &Value) -> Option<(u64, u64)> {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    u.is_object().then(|| {
        (
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
            n("output_tokens"),
        )
    })
}

/// Calls to the worker's own `report_status` tool, including claude's lookup of
/// it: the worker already reports them as `Status`, so the echo is noise (and
/// would make every task look like it used tools).
fn is_own_tool(block: &Value) -> bool {
    let tool = format!("mcp__{MCP_SERVER}__report_status");
    block["name"] == tool.as_str()
        || (block["name"] == "ToolSearch"
            && block["input"]["query"] == format!("select:{tool}").as_str())
}

/// Events for one line. Tool calls hidden by `is_own_tool` go into `hidden`,
/// and their results are dropped when they arrive.
fn events(v: &Value, hidden: &mut HashSet<String>) -> Vec<AgentEvent> {
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            vec![AgentEvent::Started {
                session: v["session_id"].as_str().map(String::from),
                model: v["model"].as_str().map(String::from),
            }]
        }
        Some("assistant") => blocks(v)
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(AgentEvent::Text {
                    text: b["text"].as_str()?.to_string(),
                }),
                "tool_use" if is_own_tool(b) => {
                    hidden.insert(b["id"].as_str()?.to_string());
                    None
                }
                "tool_use" => Some(AgentEvent::ToolUse {
                    name: b["name"].as_str()?.to_string(),
                    input: b["input"].clone(),
                }),
                _ => None,
            })
            .collect(),
        Some("user") => blocks(v)
            .filter(|b| b["type"] == "tool_result")
            .filter(|b| {
                !b["tool_use_id"]
                    .as_str()
                    .is_some_and(|id| hidden.remove(id))
            })
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

    fn models(&self) -> Vec<String> {
        // claude keeps `.claude.json` in CLAUDE_CONFIG_DIR when set, else in home.
        let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(crate::home, |d| Ok(d.into()));
        let models = dir.and_then(|d| crate::parse_file(&d.join(".claude.json"), claude_models));
        models.unwrap_or_else(|e| {
            tracing::warn!("{e}; offering only the claude model aliases");
            ALIASES.map(String::from).to_vec()
        })
    }

    fn run<'a>(
        &'a self,
        task: TaskId,
        prompt: &'a str,
        cwd: Option<&'a str>,
        model: Option<&'a str>,
        mcp: Option<&'a Mcp>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("claude");
            cmd.args(["-p", "--output-format", "stream-json", "--verbose"]);
            if self.skip_permissions {
                cmd.arg("--dangerously-skip-permissions");
            }
            cmd.args(&self.extra_args);
            if let Some(m) = mcp {
                cmd.args(mcp_args(&m.url)).env(MCP_TOKEN_ENV, &m.token);
            }
            // One `=` token, so a model starting with '-' can't be read as a flag.
            if let Some(m) = model {
                cmd.arg(format!("--model={m}"));
            }
            cmd.arg("--").arg(prompt); // prompt may start with '-'
            if let Some(d) = cwd {
                cmd.current_dir(d);
            }
            cmd.env(TASK_ENV, task.to_string());
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
            let mut parser = Parser::default();
            while let Some(line) = lines.next_line().await? {
                for ev in parser.line(&line) {
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
        let init = r#"{"type":"system","subtype":"init","session_id":"abc","tools":[],"model":"claude-sonnet-4-5"}"#;
        assert_eq!(
            parse_line(init),
            vec![AgentEvent::Started {
                session: Some("abc".into()),
                model: Some("claude-sonnet-4-5".into()),
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

    #[test]
    fn tallies_usage_once_per_message_then_takes_result_totals() {
        let asst = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":8,"output_tokens":{out}}}}}}}"#
            )
        };
        let usage = |input, output| AgentEvent::Usage { input, output };
        let text = AgentEvent::Text { text: "x".into() };
        let mut p = Parser::default();
        assert_eq!(p.line(&asst("m1", 4)), [usage(110, 4), text.clone()]);
        // Same message again (next content block): replaced, not added; unchanged is not resent.
        assert_eq!(p.line(&asst("m1", 9)), [usage(110, 9), text.clone()]);
        assert_eq!(p.line(&asst("m1", 9)), std::slice::from_ref(&text));
        assert_eq!(p.line(&asst("m2", 1)), [usage(220, 10), text.clone()]);
        // Interleaved (e.g. a subagent's messages): m1 still counted once.
        assert_eq!(p.line(&asst("m1", 12)), [usage(220, 13), text]);
        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":4,"cache_creation_input_tokens":200,"cache_read_input_tokens":16,"output_tokens":83}}"#;
        assert_eq!(
            p.line(res),
            [
                usage(220, 83),
                AgentEvent::Finished {
                    ok: true,
                    summary: Some("ok".into())
                }
            ]
        );
        assert!(p.line(r#"{"type":"system","subtype":"init"}"#).len() == 1);
    }

    #[test]
    fn own_status_tool_calls_and_lookup_are_hidden() {
        let call = |id: &str, name: &str, input: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
            )
        };
        let result = |id: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"ok"}}]}}}}"#
            )
        };
        let mut p = Parser::default();
        let own = [
            call(
                "t1",
                "ToolSearch",
                r#"{"query":"select:mcp__mis__report_status"}"#,
            ),
            result("t1"),
            call("t2", "mcp__mis__report_status", r#"{"text":"x"}"#),
            result("t2"),
        ];
        for line in own {
            assert!(p.line(&line).is_empty(), "{line}");
        }
        // Other tools, and other tool searches, still show.
        assert_eq!(
            p.line(&call("t3", "ToolSearch", r#"{"query":"git"}"#))
                .len(),
            1
        );
        assert_eq!(p.line(&result("t3")).len(), 1);
        assert!(p.hidden.is_empty());
    }

    #[test]
    fn mcp_config_names_url_and_token_env_only() {
        let args = mcp_args("http://127.0.0.1:9/mcp");
        assert_eq!(args[0], "--mcp-config");
        let config: Value = serde_json::from_str(&args[1]).unwrap();
        let mis = &config["mcpServers"]["mis"];
        assert_eq!(
            (mis["type"].as_str(), mis["url"].as_str()),
            (Some("http"), Some("http://127.0.0.1:9/mcp"))
        );
        assert_eq!(mis["headers"]["Authorization"], "Bearer ${MIS_MCP_TOKEN}");
        assert_eq!(
            args[2..],
            ["--strict-mcp-config", "--allowedTools", "mcp__mis"]
        );
    }

    #[test]
    fn models_are_aliases_plus_account_extras() {
        let json = r#"{"numStartups":3,"additionalModelOptionsCache":[
            {"value":"claude-fable-5-1[1m]","label":"Fable","description":"x"},
            {"value":"opus","label":"dup"},{"label":"no value"}]}"#;
        assert_eq!(
            claude_models(json).unwrap(),
            ["haiku", "sonnet", "opus", "fable", "claude-fable-5-1[1m]"]
        );
        assert_eq!(claude_models("{}").unwrap(), ALIASES);
        assert!(claude_models("not json").is_err());
    }
}
