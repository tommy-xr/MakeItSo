//! Claude Code backend: `claude -p <prompt> --output-format stream-json --verbose --include-partial-messages`.

use anyhow::Context;
use mis_types::{
    bound_plan, AgentEvent, LimitWindow, PermissionMode, Permissions, PlanItem, PlanStatus, TaskId,
    TASK_ENV,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use std::collections::HashSet;

use crate::{Agent, BoxFut, RunOpts, Setup, MCP_SERVER, MCP_TOKEN_ENV, OWN_TOOLS};

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
        // so `approve` and `merge_pr` can wait for a person.
        "timeout": crate::MCP_CALL_WAIT.as_millis() as u64,
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

/// GitHub reads a read-only run may make: none of them has a flag that writes.
const GH_READS: [&str; 12] = [
    "gh pr list",
    "gh pr view",
    "gh pr diff",
    "gh pr checks",
    "gh pr status",
    "gh issue list",
    "gh issue view",
    "gh run list",
    "gh run view",
    "gh repo view",
    "gh release list",
    "gh search",
];

/// Told to a read-only run, so it keeps investigating past a denial.
const READ_ONLY_NOTE: &str = "This run is read-only: edits, and shell commands that could \
    change anything (writes, commits, `git config`), are denied. Read-only ones such as \
    `git log`, `ls` and `gh pr list` work.";

/// Flags for a read-only run under `p`, never more than `p` allows. `dontAsk`
/// denies what no rule allows; claude itself allows reading files and read-only
/// git; `allowed` gets the `GH_READS` that `p` lets run (all in full mode, else
/// those its commands cover). The machine's own settings and MCP servers are left
/// out, as their rules could allow more (say `Bash(gh *)`), and so are `p`'s
/// servers and asking. Folders keep their sandbox and dirs.
fn read_only_args(p: &Permissions, allowed: &mut Vec<String>) -> Vec<String> {
    let full = p.mode == PermissionMode::Full;
    let reads = GH_READS.iter().filter(|r| full || p.allows_command(r));
    allowed.extend(reads.map(|r| format!("Bash({r} *)")));
    let mut a: Vec<String> = [
        "--permission-mode",
        "dontAsk",
        "--setting-sources=",
        "--strict-mcp-config",
        "--append-system-prompt",
    ]
    .map(String::from)
    .to_vec();
    a.push(READ_ONLY_NOTE.into());
    if !full {
        a.extend(["--settings".into(), SANDBOX.into()]);
        for f in &p.folders {
            a.extend(["--add-dir".into(), f.clone()]);
        }
    }
    a
}

/// Denied in a live routine run, whatever else allows them: shell routes to
/// merging a PR or moving a branch that skip `merge_pr`'s checks (gh's merge,
/// API merges, ref and file writes, gh aliases, pushes to main or master), and
/// the token a direct API call would need. Each matches anywhere in a command, so wrappers
/// (`sh -c "…"`, `xargs`, `cd x && …`) are caught too, at the cost of some reads
/// (`gh api …/merge`). Best effort: a shell that reaches GitHub another way, say
/// a script it writes, isn't sandboxed.
pub const NO_MERGE: [&str; 16] = [
    "Bash(*gh pr merge*)",
    "Bash(*gh api*merge*)",
    "Bash(*gh api*Merge*)",
    "Bash(*gh api*PUT*)",
    "Bash(*gh api*git/refs*)",
    "Bash(*gh alias set*)",
    "Bash(*gh auth token*)",
    "Bash(*gh auth status*)",
    "Bash(*--show-token*)",
    "Bash(*credential fill*)",
    "Bash(*git*push*main)",
    "Bash(*git*push*main *)",
    "Bash(*git*push*master)",
    "Bash(*git*push*master *)",
    "Bash(*git*push*--mirror*)",
    "Bash(*git*push*--all*)",
];

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

/// Claude's checklist tool: each call holds the whole list.
const TODO_WRITE: &str = "TodoWrite";

/// Calls to the worker's `OWN_TOOLS`, including claude's lookup of them (or of
/// `TodoWrite`): the worker already reports them as events, so the echo is noise
/// (and would make every task look like it used tools).
fn is_own_tool(block: &Value) -> bool {
    let own = |name: &str| {
        (name.strip_prefix(&format!("mcp__{MCP_SERVER}__")))
            .is_some_and(|tool| OWN_TOOLS.contains(&tool))
    };
    let name = block["name"].as_str().unwrap_or("");
    let lookup = block["input"]["query"]
        .as_str()
        .and_then(|q| q.strip_prefix("select:"));
    let looked_up = |q: &str| q.split(',').all(|n| own(n) || n == TODO_WRITE);
    own(name) || (name == "ToolSearch" && lookup.is_some_and(looked_up))
}

/// A `TodoWrite` input (`{todos: [{content, status, activeForm}]}`) as a plan;
/// None if any row isn't one.
fn todo_plan(input: &Value) -> Option<Vec<PlanItem>> {
    let rows = input["todos"].as_array()?.iter().map(|t| {
        Some(PlanItem {
            text: t["content"].as_str()?.into(),
            status: match t["status"].as_str()? {
                "pending" => PlanStatus::Pending,
                "in_progress" => PlanStatus::InProgress,
                "completed" => PlanStatus::Completed,
                _ => return None,
            },
            active_form: t["activeForm"].as_str().map(String::from),
        })
    });
    Some(bound_plan(rows.collect::<Option<_>>()?))
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
                // The main agent's checklist (a subagent's isn't the task's), its
                // "modified successfully" result hidden.
                "tool_use" if b["name"] == TODO_WRITE && v["parent_tool_use_id"].is_null() => {
                    let Some(items) = todo_plan(&b["input"]) else {
                        return Some(AgentEvent::ToolUse {
                            name: TODO_WRITE.into(),
                            input: b["input"].clone(),
                        });
                    };
                    hidden.insert(b["id"].as_str()?.to_string());
                    Some(AgentEvent::Plan { items })
                }
                "tool_use" => Some(AgentEvent::ToolUse {
                    name: b["name"].as_str()?.to_string(),
                    input: b["input"].clone(),
                }),
                _ => None,
            })
            .collect(),
        // `--include-partial-messages`: a text block's pieces as they arrive, the main
        // agent's only (a subagent's carry its Task call's id).
        Some("stream_event") if v["parent_tool_use_id"].is_null() => {
            let d = &v["event"]["delta"];
            match (
                v["event"]["type"].as_str(),
                d["type"].as_str(),
                d["text"].as_str(),
            ) {
                (Some("content_block_delta"), Some("text_delta"), Some(t)) if !t.is_empty() => {
                    vec![AgentEvent::TextDelta { text: t.into() }]
                }
                _ => vec![],
            }
        }
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

/// A tool result's text: a string, or its text blocks joined.
pub fn tool_result_text(c: &Value) -> String {
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
        let mut a: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]
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
        match o.permissions {
            Some(p) if o.read_only => a.extend(read_only_args(p, &mut allowed)),
            p => {
                a.extend(setup_args(o.setup));
                if let Some(p) = p {
                    a.extend(permission_args(p, &mut allowed));
                    // Ask: what the rules don't allow goes to the worker's `approve` tool.
                    if p.mode == PermissionMode::Ask && o.mcp.is_some() {
                        let tool = format!("mcp__{MCP_SERVER}__{}", crate::APPROVE_TOOL);
                        a.extend(["--permission-prompt-tool".into(), tool]);
                    }
                }
            }
        }
        // One list: each value is its own argv entry, as rules may hold spaces.
        if !allowed.is_empty() {
            a.push("--allowedTools".into());
            a.extend(allowed);
        }
        if o.no_merge {
            a.push("--disallowedTools".into());
            a.extend(NO_MERGE.map(String::from));
        }
        a.extend(self.extra_args.iter().cloned());
        // One `=` token each, so a value starting with '-' can't be read as a flag.
        if let Some(m) = o.model {
            a.push(format!("--model={m}"));
        }
        // Replies fork, so two replies to one task don't see each other's turns.
        if let Some(s) = o.resume {
            a.push(format!("--resume={s}"));
            if o.fork.unwrap_or(true) {
                a.push("--fork-session".into());
            }
        }
        a.extend(["--".into(), with_images(prompt, o.images)]); // prompt may start with '-'
        a
    }
}

/// Flags narrowing what claude reads of the user's setup. Its setting sources also
/// decide which CLAUDE.md files, skills, agents, commands and plugins load; MCP
/// servers come from their own files, so only the run's `--mcp-config` stays. A
/// read-only run already reads no setting sources or MCP servers.
fn setup_args(s: Setup) -> Vec<String> {
    let sources = match s {
        Setup::UserAndProject => return vec![],
        Setup::Project => "--setting-sources=project,local",
        Setup::Isolated => "--setting-sources=",
    };
    vec![sources.into(), "--strict-mcp-config".into()]
}

/// Env turning off claude's auto memory, kept under the user's home, for any
/// narrowed `Setup` (the setting sources don't cover it).
fn setup_env(s: Setup) -> Option<(&'static str, &'static str)> {
    (s != Setup::UserAndProject).then_some(("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"))
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
            cmd.envs(opts.env.iter().copied());
            // Current models get no checklist tool unless asked; tasks off picks
            // `TodoWrite` (one call, whole list) over the TaskCreate/TaskUpdate set.
            cmd.env("CLAUDE_CODE_ENABLE_TODO_TOOLS", "1")
                .env("CLAUDE_CODE_ENABLE_TASKS", "0");
            cmd.envs(setup_env(opts.setup));
            // stderr inherits so it can't fill a pipe and block; kill_on_drop so a
            // dropped task can't leave claude running headless.
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            let mut child = cmd.spawn().context("spawning claude")?;
            // Declared after `child`, so it drops first, while the tree still hangs off it.
            let mut tree = crate::Tree::new(child.id());
            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            let mut finished = false;
            let mut parser = Parser::default();
            while let Some(line) = lines.next_line().await? {
                for ev in parser.line(&line) {
                    finished |= matches!(ev, AgentEvent::Finished { .. });
                    // Nobody listens: `tree` kills it all on return, while the tree is intact.
                    if tx.send(ev).await.is_err() {
                        return Ok(());
                    }
                }
            }
            let status = child.wait().await;
            tree.disarm();
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

    /// A real `--include-partial-messages` run (haiku: a sentence, a Bash call, a
    /// sentence; ids zeroed): each text block streams as deltas, then its `assistant`
    /// line repeats it whole. Tool input, thinking and signature deltas aren't text.
    #[test]
    fn partial_messages_stream_each_text_block_before_it_lands() {
        let mut p = Parser::default();
        let fixture = include_str!("../tests/fixtures/claude-partial.jsonl");
        let got: Vec<String> = fixture
            .lines()
            .flat_map(|l| p.line(l))
            .map(|e| match e {
                AgentEvent::TextDelta { text } => format!("+{text}"),
                AgentEvent::Text { text } => format!("={text}"),
                AgentEvent::ToolUse { name, .. } => format!("tool {name}"),
                AgentEvent::ToolResult { content } => format!("result {content}"),
                AgentEvent::Started { .. } => "started".into(),
                AgentEvent::Usage { .. } => "usage".into(),
                AgentEvent::Finished { ok, .. } => format!("finished {ok}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            [
                "started",
                "+I",
                "+ will",
                "+ run `",
                "+echo probe-ok`",
                "+ now.",
                "usage",
                "=I will run `echo probe-ok` now.",
                "tool Bash",
                "result probe-ok",
                "usage",
                "+The command",
                "+ print",
                "+ed `probe-ok`",
                "+.",
                "=The command printed `probe-ok`.",
                "usage",
                "finished true",
            ]
        );
        // A subagent's text streams under its Task call: not the reply.
        let sub = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}},"parent_tool_use_id":"toolu_1"}"#;
        assert!(parse_line(sub).is_empty());
    }

    /// A real run (haiku, `CLAUDE_CODE_ENABLE_TODO_TOOLS=1 CLAUDE_CODE_ENABLE_TASKS=0`,
    /// told to plan 3 steps and tick them off; ids zeroed, deltas dropped): each
    /// `TodoWrite` is a whole plan; its lookup and results are hidden.
    #[test]
    fn todo_write_calls_become_plans() {
        use PlanStatus::*;
        let mut p = Parser::default();
        let fixture = include_str!("../tests/fixtures/claude-todowrite.jsonl");
        let got: Vec<String> = fixture
            .lines()
            .flat_map(|l| p.line(l))
            .filter_map(|e| match e {
                AgentEvent::Plan { items } => Some(
                    (items.iter())
                        .map(|i| match i.status {
                            Pending => "○",
                            InProgress => "◐",
                            Completed => "✓",
                        })
                        .collect(),
                ),
                AgentEvent::ToolUse { name, .. } => Some(format!("tool {name}")),
                AgentEvent::ToolResult { content } => Some(format!("result {content}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            [
                "◐○○",
                "tool Bash",
                "result one",
                "✓◐○",
                "tool Bash",
                "result two",
                "✓✓◐",
                "✓✓✓",
            ]
        );
        let first = fixture.lines().flat_map(parse_line).find_map(|e| match e {
            AgentEvent::Plan { items } => Some(items),
            _ => None,
        });
        assert_eq!(
            first.unwrap()[0],
            PlanItem {
                text: "Run 'echo one'".into(),
                status: InProgress,
                active_form: Some("Running 'echo one'".into()),
            }
        );
        // A subagent's list isn't the task's; a malformed one stays a tool call.
        let call = |parent: &str, status: &str| {
            format!(
                r#"{{"type":"assistant","parent_tool_use_id":{parent},"message":{{"content":[{{"type":"tool_use","id":"t","name":"TodoWrite","input":{{"todos":[{{"content":"a","status":"{status}"}}]}}}}]}}}}"#
            )
        };
        let tool = |l: &str| matches!(&parse_line(l)[..], [AgentEvent::ToolUse { .. }]);
        assert!(tool(&call(r#""toolu_1""#, "pending")));
        assert!(tool(&call("null", "blocked")));
        let plan = parse_line(&call("null", "pending"));
        let [AgentEvent::Plan { items }] = &plan[..] else {
            panic!("{plan:?}")
        };
        assert_eq!((items.len(), items[0].active_form.as_ref()), (1, None));
    }

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
    fn embedder_tools_on_the_mis_server_show_and_own_ones_stay_hidden() {
        let call = |id: &str, name: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}]}}}}"#
            )
        };
        let result = |id: &str, text: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"{text}"}}]}}}}"#
            )
        };
        let mut p = Parser::default();
        for tool in ["report_status", "report_result", "approve", "merge_pr"] {
            let name = format!("mcp__mis__{tool}");
            assert!(p.line(&call("own", &name)).is_empty(), "{tool}");
            assert!(p.line(&result("own", "ok")).is_empty(), "{tool}");
        }
        // Interleaved: a hidden report call between a visible call and its result.
        let shown = AgentEvent::ToolUse {
            name: "mcp__mis__capture".into(),
            input: serde_json::json!({}),
        };
        assert_eq!(p.line(&call("e1", "mcp__mis__capture")), [shown]);
        assert!(p.line(&call("r1", "mcp__mis__report_evidence")).is_empty());
        assert!(p.line(&result("r1", "ok")).is_empty());
        assert_eq!(
            p.line(&result("e1", "frame.png")),
            [AgentEvent::ToolResult {
                content: "frame.png".into()
            }]
        );
        // Looking up an embedder tool is a lookup like any other.
        let lookup = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"s1","name":"ToolSearch","input":{"query":"select:mcp__mis__capture"}}]}}"#;
        assert_eq!(p.line(lookup).len(), 1);
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
                "--include-partial-messages",
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
        // Messaging the session itself: no fork; a copy: forked.
        let resume = |fork| {
            let o = RunOpts {
                resume: Some("s-1"),
                fork: Some(fork),
                ..Default::default()
            };
            let a = ClaudeAgent::default().argv("hi", &o);
            a[a.len() - 4..].to_vec()
        };
        assert_eq!(
            resume(false),
            ["--include-partial-messages", "--resume=s-1", "--", "hi"]
        );
        assert_eq!(resume(true), ["--resume=s-1", "--fork-session", "--", "hi"]);
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
            a[7..],
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
        assert_eq!(a[7..], ["--allowedTools", "mcp__mis", "--", "hi"]);
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
    fn read_only_denies_all_but_reads_and_never_widens_the_workers_rules() {
        let mcp = crate::Mcp {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "t".into(),
        };
        let argv = |p: &Permissions| {
            let o = RunOpts {
                mcp: Some(&mcp),
                permissions: Some(p),
                read_only: true,
                ..Default::default()
            };
            ClaudeAgent::default().argv("hi", &o)
        };
        let allowed = |a: &[String]| -> Vec<String> {
            let i = a.iter().position(|x| x == "--allowedTools").unwrap();
            a[i + 1..a.len() - 2].to_vec()
        };
        // Full: no bypass, but every GitHub read.
        let full = Permissions {
            mode: PermissionMode::Full,
            ..Default::default()
        };
        let a = argv(&full);
        assert!(!a.iter().any(|x| x.contains("dangerously")));
        let isolated = ["dontAsk", "--setting-sources=", "--strict-mcp-config"];
        assert_eq!(a[8..11], isolated);
        assert_eq!(a[12], READ_ONLY_NOTE);
        let all = allowed(&a);
        assert_eq!(all.len(), 1 + GH_READS.len());
        assert!(all.contains(&"Bash(gh pr list *)".into()) && all[0] == "mcp__mis");
        // Ask with `gh pr` and python3 allowed: only the `gh pr` reads, in its sandbox;
        // no python3, no blender, no asking.
        let ask = Permissions {
            mode: PermissionMode::Ask,
            folders: vec!["/u/game".into()],
            mcp: vec!["blender".into()],
            commands: vec!["gh pr".into(), "python3".into()],
        };
        let a = argv(&ask);
        assert_eq!(a[8..11], isolated);
        assert_eq!(a[13..17], ["--settings", SANDBOX, "--add-dir", "/u/game"]);
        assert!(!a.iter().any(|x| x == "--permission-prompt-tool"));
        let got = allowed(&a);
        assert!(got
            .iter()
            .all(|r| r == "mcp__mis" || r.starts_with("Bash(gh pr ")));
        assert_eq!(got.len(), 6, "{got:?}");
        // Nothing covering gh: no gh at all.
        let none = Permissions {
            commands: vec!["ghx".into()],
            ..ask
        };
        assert_eq!(allowed(&argv(&none)), ["mcp__mis"]);
    }

    #[test]
    fn setup_narrows_what_is_read_and_keeps_permissions_and_our_server() {
        let mcp = crate::Mcp {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "t".into(),
        };
        let p = Permissions {
            mode: PermissionMode::Folders,
            folders: vec!["/u/game".into()],
            ..Default::default()
        };
        let argv = |setup, read_only| {
            let o = RunOpts {
                mcp: Some(&mcp),
                permissions: Some(&p),
                read_only,
                setup,
                ..Default::default()
            };
            ClaudeAgent::default().argv("hi", &o)
        };
        let user = argv(Setup::UserAndProject, false);
        assert!(!user.iter().any(|a| a.starts_with("--setting-sources")));
        assert!(!user.contains(&"--strict-mcp-config".into()));
        assert_eq!(setup_env(Setup::UserAndProject), None);
        for (setup, sources) in [
            (Setup::Project, "--setting-sources=project,local"),
            (Setup::Isolated, "--setting-sources="),
        ] {
            let mut a = argv(setup, false);
            let i = a.iter().position(|x| x == sources).unwrap();
            assert_eq!(a[i + 1], "--strict-mcp-config", "{setup:?}");
            // The rest is as the caller set it: our server, the folders' rules.
            a.drain(i..i + 2);
            assert_eq!(a, user);
            assert_eq!(
                setup_env(setup),
                Some(("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"))
            );
            // Read-only already reads no setting sources: no second set of flags.
            assert_eq!(argv(setup, true), argv(Setup::UserAndProject, true));
        }
    }

    #[test]
    fn live_routine_runs_deny_merging_around_merge_pr_even_in_full_mode() {
        let full = Permissions {
            mode: PermissionMode::Full,
            ..Default::default()
        };
        let o = RunOpts {
            permissions: Some(&full),
            no_merge: true,
            ..Default::default()
        };
        let a = ClaudeAgent::default().argv("hi", &o);
        let i = a.iter().position(|x| x == "--disallowedTools").unwrap();
        assert_eq!(a[i + 1..i + 1 + NO_MERGE.len()], NO_MERGE.map(String::from));
        assert!(NO_MERGE.contains(&"Bash(*gh pr merge*)"));
        assert_eq!(a[a.len() - 2..], ["--", "hi"]);
        let plain = RunOpts {
            no_merge: false,
            ..o
        };
        assert!(!ClaudeAgent::default()
            .argv("hi", &plain)
            .contains(&"--disallowedTools".into()));
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
