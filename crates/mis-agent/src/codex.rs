//! Codex CLI backend: `codex exec --json <prompt>`.

use anyhow::Context;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use mis_types::{AgentEvent, LimitWindow, TaskId, TASK_ENV};
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

/// Limit windows from the tail of a session rollout (JSONL): the last
/// `token_count` event with a `rate_limits` window, its `primary` and `secondary`
/// windows observed at the event's `timestamp` (else `fallback_at`).
/// Unparsable lines (a cut first line) are skipped.
pub fn rollout_limits(tail: &str, fallback_at: u64) -> Option<Vec<LimitWindow>> {
    tail.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        let p = &v["payload"];
        if v["type"] != "event_msg" || p["type"] != "token_count" {
            return None;
        }
        let at = v["timestamp"]
            .as_str()
            .and_then(utc_ms)
            .unwrap_or(fallback_at);
        let windows: Vec<_> = ["primary", "secondary"]
            .iter()
            .filter_map(|k| window(&p["rate_limits"][k], at))
            .collect();
        (!windows.is_empty()).then_some(windows)
    })
}

/// One `rate_limits` window. Older codex versions give `resets_in_seconds`
/// rather than `resets_at`.
fn window(w: &Value, observed_at: u64) -> Option<LimitWindow> {
    let name = match w["window_minutes"].as_u64() {
        Some(300) => "5h".to_string(),
        Some(10080) => "7d".to_string(),
        Some(n) => format!("{n}m"),
        None => return None,
    };
    let resets_at = w["resets_at"].as_u64().map(|s| s * 1000);
    let resets_in = w["resets_in_seconds"]
        .as_u64()
        .map(|s| observed_at + s * 1000);
    Some(LimitWindow {
        name,
        used_pct: w["used_percent"].as_f64()?.clamp(0.0, 100.0) as f32,
        resets_at: resets_at.or(resets_in),
        observed_at,
    })
}

/// Unix millis of a UTC timestamp like `2026-10-28T19:41:03.790Z`.
fn utc_ms(s: &str) -> Option<u64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let num = |p: &str| p.parse::<i64>().ok();
    let d: Vec<i64> = date.split('-').map(num).collect::<Option<_>>()?;
    let t: Vec<i64> = hms.split(':').map(num).collect::<Option<_>>()?;
    let (&[y, m, day], &[h, mi, sec]) = (&d[..], &t[..]) else {
        return None;
    };
    let ms: String = frac.chars().chain("000".chars()).take(3).collect();
    // Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let secs = ((days * 24 + h) * 60 + mi) * 60 + sec;
    u64::try_from(secs * 1000 + num(&ms)?).ok()
}

/// Bytes read from the end of a rollout: most tails hold a `token_count` within
/// the first size; a run of big tool outputs can push it further back.
const TAIL_BYTES: [u64; 2] = [64 * 1024, 1024 * 1024];

/// The most recently written `rollout-*.jsonl` under `sessions/YYYY/MM/DD`, from
/// the newest two days (a session that crosses midnight stays in its first day's dir).
fn newest_rollout(sessions: &Path) -> Option<PathBuf> {
    let newest_first = |dir: &Path| {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| Some(e.ok()?.path()))
            .collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };
    let days = newest_first(sessions)
        .into_iter()
        .filter(|p| p.is_dir())
        .flat_map(|y| newest_first(&y).into_iter().filter(|p| p.is_dir()))
        .flat_map(|m| newest_first(&m).into_iter().filter(|p| p.is_dir()))
        .take(2);
    days.flat_map(|d| newest_first(&d))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("rollout-") && name.ends_with(".jsonl")
        })
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p)
}

/// `CODEX_HOME`, default `~/.codex`.
fn codex_home() -> Result<PathBuf, String> {
    std::env::var_os("CODEX_HOME")
        .map_or_else(|| crate::home().map(|h| h.join(".codex")), |d| Ok(d.into()))
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
    /// `exec resume [opts] -- <session> <prompt>` to continue a session. Codex
    /// can't fork one, so replies to the same task share a single history.
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
        let models = codex_home().and_then(|dir| {
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

    /// From the newest session rollout, so interactive sessions on this account
    /// count too.
    fn limits(&self) -> Option<Vec<LimitWindow>> {
        let path = newest_rollout(&codex_home().ok()?.join("sessions"))?;
        let mut f = std::fs::File::open(&path).ok()?;
        let meta = f.metadata().ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        for size in TAIL_BYTES {
            f.seek(SeekFrom::Start(meta.len().saturating_sub(size)))
                .ok()?;
            let mut tail = Vec::new();
            f.read_to_end(&mut tail).ok()?;
            let found = rollout_limits(&String::from_utf8_lossy(&tail), mtime.as_millis() as u64);
            if found.is_some() || meta.len() <= size {
                return found;
            }
        }
        None
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

    /// Shaped like a live rollout tail: a cut first line, `token_count` with the
    /// `rate_limits` seen on this account (credits and usage numbers zeroed),
    /// then a later event.
    const ROLLOUT_TAIL: &str = concat!(
        r#"put":"cut mid-line"}}"#,
        "\n",
        r#"{"timestamp":"2026-10-28T19:41:03.790Z","ordinal":41,"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":100.0,"window_minutes":300,"resets_at":1793219109},"secondary":{"used_percent":16.0,"window_minutes":10080,"resets_at":1793805909},"credits":{"has_credits":true,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}"#,
        "\n",
        r#"{"timestamp":"2026-10-28T19:41:04.002Z","ordinal":42,"type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
        "\n",
    );

    #[test]
    fn rollout_limits_take_the_last_token_count() {
        let w = |name: &str, used_pct, resets_at, observed_at| LimitWindow {
            name: name.into(),
            used_pct,
            resets_at,
            observed_at,
        };
        // Observed when the event was written, not when the file last changed.
        const AT: u64 = 1_793_216_463_790;
        assert_eq!(
            rollout_limits(ROLLOUT_TAIL, 7),
            Some(vec![
                w("5h", 100.0, Some(1_793_219_109_000), AT),
                w("7d", 16.0, Some(1_793_805_909_000), AT)
            ])
        );
        // A later reading wins; other window lengths are named in minutes; a window
        // without a percentage is skipped; no timestamp falls back.
        let later = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":3.5,"window_minutes":90},"secondary":{"window_minutes":10080}}}}"#;
        assert_eq!(
            rollout_limits(&format!("{ROLLOUT_TAIL}{later}\n"), 7),
            Some(vec![w("90m", 3.5, None, 7)])
        );
        // Older codex: seconds until the reset.
        let old = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":50,"window_minutes":300,"resets_in_seconds":60}}}}"#;
        assert_eq!(
            rollout_limits(old, 7),
            Some(vec![w("5h", 50.0, Some(60_007), 7)])
        );
        // No usable window (`rate_limits: null`, or windows null) is no reading:
        // the one before it counts; nothing at all is None.
        let empty = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":null}}}"#;
        assert_eq!(
            rollout_limits(&format!("{ROLLOUT_TAIL}{empty}"), 7)
                .unwrap()
                .len(),
            2
        );
        let null = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":null}}"#;
        assert_eq!(
            rollout_limits(&format!("{ROLLOUT_TAIL}{null}"), 7)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(rollout_limits(null, 7), None);
        assert_eq!(rollout_limits("", 7), None);
    }

    #[test]
    fn utc_timestamps_parse_to_unix_ms() {
        assert_eq!(utc_ms("2026-10-28T19:41:03.790Z"), Some(1_793_216_463_790));
        assert_eq!(utc_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
        assert_eq!(
            utc_ms("2026-10-28T19:41:03.7901234Z"),
            Some(1_793_216_463_790)
        );
        for bad in [
            "",
            "2026-10-28 19:41:03Z",
            "2026-10-28T19:41:03+00:00",
            "2026-10-28T19:41Z",
            "x-y-zTa:b:cZ",
            "2026-10-28T19:41:03.éZ",
        ] {
            assert_eq!(utc_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn newest_rollout_is_the_latest_write_in_the_last_two_days() {
        let root = std::env::temp_dir().join(format!("mis-rollouts-{}", std::process::id()));
        // Each file written a minute after the last, whatever the filesystem's clock resolution.
        let t0 = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let n = std::cell::Cell::new(0);
        let write = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let f = std::fs::File::create(&p).unwrap();
            n.set(n.get() + 1);
            f.set_modified(t0 + std::time::Duration::from_secs(60 * n.get()))
                .unwrap();
            p
        };
        assert_eq!(newest_rollout(&root), None);
        write("2026/10/26/rollout-old.jsonl");
        write("2026/10/28/rollout-today.jsonl");
        // Written last, but in yesterday's dir: a session still running past midnight.
        let still = write("2026/10/27/rollout-yesterday.jsonl");
        write("2026/10/28/notes.txt");
        assert_eq!(newest_rollout(&root), Some(still));
        std::fs::remove_dir_all(&root).unwrap();
    }

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
