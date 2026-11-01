//! The last turns of one Claude or Codex session, parsed from a bounded tail of its file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use mis_types::{first_line, Answer, Role, Turn, MAX_TURNS};
use serde_json::Value;
use uuid::Uuid;

use crate::rules;
use crate::sessions::{active, feed, jsonl_at, mtime_ms, typed};

/// Bytes read from the end of the file; older turns are reported as `more`.
const WINDOW: u64 = 2 << 20;
/// A message's text, clipped.
const TEXT_CHARS: usize = 2000;
/// A tool call or result: one line.
const TOOL_CHARS: usize = 160;
/// Total text sent; oldest turns go first.
const MAX_BYTES: usize = 256 << 10;

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

fn turn(role: Role, text: String, at: &Value) -> Turn {
    let at = at.as_str().map(String::from);
    Turn { role, text, at }
}

/// A Codex call: `shell: bash -lc ls`, `apply_patch: *** Begin Patch`; else as the approval cards say it.
fn codex_call(name: &str, input: &Value) -> String {
    let arg = match (input, &input["cmd"]) {
        (Value::String(s), _) | (_, Value::String(s)) => s.clone(),
        (_, Value::Array(a)) => {
            let words: Vec<_> = a.iter().filter_map(Value::as_str).collect();
            words.join(" ")
        }
        _ => return rules::summary(name, input, None),
    };
    first_line(&format!("{name}: {arg}"), TOOL_CHARS)
}

/// `→ first line` of a tool's output; `✗` when it failed.
fn result(text: &str, error: bool) -> String {
    let mark = if error { "✗" } else { "→" };
    first_line(&format!("{mark} {}", text.trim_start()), TOOL_CHARS)
}

/// Text the user typed, or any the agent wrote; None for injected or blank text.
fn said(role: Role, s: &str) -> Option<String> {
    match role {
        Role::User => typed(s),
        _ => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
    }
    .map(|s| clip(&s, TEXT_CHARS))
}

/// Turns on one line of a Claude transcript: typed prompts, replies, tool calls and results.
/// Meta, sidechain, compaction-summary, injected and thinking content is left out.
pub fn claude_turns(line: &str) -> Vec<Turn> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let role = match v["type"].as_str() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return vec![],
    };
    if v["isMeta"] == true || v["isSidechain"] == true || v["isCompactSummary"] == true {
        return vec![];
    }
    let at = &v["timestamp"];
    let blocks = match &v["message"]["content"] {
        Value::String(s) => {
            return said(role, s)
                .map(|s| turn(role, s, at))
                .into_iter()
                .collect()
        }
        Value::Array(blocks) => blocks,
        _ => return vec![],
    };
    blocks
        .iter()
        .filter_map(|b| match b["type"].as_str()? {
            "text" => said(role, b["text"].as_str()?).map(|s| turn(role, s, at)),
            "tool_use" => {
                let home = std::env::home_dir();
                let call = rules::summary(b["name"].as_str()?, &b["input"], home.as_deref());
                Some(turn(Role::Tool, call, at))
            }
            "tool_result" => {
                let out = mis_agent::claude::tool_result_text(&b["content"]);
                Some(turn(Role::Tool, result(&out, b["is_error"] == true), at))
            }
            _ => None,
        })
        .collect()
}

/// Turns on one line of a Codex rollout. Developer messages, injected context and
/// reasoning are left out.
pub fn codex_turns(line: &str) -> Vec<Turn> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let (p, at) = (&v["payload"], &v["timestamp"]);
    if v["type"] != "response_item" {
        return vec![];
    }
    match p["type"].as_str() {
        Some("message") => {
            let (role, kind) = match p["role"].as_str() {
                Some("user") => (Role::User, "input_text"),
                Some("assistant") => (Role::Assistant, "output_text"),
                _ => return vec![],
            };
            let parts = p["content"].as_array().into_iter().flatten();
            parts
                .filter(|c| c["type"] == kind)
                .filter_map(|c| said(role, c["text"].as_str()?))
                .map(|s| turn(role, s, at))
                .collect()
        }
        Some("function_call") => {
            // `arguments` is JSON in a string.
            let args = p["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str(a).ok());
            let name = p["name"].as_str().unwrap_or("tool");
            vec![turn(
                Role::Tool,
                codex_call(name, &args.unwrap_or(Value::Null)),
                at,
            )]
        }
        Some("custom_tool_call") => {
            let name = p["name"].as_str().unwrap_or("tool");
            vec![turn(Role::Tool, codex_call(name, &p["input"]), at)]
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let out = mis_agent::claude::tool_result_text(&p["output"]);
            // Scripts report status first: `Script completed\nWall time 6.6 seconds\nOutput:\n…`.
            let body = out.split_once("Output:\n").map(|(_, b)| b);
            let body = body.filter(|b| !b.trim().is_empty()).unwrap_or(&out);
            vec![turn(
                Role::Tool,
                result(body, out.starts_with("Script failed")),
                at,
            )]
        }
        _ => vec![],
    }
}

/// The last `limit` turns, then fewer until their text fits `MAX_BYTES`. True: some were dropped.
pub fn last(mut turns: Vec<Turn>, limit: usize) -> (Vec<Turn>, bool) {
    let total = turns.len();
    turns.drain(..total.saturating_sub(limit.min(MAX_TURNS)));
    let mut bytes: usize = turns.iter().map(|t| t.text.len()).sum();
    let mut cut = 0;
    while bytes > MAX_BYTES {
        bytes -= turns[cut].text.len();
        cut += 1;
    }
    turns.drain(..cut);
    let dropped = total > turns.len();
    (turns, dropped)
}

/// The session's file: `<claude>/projects/*/<id>.jsonl` or
/// `<codex>/sessions/Y/M/D/rollout-*-<id>.jsonl`. Paths are built from a parsed
/// UUID, so they can't leave the roots.
fn find(claude: &Path, codex: &Path, harness: &str, id: Uuid) -> Option<PathBuf> {
    match harness {
        "claude" => {
            let name = format!("{id}.jsonl");
            let projects = std::fs::read_dir(claude.join("projects")).ok()?;
            projects
                .flatten()
                .map(|d| d.path().join(&name))
                .find(|p| p.is_file())
        }
        "codex" => {
            let suffix = format!("-{id}.jsonl");
            let files = jsonl_at(&codex.join("sessions"), 3).into_iter();
            files.map(|(_, p)| p).find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
            })
        }
        _ => None,
    }
}

/// Answer `Query::ReadSession`: its last turns from up to `WINDOW` bytes at the end of its file.
pub fn read(
    claude: &Path,
    codex: &Path,
    harness: &str,
    id: Uuid,
    limit: usize,
    now_ms: u64,
) -> Answer {
    let Some(path) = find(claude, codex, harness, id) else {
        return Answer::NotFound;
    };
    let Ok(mut file) = File::open(&path) else {
        return Answer::NotFound;
    };
    let Ok(meta) = file.metadata() else {
        return Answer::NotFound;
    };
    let len = meta.len();
    let from = len.saturating_sub(WINDOW);
    let parse = if harness == "claude" {
        claude_turns
    } else {
        codex_turns
    };
    let mut turns = vec![];
    if file.seek(SeekFrom::Start(from)).is_ok() {
        // Up to the size seen: a live session may be appending. A cut first line is skipped.
        feed((&mut file).take(len - from), |l| {
            turns.extend(parse(l));
            false
        });
    }
    let (turns, dropped) = last(turns, limit);
    let updated_ms = mtime_ms(&meta);
    Answer::Transcript {
        turns,
        more: dropped || from > 0,
        updated_ms,
        active: active(updated_ms, now_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(turns: &[Turn]) -> Vec<(Role, &str)> {
        turns.iter().map(|t| (t.role, t.text.as_str())).collect()
    }

    fn all(parse: fn(&str) -> Vec<Turn>, lines: &[&str]) -> Vec<Turn> {
        lines.iter().flat_map(|l| parse(l)).collect()
    }

    #[test]
    fn claude_turns_skip_injected_and_collapse_tools() {
        let turns = all(
            claude_turns,
            &[
                r#"{"type":"queue-operation","operation":"enqueue"}"#,
                r#"{"type":"user","isMeta":true,"message":{"content":"Caveat: local commands"}}"#,
                r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","timestamp":"2026-11-01T10:00:00Z","message":{"content":"fix the build"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#,
                r#"{"type":"user","isCompactSummary":true,"message":{"content":"This session is being continued"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo build","description":"Build"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"\nerror[E0308]: mismatched types\nmore"}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":[{"type":"text","text":"denied"}]}]}}"#,
                r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixed it."}]}}"#,
                r#"{"type":"system","subtype":"turn_duration"}"#,
                "cut off {\"type\":\"user",
            ],
        );
        assert_eq!(
            texts(&turns),
            [
                (Role::User, "fix the build"),
                (Role::Tool, "Bash: cargo build"),
                (Role::Tool, "→ error[E0308]: mismatched types"),
                (Role::Tool, "✗ denied"),
                (Role::Assistant, "Fixed it."),
            ]
        );
        assert_eq!(turns[0].at.as_deref(), Some("2026-11-01T10:00:00Z"));
    }

    #[test]
    fn codex_turns_skip_context_and_collapse_tools() {
        let turns = all(
            codex_turns,
            &[
                r#"{"type":"session_meta","payload":{"id":"i"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}"#,
                r##"{"type":"response_item","timestamp":"t1","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /r"},{"type":"input_text","text":"<environment_context></environment_context>"},{"type":"input_text","text":"rename the crate"}]}}"##,
                r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":[\"bash\",\"-lc\",\"ls\"]}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","output":"Cargo.toml\nsrc"}}"#,
                r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n..."}}"#,
                r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"Script completed\nWall time 1 seconds\nOutput:\n"},{"type":"input_text","text":"Done"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"Script failed\nOutput:\n"}]}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"dup"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed."}]}}"#,
            ],
        );
        assert_eq!(
            texts(&turns),
            [
                (Role::User, "rename the crate"),
                (Role::Tool, "shell: bash -lc ls"),
                (Role::Tool, "→ Cargo.toml"),
                (Role::Tool, "apply_patch: *** Begin Patch"),
                (Role::Tool, "→ Done"),
                (Role::Tool, "✗ Script failed"),
                (Role::Assistant, "Renamed."),
            ]
        );
        assert_eq!(turns[0].at.as_deref(), Some("t1"));
    }

    #[test]
    fn long_texts_and_totals_are_clipped() {
        let long = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "é".repeat(5000)
        );
        let t = &claude_turns(&long)[0];
        assert_eq!(t.text.chars().count(), TEXT_CHARS + 1);
        assert!(t.text.ends_with('…'));
        let call = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Grep","input":{{"pattern":"{}"}}}}]}}}}"#,
            "a".repeat(500)
        );
        assert_eq!(claude_turns(&call)[0].text.chars().count(), TOOL_CHARS);

        let turn = |n: usize| Turn {
            role: Role::User,
            text: "x".repeat(n),
            at: None,
        };
        // The last `limit`, oldest first.
        let (kept, more) = last((1..=5).map(turn).collect(), 2);
        assert_eq!(
            kept.iter().map(|t| t.text.len()).collect::<Vec<_>>(),
            [4, 5]
        );
        assert!(more);
        assert!(!last(vec![turn(1)], 30).1);
        // Then the oldest go until the text fits.
        let (kept, more) = last(vec![turn(MAX_BYTES), turn(10), turn(MAX_BYTES - 10)], 30);
        assert_eq!(kept.len(), 2);
        assert!(more);
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mis-transcript-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_a_session_by_harness_and_id() {
        let root = temp();
        let (claude, codex) = (root.join("claude"), root.join("codex"));
        let id = Uuid::new_v4();
        let proj = claude.join("projects").join("-w");
        std::fs::create_dir_all(&proj).unwrap();
        let line = r#"{"type":"user","message":{"content":"hello"}}"#;
        std::fs::write(proj.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
        let day = codex.join("sessions").join("2026").join("11").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let line = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#;
        std::fs::write(
            day.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl")),
            line,
        )
        .unwrap();

        let Answer::Transcript {
            turns,
            more,
            active,
            ..
        } = read(&claude, &codex, "claude", id, 30, 0)
        else {
            panic!("claude session")
        };
        assert_eq!(texts(&turns), [(Role::User, "hello")]);
        assert!(!more && active);
        let Answer::Transcript { turns, .. } = read(&claude, &codex, "codex", id, 30, 0) else {
            panic!("codex session")
        };
        assert_eq!(texts(&turns), [(Role::Assistant, "hi")]);
        let other = Uuid::new_v4();
        assert_eq!(
            read(&claude, &codex, "claude", other, 30, 0),
            Answer::NotFound
        );
        assert_eq!(
            read(&claude, &codex, "opencode", id, 30, 0),
            Answer::NotFound
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
