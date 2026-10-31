//! Claude Code backend: `claude -p <prompt> --output-format stream-json --verbose`.

use anyhow::Context;
use mis_types::{AgentEvent, LimitWindow, PermissionMode, Permissions, TaskId, TASK_ENV};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use std::collections::HashSet;

use crate::{Agent, BoxFut, RunOpts, MCP_SERVER, MCP_TOKEN_ENV};

#[derive(Default)]
pub struct ClaudeAgent {
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

/// `--mcp-config` JSON offering `url` as MCP server `MCP_SERVER`, sending
/// `headers` (values may name env vars as `${VAR}`, which claude expands, so
/// secrets stay out of argv).
fn mcp_config(url: &str, headers: &[(&str, String)]) -> String {
    let headers: serde_json::Map<_, _> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone().into()))
        .collect();
    let config = serde_json::json!({"mcpServers": {MCP_SERVER: {
        "type": "http",
        "url": url,
        "headers": headers,
        // Lifts claude's 60 s per-request and 5 min idle limits for this server,
        // so `approve` can wait for a person (the worker answers by APPROVAL_WAIT).
        "timeout": (crate::APPROVAL_WAIT + std::time::Duration::from_secs(60)).as_millis() as u64,
    }}});
    config.to_string()
}

/// Flags offering `url` (see `mcp_config`) as the only MCP server, its tools
/// pre-approved so they work without `--dangerously-skip-permissions`.
pub fn mcp_args(url: &str, headers: &[(&str, String)]) -> Vec<String> {
    vec![
        "--mcp-config".into(),
        mcp_config(url, headers),
        "--strict-mcp-config".into(),
        "--allowedTools".into(),
        format!("mcp__{MCP_SERVER}"),
    ]
}

/// Folders mode's Bash sandbox: an allowed command (say `python3 -c ...`) can
/// write only the cwd, the folders and a temp dir, enforced by the OS; network
/// stays open. No unsandboxed retry, and no run at all where the sandbox can't
/// start (Linux without bubblewrap). Allowed commands still need their rule.
const SANDBOX: &str = r#"{"sandbox":{"enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false,"autoAllowBashIfSandboxed":false,"network":{"allowedDomains":["*"]}}}"#;

/// Flags for `p` (which passed `crate::check`). Folders and ask: edits auto-accepted
/// in the cwd and each folder, shell commands sandboxed to them (`SANDBOX`);
/// `allowed` gets each MCP server's tools and each command prefix. Anything
/// else is denied, as nobody can answer a prompt.
fn permission_args(p: &Permissions, allowed: &mut Vec<String>) -> Vec<String> {
    if p.mode == PermissionMode::Full {
        return vec!["--dangerously-skip-permissions".into()];
    }
    let mut a: Vec<String> = ["--permission-mode", "acceptEdits", "--settings", SANDBOX]
        .map(String::from)
        .to_vec();
    for f in &p.folders {
        a.extend(["--add-dir".into(), f.clone()]);
    }
    allowed.extend(p.mcp.iter().map(|s| format!("mcp__{s}__*")));
    allowed.extend(p.commands.iter().map(|c| format!("Bash({c} *)")));
    a
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

/// Calls to the worker's own tools (`report_status`, `report_evidence`),
/// including claude's lookup of them: the worker already reports them as
/// events, so the echo is noise (and would make every task look like it used tools).
fn is_own_tool(block: &Value) -> bool {
    let own = |name: &str| name.starts_with(&format!("mcp__{MCP_SERVER}__"));
    let name = block["name"].as_str().unwrap_or("");
    let lookup = block["input"]["query"]
        .as_str()
        .and_then(|q| q.strip_prefix("select:"));
    own(name) || (name == "ToolSearch" && lookup.is_some_and(|q| q.split(',').all(own)))
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
        // `allowed` / `allowed_warning` still run; anything else (e.g. `rejected`) is a hit limit.
        Some("rate_limit_event") => {
            let info = &v["rate_limit_info"];
            let windows = windows(info);
            let limits = (!windows.is_empty()).then_some(AgentEvent::Limits { windows });
            let limited = match info["status"].as_str() {
                Some(s) if !s.starts_with("allowed") => Some(AgentEvent::Limited {
                    resets_at: info["resetsAt"].as_u64(),
                }),
                _ => None,
            };
            limits.into_iter().chain(limited).collect()
        }
        Some("result") => vec![AgentEvent::Finished {
            ok: v["is_error"] != true && v["subtype"].as_str().is_none_or(|s| s == "success"),
            summary: v["result"].as_str().map(String::from),
        }],
        _ => vec![],
    }
}

/// `unifiedWindows.{five_hour,seven_day}` of a `rate_limit_info`, skipping any
/// without a utilization (0-1). `observed_at` is 0: the worker stamps it.
/// Overage counts when `overageStatus` is `allowed*`.
fn windows(info: &Value) -> Vec<LimitWindow> {
    let overage = info["overageStatus"]
        .as_str()
        .is_some_and(|s| s.starts_with("allowed"));
    [("five_hour", "5h"), ("seven_day", "7d")]
        .into_iter()
        .filter_map(|(key, name)| {
            let w = &info["unifiedWindows"][key];
            Some(LimitWindow {
                name: name.into(),
                used_pct: (w["utilization"].as_f64()? * 100.0).clamp(0.0, 100.0) as f32,
                resets_at: w["resetsAt"].as_u64().map(|s| s * 1000),
                observed_at: 0,
                overage,
            })
        })
        .collect()
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

impl ClaudeAgent {
    /// `claude` argv for one run; the MCP token goes in the env (see `mcp_args`).
    pub fn argv(&self, prompt: &str, o: &RunOpts) -> Vec<String> {
        let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
            .map(String::from)
            .to_vec();
        let mut allowed = vec![];
        if let Some(m) = o.mcp {
            let auth = format!("Bearer ${{{MCP_TOKEN_ENV}}}");
            a.extend([
                "--mcp-config".into(),
                mcp_config(&m.url, &[("Authorization", auth)]),
            ]);
            allowed.push(format!("mcp__{MCP_SERVER}"));
        }
        if let Some(p) = o.permissions {
            a.extend(permission_args(p, &mut allowed));
            // Ask: what the rules don't allow goes to the worker's `approve` tool.
            if p.mode == PermissionMode::Ask && o.mcp.is_some() {
                let tool = format!("mcp__{MCP_SERVER}__{}", crate::APPROVE_TOOL);
                a.extend(["--permission-prompt-tool".into(), tool]);
            }
        }
        // One list: each value is its own argv entry, as rules may hold spaces.
        if !allowed.is_empty() {
            a.push("--allowedTools".into());
            a.extend(allowed);
        }
        a.extend(self.extra_args.iter().cloned());
        // One `=` token each, so a value starting with '-' can't be read as a flag.
        if let Some(m) = o.model {
            a.push(format!("--model={m}"));
        }
        // Forked, so two replies to one task don't see each other's turns.
        if let Some(s) = o.resume {
            a.extend([format!("--resume={s}"), "--fork-session".into()]);
        }
        a.extend(["--".into(), with_images(prompt, o.images)]); // prompt may start with '-'
        a
    }
}

/// Claude has no image flag: name the files in the prompt for its Read tool.
fn with_images(prompt: &str, images: &[String]) -> String {
    match images {
        [] => prompt.into(),
        _ => format!(
            "{prompt}\n\nAttached images: {} (read them with the Read tool)",
            images.join(", ")
        ),
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
        opts: RunOpts<'a>,
        tx: mpsc::Sender<AgentEvent>,
    ) -> BoxFut<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("claude");
            cmd.args(self.argv(prompt, &opts));
            if let Some(m) = opts.mcp {
                cmd.env(MCP_TOKEN_ENV, &m.token);
            }
            if let Some(d) = opts.cwd {
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
        // Only the session's own status counts; a rejected overage doesn't stop it.
        let limit = |status: &str| {
            parse_line(&format!(
                r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"{status}","resetsAt":1793223000,"rateLimitType":"five_hour","overageStatus":"rejected"}}}}"#
            ))
        };
        assert!(limit("allowed").is_empty());
        assert!(limit("allowed_warning").is_empty());
        assert_eq!(
            limit("rejected"),
            vec![AgentEvent::Limited {
                resets_at: Some(1793223000)
            }]
        );
        assert!(parse_line("not json").is_empty());
    }

    /// From a live `claude -p` run (ids zeroed).
    const RATE_LIMIT_LINE: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793223000,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.43,"resetsAt":1793223000,"status":"allowed"},"seven_day":{"utilization":0.62,"resetsAt":1793444400,"status":"allowed"}}},"uuid":"00000000-0000-0000-0000-000000000000","session_id":"00000000-0000-0000-0000-000000000000"}"#;

    #[test]
    fn rate_limit_windows_are_reported() {
        let w = |name: &str, used_pct, resets_at| LimitWindow {
            name: name.into(),
            used_pct,
            resets_at,
            observed_at: 0,
            overage: false, // `overageStatus: rejected`
        };
        assert_eq!(
            parse_line(RATE_LIMIT_LINE),
            vec![AgentEvent::Limits {
                windows: vec![
                    w("5h", 43.0, Some(1_793_223_000_000)),
                    w("7d", 62.0, Some(1_793_444_400_000))
                ]
            }]
        );
        // Rejected: the windows, then the hit limit.
        let rejected =
            RATE_LIMIT_LINE.replacen(r#""status":"allowed""#, r#""status":"rejected""#, 1);
        let got = parse_line(&rejected);
        assert!(matches!(got[0], AgentEvent::Limits { .. }));
        assert_eq!(
            got[1],
            AgentEvent::Limited {
                resets_at: Some(1793223000)
            }
        );
        // A window without utilization is skipped; one without a reset keeps None.
        let partial = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"status":"allowed"},"seven_day":{"utilization":1.2}}}}"#;
        assert_eq!(
            parse_line(partial),
            vec![AgentEvent::Limits {
                windows: vec![w("7d", 100.0, None)]
            }]
        );
        // Overage allowed: a full window doesn't stop it.
        let over = RATE_LIMIT_LINE.replace(
            r#""overageStatus":"rejected""#,
            r#""overageStatus":"allowed""#,
        );
        let AgentEvent::Limits { windows } = &parse_line(&over)[0] else {
            panic!()
        };
        assert!(windows.iter().all(|w| w.overage));
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
            call(
                "t4",
                "ToolSearch",
                r#"{"query":"select:mcp__mis__report_status,mcp__mis__report_evidence"}"#,
            ),
            result("t4"),
            call("t5", "mcp__mis__report_evidence", r#"{"kind":"note"}"#),
            result("t5"),
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
        let mixed = r#"{"query":"select:mcp__mis__report_status,Bash"}"#;
        assert_eq!(p.line(&call("t6", "ToolSearch", mixed)).len(), 1);
        assert_eq!(p.line(&result("t3")).len(), 1);
        assert!(p.hidden.is_empty());
    }

    #[test]
    fn mcp_config_names_url_and_token_env_only() {
        let auth = ("Authorization", "Bearer ${MIS_MCP_TOKEN}".to_string());
        let args = mcp_args("http://127.0.0.1:9/mcp", &[auth]);
        assert_eq!(args[0], "--mcp-config");
        let config: Value = serde_json::from_str(&args[1]).unwrap();
        let mis = &config["mcpServers"]["mis"];
        assert_eq!(
            (mis["type"].as_str(), mis["url"].as_str()),
            (Some("http"), Some("http://127.0.0.1:9/mcp"))
        );
        assert_eq!(mis["headers"]["Authorization"], "Bearer ${MIS_MCP_TOKEN}");
        // `mis ask`: only the fleet's server. Tasks use `mcp_config` without the flag.
        assert_eq!(
            args[2..],
            ["--strict-mcp-config", "--allowedTools", "mcp__mis"]
        );
    }

    #[test]
    fn argv_resumes_and_keeps_prompt_last() {
        let agent = ClaudeAgent {
            extra_args: vec!["--max-turns".into(), "1".into()],
        };
        let full = Permissions {
            mode: PermissionMode::Full,
            folders: vec!["/ignored".into()],
            ..Default::default()
        };
        let o = RunOpts {
            model: Some("haiku"),
            resume: Some("s-1"),
            permissions: Some(&full),
            ..Default::default()
        };
        assert_eq!(
            agent.argv("-hi", &o),
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--max-turns",
                "1",
                "--model=haiku",
                "--resume=s-1",
                "--fork-session",
                "--",
                "-hi"
            ]
        );
        let plain = ClaudeAgent::default().argv("hi", &RunOpts::default());
        assert!(!plain.iter().any(|a| a.starts_with("--resume")));
        assert_eq!(plain.last().unwrap(), "hi");
    }

    #[test]
    fn folders_accept_edits_in_each_dir_and_allow_listed_tools_only() {
        let p = Permissions {
            mode: PermissionMode::Folders,
            folders: vec!["/u/game".into(), "/u/my assets".into()],
            mcp: vec!["blender".into()],
            commands: vec!["python3".into(), "git log".into()],
        };
        let mcp = crate::Mcp {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "t".into(),
        };
        let o = RunOpts {
            mcp: Some(&mcp),
            permissions: Some(&p),
            ..Default::default()
        };
        let a = ClaudeAgent::default().argv("hi", &o);
        // The user's own MCP servers (e.g. blender) load beside ours.
        assert!(!a.iter().any(|x| x == "--strict-mcp-config"));
        assert!(!a.iter().any(|x| x.contains("dangerously")));
        assert_eq!(
            a[6..],
            [
                "--permission-mode",
                "acceptEdits",
                "--settings",
                SANDBOX,
                "--add-dir",
                "/u/game",
                "--add-dir",
                "/u/my assets",
                "--allowedTools",
                "mcp__mis",
                "mcp__blender__*",
                "Bash(python3 *)",
                "Bash(git log *)",
                "--",
                "hi"
            ]
        );
        // Unset: only our own server's tools are allowed.
        let o = RunOpts {
            mcp: Some(&mcp),
            ..Default::default()
        };
        let a = ClaudeAgent::default().argv("hi", &o);
        assert_eq!(a[6..], ["--allowedTools", "mcp__mis", "--", "hi"]);
    }

    #[test]
    fn ask_sends_what_the_rules_dont_allow_to_the_approve_tool() {
        let p = Permissions {
            mode: PermissionMode::Ask,
            folders: vec!["/u/game".into()],
            ..Default::default()
        };
        let mcp = crate::Mcp {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "t".into(),
        };
        let o = RunOpts {
            mcp: Some(&mcp),
            permissions: Some(&p),
            ..Default::default()
        };
        let a = ClaudeAgent::default().argv("hi", &o);
        let at = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].as_str());
        assert_eq!(at("--permission-prompt-tool"), Some("mcp__mis__approve"));
        assert_eq!(at("--permission-mode"), Some("acceptEdits"));
        assert_eq!(at("--add-dir"), Some("/u/game"));
        let config: Value = serde_json::from_str(at("--mcp-config").unwrap()).unwrap();
        assert!(config["mcpServers"]["mis"]["timeout"].as_u64().unwrap() > 30 * 60 * 1000);
        // No server to ask through: plain folders, denied rather than stuck.
        let o = RunOpts { mcp: None, ..o };
        let a = ClaudeAgent::default().argv("hi", &o);
        assert!(!a.iter().any(|x| x == "--permission-prompt-tool"));
    }

    #[test]
    fn argv_names_images_in_the_prompt() {
        let images = ["/t/a.jpg".to_string(), "/t/b.png".to_string()];
        let o = RunOpts {
            images: &images,
            ..Default::default()
        };
        let a = ClaudeAgent::default().argv("what is this?", &o);
        assert_eq!(
            &a[a.len() - 2..],
            [
                "--",
                "what is this?\n\nAttached images: /t/a.jpg, /t/b.png (read them with the Read tool)"
            ]
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
