//! Claude and Codex sessions on this machine, from the harnesses' own transcript
//! files: `<claude>/projects/<slug>/<id>.jsonl`, `<codex>/sessions/Y/M/D/rollout-*.jsonl`.
//! Only a bounded head and tail of each file is read; transcripts can be huge.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use mis_types::{first_line, Session};
use serde_json::Value;

/// Most sessions listed.
pub const MAX_SESSIONS: usize = 50;
/// Codex writes ~150 KB of instructions (more with images) before the first prompt.
const HEAD_BYTES: u64 = 1 << 20;
/// Claude appends its generated titles as the session goes on.
const TAIL_BYTES: u64 = 64 << 10;
/// Written this recently: likely still running.
const ACTIVE_MS: u64 = 2 * 60 * 1000;
const TITLE_CHARS: usize = 120;

/// What one file's lines say so far; fed head lines, then tail lines.
#[derive(Debug, Default)]
pub struct Head {
    pub id: String,
    cwd: Option<String>,
    /// First real user prompt.
    prompt: Option<String>,
    /// A title the user set (`/rename`), else one the harness generated; latest wins.
    custom_title: Option<String>,
    auto_title: Option<String>,
    /// Latest seen.
    model: Option<String>,
    headless: bool,
    /// Not worth listing (a Codex subagent's thread).
    skip: bool,
}

impl Head {
    /// Seeded with the id the file name gives.
    pub fn new(id: String) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    /// The head has everything it needs; the tail can still add titles.
    pub fn done(&self) -> bool {
        self.skip || (self.cwd.is_some() && self.prompt.is_some() && self.model.is_some())
    }

    /// None: nothing to list (no cwd and no title, or skipped).
    pub fn finish(self, harness: &str, updated_ms: u64, now_ms: u64) -> Option<Session> {
        let title = self.custom_title.or(self.auto_title).or(self.prompt);
        if self.skip || (self.cwd.is_none() && title.is_none()) {
            return None;
        }
        Some(Session {
            harness: harness.into(),
            id: self.id,
            cwd: self.cwd,
            title: title.map(|t| first_line(&t, TITLE_CHARS)),
            model: self.model,
            updated_ms,
            active: now_ms.saturating_sub(updated_ms) < ACTIVE_MS,
            interactive: !self.headless,
        })
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Text the user typed, not what the harness injected under the user's role:
/// `<command-name>`, `<system-reminder>`, `<environment_context>`, AGENTS.md, interrupts.
fn typed(s: &str) -> Option<String> {
    let s = s.trim();
    let injected = ["<", "[Request interrupted", "# AGENTS.md instructions"];
    (!s.is_empty() && !injected.iter().any(|p| s.starts_with(p))).then(|| s.to_string())
}

/// One line of a Claude transcript. Unparseable lines (a cut-off last one) are skipped.
pub fn claude_line(h: &mut Head, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("custom-title") => h.custom_title = text(&v["customTitle"]).or(h.custom_title.take()),
        Some("ai-title") => h.auto_title = text(&v["aiTitle"]).or(h.auto_title.take()),
        // Titles of older versions.
        Some("summary") => h.auto_title = text(&v["summary"]).or(h.auto_title.take()),
        Some(kind @ ("user" | "assistant")) => {
            if v["isSidechain"] == true {
                return;
            }
            if h.cwd.is_none() {
                h.cwd = text(&v["cwd"]);
            }
            // `cli`; `sdk-cli` and the like for `claude -p` and SDK runs.
            if let Some(e) = v["entrypoint"].as_str() {
                h.headless = e.starts_with("sdk");
            }
            let msg = &v["message"];
            if kind == "assistant" {
                // `<synthetic>` marks messages claude made up itself (errors, interrupts).
                if let Some(m) = text(&msg["model"]).filter(|m| !m.starts_with('<')) {
                    h.model = Some(m);
                }
            } else if h.prompt.is_none() && v["isMeta"] != true {
                h.prompt = match &msg["content"] {
                    Value::String(s) => typed(s),
                    // The first text block; tool results come as other block types.
                    Value::Array(blocks) => blocks
                        .iter()
                        .find(|b| b["type"] == "text")
                        .and_then(|b| b["text"].as_str())
                        .and_then(typed),
                    _ => None,
                };
            }
        }
        _ => {}
    }
}

/// One line of a Codex rollout.
pub fn codex_line(h: &mut Head, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let p = &v["payload"];
    match v["type"].as_str() {
        Some("session_meta") => {
            if let Some(id) = text(&p["id"]) {
                h.id = id;
            }
            h.cwd = text(&p["cwd"]).or(h.cwd.take());
            h.headless = p["originator"] == "codex_exec";
            h.skip = p["source"].get("subagent").is_some();
        }
        Some("turn_context") => h.model = text(&p["model"]).or(h.model.take()),
        // One user message holds several inputs: AGENTS.md, environment, then the prompt.
        Some("response_item")
            if p["type"] == "message" && p["role"] == "user" && h.prompt.is_none() =>
        {
            let inputs = p["content"].as_array().into_iter().flatten();
            h.prompt = inputs
                .filter(|c| c["type"] == "input_text")
                .find_map(|c| c["text"].as_str().and_then(typed));
        }
        _ => {}
    }
}

/// Feed `r`'s lines to `f` until it's exhausted or `f` says stop; returns bytes consumed.
fn feed(r: impl Read, mut f: impl FnMut(&str) -> bool) -> u64 {
    let mut r = BufReader::new(r);
    let (mut buf, mut used) = (Vec::new(), 0);
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return used,
            Ok(n) => used += n as u64,
        }
        if f(&String::from_utf8_lossy(&buf)) {
            return used;
        }
    }
}

/// One session file: up to `head` bytes from its start, then its last `tail` bytes
/// unless the head already covered them.
pub fn read_file(
    harness: &str,
    path: &Path,
    (head, tail): (u64, u64),
    updated_ms: u64,
    now_ms: u64,
) -> Option<Session> {
    let line = match harness {
        "claude" => claude_line,
        _ => codex_line,
    };
    let stem = path.file_stem()?.to_string_lossy();
    // Codex names files `rollout-<time>-<id>`; session_meta has the id too.
    let id = match harness {
        "claude" => stem.to_string(),
        _ => stem.get(stem.len().saturating_sub(36)..)?.to_string(),
    };
    let mut h = Head::new(id);
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let used = feed((&mut file).take(head), |l| {
        line(&mut h, l);
        h.done()
    });
    let from = used.max(len.saturating_sub(tail));
    if from < len && file.seek(SeekFrom::Start(from)).is_ok() {
        // A first line cut mid-way fails to parse and is skipped.
        feed(&mut file, |l| {
            line(&mut h, l);
            false
        });
    }
    h.finish(harness, updated_ms, now_ms)
}

/// `*.jsonl` files exactly `depth` directories below `dir`, with their mtimes.
fn jsonl_at(dir: &Path, depth: usize) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let path = e.path();
        if depth > 0 && meta.is_dir() {
            out.extend(jsonl_at(&path, depth - 1));
        } else if depth == 0 && meta.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
            out.push((mtime_ms(&meta), path));
        }
    }
    out
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    let since = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    since.map_or(0, |d| d.as_millis() as u64)
}

/// Where the harnesses keep sessions: `CLAUDE_CONFIG_DIR` (default `~/.claude`) and
/// `CODEX_HOME` (default `~/.codex`), as they look them up.
pub fn roots() -> (PathBuf, PathBuf) {
    let home = std::env::home_dir().unwrap_or_default();
    let dir = |var, default| std::env::var_os(var).map_or(home.join(default), PathBuf::from);
    (
        dir("CLAUDE_CONFIG_DIR", ".claude"),
        dir("CODEX_HOME", ".codex"),
    )
}

/// The newest `MAX_SESSIONS` sessions under both roots, newest first. Claude
/// subagents live a level deeper (`<id>/subagents/`) and aren't listed.
pub fn list(claude: &Path, codex: &Path, now_ms: u64) -> Vec<Session> {
    let claude = jsonl_at(&claude.join("projects"), 1)
        .into_iter()
        .map(|f| ("claude", f));
    let codex = jsonl_at(&codex.join("sessions"), 3)
        .into_iter()
        .map(|f| ("codex", f));
    let mut files: Vec<_> = claude.chain(codex).collect();
    files.sort_by_key(|(_, (at, _))| std::cmp::Reverse(*at));
    files
        .into_iter()
        .filter_map(|(h, (at, p))| read_file(h, &p, (HEAD_BYTES, TAIL_BYTES), at, now_ms))
        .take(MAX_SESSIONS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(lines: &[&str]) -> Head {
        let mut h = Head::new("c1".into());
        for l in lines {
            claude_line(&mut h, l);
        }
        h
    }

    fn codex(lines: &[&str]) -> Head {
        let mut h = Head::new("x1".into());
        for l in lines {
            codex_line(&mut h, l);
        }
        h
    }

    #[test]
    fn claude_title_skips_injected_and_tool_results() {
        let h = claude(&[
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"user","isMeta":true,"cwd":"/w","message":{"content":"Caveat: local commands"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"subagent prompt"}}"#,
            r#"{"type":"user","entrypoint":"cli","message":{"content":[{"type":"text","text":"  fix the build\nthen test"}]}}"#,
            r#"{"type":"assistant","message":{"model":"<synthetic>"}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus"}}"#,
            r#"{"type":"user","message":{"content":"later prompt"}}"#,
        ]);
        let s = h.finish("claude", 1_000, 1_000 + ACTIVE_MS).unwrap();
        assert_eq!(s.title.as_deref(), Some("fix the build"));
        assert_eq!(s.cwd.as_deref(), Some("/w"));
        assert_eq!(s.model.as_deref(), Some("claude-opus"));
        assert_eq!(
            (s.id.as_str(), s.interactive, s.active),
            ("c1", true, false)
        );
    }

    #[test]
    fn claude_named_titles_beat_the_prompt_and_latest_wins() {
        let prompt =
            r#"{"type":"user","cwd":"/w","entrypoint":"sdk-cli","message":{"content":"p"}}"#;
        let h = claude(&[prompt, r#"{"type":"ai-title","aiTitle":"First"}"#]);
        assert_eq!(
            h.finish("claude", 0, 0).unwrap().title.as_deref(),
            Some("First")
        );
        let h = claude(&[
            prompt,
            r#"{"type":"custom-title","customTitle":"Mine"}"#,
            r#"{"type":"ai-title","aiTitle":"Later"}"#,
        ]);
        let s = h.finish("claude", 0, 0).unwrap();
        assert_eq!((s.title.as_deref(), s.interactive), (Some("Mine"), false));
    }

    #[test]
    fn missing_fields_are_tolerated() {
        let h = claude(&[
            "not json",
            r#"{"type":"user"}"#,
            r#"{"type":"user","message":{"content":42}}"#,
            r#"{"type":"assistant","cwd":"/w"}"#,
        ]);
        let s = h.finish("claude", 5, 5).unwrap();
        assert_eq!(
            (s.cwd.as_deref(), s.title, s.model),
            (Some("/w"), None, None)
        );
        assert!(s.active && s.interactive);
        // Nothing to show: not listed.
        assert_eq!(claude(&[r#"{"type":"mode"}"#]).finish("claude", 0, 0), None);
        assert_eq!(
            codex(&[r#"{"type":"session_meta"}"#]).finish("codex", 0, 0),
            None
        );
    }

    #[test]
    fn codex_title_skips_agents_md_and_environment() {
        let h = codex(&[
            r#"{"type":"session_meta","payload":{"id":"real-id","cwd":"/r","originator":"codex-tui","source":"cli"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /r\n..."},{"type":"input_text","text":"<environment_context>\n</environment_context>"}]}}"##,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1]>"},{"type":"input_image","image_url":"data:"},{"type":"input_text","text":"rename the crate"}]}}"#,
        ]);
        assert!(h.done());
        let s = h.finish("codex", 0, 0).unwrap();
        assert_eq!(s.id, "real-id");
        assert_eq!(s.title.as_deref(), Some("rename the crate"));
        assert_eq!(
            (s.cwd.as_deref(), s.model.as_deref()),
            (Some("/r"), Some("gpt-5"))
        );
        assert!(s.interactive);
    }

    #[test]
    fn codex_exec_is_headless_and_subagents_are_skipped() {
        let meta = r#"{"type":"session_meta","payload":{"id":"i","cwd":"/r","originator":"codex_exec","source":"exec"}}"#;
        assert!(!codex(&[meta]).finish("codex", 0, 0).unwrap().interactive);
        let sub =
            r#"{"type":"session_meta","payload":{"id":"i","cwd":"/r","source":{"subagent":{}}}}"#;
        let h = codex(&[sub]);
        assert!(h.done());
        assert_eq!(h.finish("codex", 0, 0), None);
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mis-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_only_a_bounded_head_and_tail() {
        let dir = temp();
        let path = dir.join("c1.jsonl");
        let pad = format!("{{\"type\":\"pad\",\"x\":\"{}\"}}\n", "x".repeat(200));
        let prompt = r#"{"type":"user","cwd":"/w","message":{"content":"hidden"}}"#;
        let body = format!(
            "{pad}{prompt}\n{}{}\n",
            pad.repeat(10),
            r#"{"type":"ai-title","aiTitle":"Tail title"}"#
        );
        std::fs::write(&path, body).unwrap();
        // Head ends inside the padding, tail starts after the prompt: only the title is seen.
        let s = read_file("claude", &path, (100, 400), 0, 0).unwrap();
        assert_eq!((s.title.as_deref(), s.cwd), (Some("Tail title"), None));
        // A head long enough reaches the prompt and its cwd.
        let s = read_file("claude", &path, (1 << 20, 400), 0, 0).unwrap();
        assert_eq!(
            (s.title.as_deref(), s.cwd.as_deref()),
            (Some("Tail title"), Some("/w"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lists_both_roots_newest_first_without_subagents() {
        let (claude, codex) = (temp(), temp());
        let proj = claude.join("projects").join("-w");
        std::fs::create_dir_all(proj.join("c1").join("subagents")).unwrap();
        let line = r#"{"type":"user","cwd":"/w","message":{"content":"claude task"}}"#;
        std::fs::write(proj.join("c1.jsonl"), line).unwrap();
        std::fs::write(
            proj.join("c1").join("subagents").join("agent-a.jsonl"),
            line,
        )
        .unwrap();
        let day = codex.join("sessions").join("2026").join("11").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let id = "019a0000-0000-7000-8000-000000000001";
        let meta = r#"{"type":"session_meta","payload":{"cwd":"/r"}}"#;
        std::fs::write(
            day.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl")),
            meta,
        )
        .unwrap();
        // Make the codex file the newer one.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(proj.join("c1.jsonl"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let got = list(&claude, &codex, now);
        let ids: Vec<_> = got
            .iter()
            .map(|s| (s.harness.as_str(), s.id.as_str()))
            .collect();
        assert_eq!(ids, [("codex", id), ("claude", "c1")]);
        assert!(got[0].active && !got[1].active);
        assert_eq!(got[1].title.as_deref(), Some("claude task"));
        // Missing roots list nothing.
        assert!(list(&claude.join("none"), &codex.join("none"), now).is_empty());
        for d in [claude, codex] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }
}
