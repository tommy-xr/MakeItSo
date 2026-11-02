//! Claude and Codex sessions on this machine, from the harnesses' own transcript
//! files: `<claude>/projects/<slug>/<id>.jsonl`, `<codex>/sessions/Y/M/D/rollout-*.jsonl`.
//! Only a bounded head and tail of each file is read; transcripts can be huge.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use mis_types::{first_line, Delivered, Session, MAX_DELIVERED, PEER_NAME};

use crate::live;
use serde_json::Value;

/// Most sessions listed.
const MAX_SESSIONS: usize = 50;
/// Most files read for one list, so asking for interactive sessions only stays cheap
/// where headless runs dominate.
pub(crate) const MAX_SCANNED: usize = 1000;
/// Codex writes ~150 KB of instructions (more with images) before the first prompt.
pub(crate) const HEAD_BYTES: u64 = 1 << 20;
/// Claude appends its generated titles as the session goes on.
const TAIL_BYTES: u64 = 64 << 10;
/// Written this recently: likely still running.
const ACTIVE_MS: u64 = 2 * 60 * 1000;
const TITLE_CHARS: usize = 120;

/// What one file's lines say so far; fed head lines, then tail lines.
#[derive(Debug, Default)]
struct Head {
    id: String,
    cwd: Option<String>,
    /// First real user prompt.
    prompt: Option<String>,
    /// A title the user set (`/rename`), else one the harness generated; latest wins.
    custom_title: Option<String>,
    auto_title: Option<String>,
    /// Latest seen.
    model: Option<String>,
    /// How it was started: [`Head::headless`] prefers the first claude prompt's
    /// `turnOrigin` (copied prompts keep theirs), else the first entrypoint or originator
    /// seen, so a later `claude -p --resume` doesn't make an interactive session headless.
    origin: Option<bool>,
    entry: Option<bool>,
    /// The session a codex copy (`exec fork`) was made from; it records no copied history.
    forked_from: Option<String>,
    /// Not worth listing (a Codex subagent's thread).
    skip: bool,
    /// Where its last turn stands; latest line wins.
    turn: TurnEnd,
    /// First line of the latest assistant text.
    last: Option<String>,
    /// MIS's messages in the conversation, with their sender's address (`origin.from`):
    /// the session may answer by messaging it back.
    delivered: Vec<(String, Delivered)>,
}

/// Where a transcript's last turn stands, as far as its lines show.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    /// No turn seen.
    #[default]
    Unknown,
    /// Under way: a prompt, tool call or tool result has no reply after it.
    Running,
    /// Ended with the agent's reply.
    Replied,
    /// Ended otherwise (interrupted, aborted).
    Ended,
}

/// What [`read_file`] finds in one session file.
pub(crate) struct FileRead {
    pub session: Session,
    /// The session a codex copy was made from.
    pub forked_from: Option<String>,
    pub turn: TurnEnd,
    pub last: Option<String>,
    /// As `LiveSession::delivered`.
    pub delivered: Vec<Delivered>,
    /// Whether it is headless can't change any more (a claude prompt's origin was seen).
    pub settled: bool,
}

/// The live feed's snippet of assistant text.
const LAST_CHARS: usize = 140;

fn snippet(s: &str) -> Option<String> {
    let line = first_line(&crate::transcript::plain(s), LAST_CHARS);
    (!line.trim().is_empty()).then_some(line)
}

impl Head {
    /// Seeded with the id the file name gives.
    fn new(id: String) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    fn headless(&self) -> Option<bool> {
        self.origin.or(self.entry)
    }

    /// The head has everything it needs; the tail can still add titles.
    fn done(&self) -> bool {
        self.skip || (self.cwd.is_some() && self.prompt.is_some() && self.model.is_some())
    }

    /// None: nothing to list (no cwd and no title, or skipped).
    fn finish(self, harness: &str, updated_ms: u64, now_ms: u64) -> Option<Session> {
        let interactive = self.headless() != Some(true);
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
            active: active(updated_ms, now_ms),
            interactive,
            // Set by `list`, which knows the live processes.
            open: false,
        })
    }
}

/// Written in the last `ACTIVE_MS`.
pub(crate) fn active(updated_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(updated_ms) < ACTIVE_MS
}

fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Text the user typed, not what the harness injected under the user's role:
/// tagged blocks (`<command-name>`, `<system-reminder>`, `<environment_context>`,
/// `<INSTRUCTIONS>`, `<image …>`), AGENTS.md, interrupts. `<div> overflows` is typed.
pub(crate) fn typed(s: &str) -> Option<String> {
    let s = s.trim();
    let injected = ["[Request interrupted", "# AGENTS.md instructions"];
    let tag = s.strip_prefix('<').map(|t| {
        let name = t
            .split(|c: char| c == '>' || c.is_whitespace())
            .next()
            .unwrap_or("");
        name.contains(['-', '_']) || name == "image" || name.chars().all(|c| c.is_ascii_uppercase())
    });
    let skip = s.is_empty() || tag == Some(true) || injected.iter().any(|p| s.starts_with(p));
    (!skip).then(|| s.to_string())
}

/// One line of a Claude transcript. Unparseable lines (a cut-off last one) are skipped.
fn claude_line(h: &mut Head, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("custom-title") => h.custom_title = text(&v["customTitle"]).or(h.custom_title.take()),
        Some("ai-title") => h.auto_title = text(&v["aiTitle"]).or(h.auto_title.take()),
        // Titles of older versions.
        Some("summary") => h.auto_title = text(&v["summary"]).or(h.auto_title.take()),
        // Written as each turn ends, after its reply if any.
        Some("system") if v["subtype"] == "turn_duration" && h.turn == TurnEnd::Running => {
            h.turn = TurnEnd::Ended
        }
        Some(kind @ ("user" | "assistant")) => {
            if v["isSidechain"] == true {
                return;
            }
            if let Some(p) = mis_peer(&v) {
                let d = Delivered {
                    msg_id: p.msg_id.into(),
                    reply: None,
                };
                h.delivered.push((p.from.into(), d));
                let over = h.delivered.len().saturating_sub(MAX_DELIVERED);
                h.delivered.drain(..over);
            }
            claude_turn(h, kind, &v);
            if h.cwd.is_none() {
                h.cwd = text(&v["cwd"]);
            }
            // A prompt's `turnOrigin`: `human` typed, `sdk` for `claude -p` and SDK runs.
            // `entrypoint`: `cli`, or `sdk-cli` and the like (rewritten in copies).
            let origin = (kind == "user").then(|| v["turnOrigin"].as_str()).flatten();
            h.origin = h.origin.or(origin.map(|o| o == "sdk"));
            h.entry = h
                .entry
                .or(v["entrypoint"].as_str().map(|e| e.starts_with("sdk")));
            let msg = &v["message"];
            if kind == "assistant" {
                // `<synthetic>` marks messages claude made up itself (errors, interrupts).
                if let Some(m) = text(&msg["model"]).filter(|m| !m.starts_with('<')) {
                    h.model = Some(m);
                }
            } else if h.prompt.is_none() && v["isMeta"] != true && v["isCompactSummary"] != true {
                // The first typed text block; tool results come as other block types.
                h.prompt = match &msg["content"] {
                    Value::Array(blocks) => blocks.iter().filter_map(block_text).find_map(typed),
                    c => block_text(c).and_then(typed),
                };
            }
        }
        _ => {}
    }
}

/// A message MIS delivered (`PEER_NAME`), as the session it went to records it: a meta
/// user turn whose `origin` names the sender and holds the text as sent.
pub(crate) struct Peer<'a> {
    pub msg_id: &'a str,
    /// The sender's address, which a reply is sent to.
    pub from: &'a str,
    pub body: &'a str,
}

pub(crate) fn mis_peer(v: &Value) -> Option<Peer<'_>> {
    let o = &v["origin"];
    let mis = v["type"] == "user" && o["kind"] == "peer" && o["name"] == PEER_NAME;
    mis.then_some(())?;
    Some(Peer {
        msg_id: o["msg_id"].as_str()?,
        from: o["from"].as_str().unwrap_or_default(),
        body: o["body"].as_str()?,
    })
}

/// A content block's text; a string content is one text block.
fn block_text(b: &Value) -> Option<&str> {
    match b {
        Value::String(s) => Some(s),
        b if b["type"] == "text" => b["text"].as_str(),
        _ => None,
    }
}

/// How one claude message line moves its turn: a reply ends it; a prompt, tool call or
/// tool result keeps it going; an interrupt ends it.
fn claude_turn(h: &mut Head, kind: &str, v: &Value) {
    if v["isMeta"] == true || v["isCompactSummary"] == true {
        return;
    }
    let content = &v["message"]["content"];
    let blocks: Vec<&Value> = match content {
        Value::Array(b) => b.iter().collect(),
        _ => vec![content],
    };
    let has = |t: &str| blocks.iter().any(|b| b["type"] == t);
    if kind == "assistant" {
        let said = blocks.iter().find_map(|b| block_text(b)).and_then(snippet);
        // A delivered message is answered by a message back to its sender...
        for b in blocks.iter().filter(|b| b["name"] == "SendMessage") {
            let (to, text) = (b["input"]["to"].as_str(), b["input"]["message"].as_str());
            for (_, d) in h
                .delivered
                .iter_mut()
                .filter(|(f, _)| Some(f.as_str()) == to)
            {
                d.reply = d.reply.take().or(text.and_then(snippet));
            }
        }
        // Text written before a tool call carries the call's stop reason.
        if has("tool_use") || v["message"]["stop_reason"] == "tool_use" {
            h.turn = TurnEnd::Running;
        } else if said.is_some() {
            h.turn = TurnEnd::Replied;
            // ...or by the reply that next ends a turn.
            for (_, d) in h.delivered.iter_mut().filter(|(_, d)| d.reply.is_none()) {
                d.reply = said.clone();
            }
        }
        h.last = said.or(h.last.take());
        return;
    }
    if blocks
        .iter()
        .filter_map(|b| block_text(b))
        .any(|t| t.starts_with("[Request interrupted"))
    {
        h.turn = TurnEnd::Ended;
    } else if has("tool_result")
        || blocks
            .iter()
            .filter_map(|b| block_text(b))
            .any(|t| typed(t).is_some())
    {
        h.turn = TurnEnd::Running;
    }
}

/// One line of a Codex rollout.
fn codex_line(h: &mut Head, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let p = &v["payload"];
    match v["type"].as_str() {
        // The file's own (first) one; a later one may describe another session.
        Some("session_meta") if h.entry.is_none() => {
            if let Some(id) = text(&p["id"]) {
                h.id = id;
            }
            h.cwd = text(&p["cwd"]).or(h.cwd.take());
            h.entry = Some(p["originator"] == "codex_exec");
            h.forked_from = text(&p["forked_from_id"]);
            h.skip = p["source"].get("subagent").is_some();
        }
        Some("turn_context") => h.model = text(&p["model"]).or(h.model.take()),
        Some("event_msg") => match p["type"].as_str() {
            Some("task_started") => h.turn = TurnEnd::Running,
            Some("task_complete" | "turn_aborted") => h.turn = TurnEnd::Ended,
            _ => {}
        },
        // Commentary comes mid-turn too: only `task_complete` ends one.
        Some("response_item") if p["type"] == "message" && p["role"] == "assistant" => {
            let parts = p["content"].as_array().into_iter().flatten();
            let said = parts
                .filter(|c| c["type"] == "output_text")
                .find_map(|c| c["text"].as_str().and_then(snippet));
            h.last = said.or(h.last.take());
        }
        Some("response_item")
            if matches!(
                p["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            ) =>
        {
            h.turn = TurnEnd::Running
        }
        // One user message holds several inputs: AGENTS.md, environment, then the prompt.
        Some("response_item") if p["type"] == "message" && p["role"] == "user" => {
            let inputs = p["content"].as_array().into_iter().flatten();
            let prompt = inputs
                .filter(|c| c["type"] == "input_text")
                .find_map(|c| c["text"].as_str().and_then(typed));
            if prompt.is_some() {
                h.turn = TurnEnd::Running;
            }
            h.prompt = h.prompt.take().or(prompt);
        }
        _ => {}
    }
}

/// Feed `r`'s lines to `f` until it's exhausted or `f` says stop; returns bytes consumed.
pub(crate) fn feed(r: impl Read, mut f: impl FnMut(&str) -> bool) -> u64 {
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

/// The session id a codex rollout's file stem ends with: `rollout-<time>-<id>`.
fn codex_id(stem: &str) -> Option<&str> {
    stem.get(stem.len().checked_sub(36)?..)
}

/// One session file: up to `head` bytes from its start, then its last `tail` bytes
/// unless the head already covered them, and the session it is a codex copy of. None
/// for a headless one unless `headless` (a copy is judged by its original, later).
pub(crate) fn read_file(
    harness: &str,
    path: &Path,
    (head, tail): (u64, u64),
    headless: bool,
    updated_ms: u64,
    now_ms: u64,
) -> Option<FileRead> {
    let line = match harness {
        "claude" => claude_line,
        _ => codex_line,
    };
    let stem = path.file_stem()?.to_string_lossy();
    // Codex names files `rollout-<time>-<id>`; session_meta has the id too.
    let id = match harness {
        "claude" => stem.to_string(),
        _ => codex_id(&stem)?.to_string(),
    };
    let mut h = Head::new(id);
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // Settled once a claude prompt's origin is seen; codex has only its originator.
    let settled = |h: &Head| h.origin.is_some() || harness == "codex";
    let unwanted =
        |h: &Head| h.headless() == Some(true) && settled(h) && h.forked_from.is_none() && !headless;
    let used = feed((&mut file).take(head), |l| {
        line(&mut h, l);
        h.done() || unwanted(&h)
    });
    if unwanted(&h) {
        return None;
    }
    let from = used.max(len.saturating_sub(tail));
    // Lines skipped between head and tail: what the head said of the turn is stale.
    if from > used {
        (h.turn, h.last) = (TurnEnd::Unknown, None);
        h.delivered.clear();
    }
    if from < len && file.seek(SeekFrom::Start(from)).is_ok() {
        // A first line cut mid-way fails to parse and is skipped. Stop at the old end:
        // a live session may be appending.
        feed((&mut file).take(len - from), |l| {
            line(&mut h, l);
            false
        });
    }
    let (forked_from, turn, last, settled) =
        (h.forked_from.take(), h.turn, h.last.take(), settled(&h));
    let delivered = h.delivered.drain(..).map(|(_, d)| d).collect();
    let session = h.finish(harness, updated_ms, now_ms)?;
    Some(FileRead {
        session,
        forked_from,
        turn,
        last,
        delivered,
        settled,
    })
}

/// Copies made of copies: how far back to look for the original.
const MAX_COPY_DEPTH: usize = 4;

/// A codex copy as listed: its original's title (its own first prompt is the message
/// that made it), and interactive if either was started so. The original is found by
/// following `forked_from` through `find` (session id -> its file); if it isn't found
/// within `MAX_COPY_DEPTH`, `s` is unchanged.
pub(crate) fn resolve_copy(
    s: Session,
    mut forked_from: Option<String>,
    find: impl Fn(&str) -> Option<(u64, PathBuf)>,
    now_ms: u64,
) -> Session {
    let mut original = None;
    for _ in 0..MAX_COPY_DEPTH {
        let Some((at, path)) = forked_from.as_deref().and_then(&find) else {
            break;
        };
        let Some(o) = read_file("codex", &path, (HEAD_BYTES, TAIL_BYTES), true, at, now_ms) else {
            break;
        };
        (original, forked_from) = (Some(o.session), o.forked_from);
    }
    match original {
        Some(o) if forked_from.is_none() => Session {
            title: o.title.or(s.title),
            interactive: s.interactive || o.interactive,
            ..s
        },
        _ => s,
    }
}

/// Codex files by session id (`rollout-<time>-<id>.jsonl`), to find a copy's original.
pub(crate) fn codex_ids(files: &[(u64, PathBuf)]) -> HashMap<String, (u64, PathBuf)> {
    (files.iter())
        .filter_map(|(at, p)| {
            let stem = p.file_stem()?.to_string_lossy();
            Some((codex_id(&stem)?.to_string(), (*at, p.clone())))
        })
        .collect()
}

/// `*.jsonl` files exactly `depth` directories below `dir`, with their mtimes.
pub(crate) fn jsonl_at(dir: &Path, depth: usize) -> Vec<(u64, PathBuf)> {
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

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
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
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map_or(home.join(".claude"), PathBuf::from);
    let codex = mis_agent::codex::codex_home().unwrap_or_default();
    (claude, codex)
}

/// The newest `MAX_SESSIONS` sessions under both roots, newest first; headless ones
/// (`claude -p`, `codex exec`) only if `headless`. Claude subagents live a level
/// deeper (`<id>/subagents/`) and aren't listed.
/// Every session file under both roots with its harness and mtime, newest first.
pub(crate) fn files(claude_root: &Path, codex: &Path) -> Vec<(&'static str, (u64, PathBuf))> {
    let claude = jsonl_at(&claude_root.join("projects"), 1)
        .into_iter()
        .map(|f| ("claude", f));
    let codex = jsonl_at(&codex.join("sessions"), 3)
        .into_iter()
        .map(|f| ("codex", f));
    let mut files: Vec<_> = claude.chain(codex).collect();
    files.sort_by_key(|(_, (at, _))| std::cmp::Reverse(*at));
    files
}

pub fn list(claude_root: &Path, codex: &Path, headless: bool, now_ms: u64) -> Vec<Session> {
    let files = files(claude_root, codex);
    let held = live::claude_open(claude_root, live::alive);
    let codex_files: Vec<_> = (files.iter())
        .filter(|(h, _)| *h == "codex")
        .map(|(_, f)| f.clone())
        .collect();
    let by_id = codex_ids(&codex_files);
    files
        .iter()
        .take(MAX_SCANNED)
        .filter_map(|(h, (at, p))| read_file(h, p, (HEAD_BYTES, TAIL_BYTES), headless, *at, now_ms))
        .map(|r| {
            resolve_copy(
                r.session,
                r.forked_from,
                |id| by_id.get(id).cloned(),
                now_ms,
            )
        })
        .filter(|s| headless || s.interactive)
        .take(MAX_SESSIONS)
        .map(|mut s| {
            s.open = open(&s.harness, &s.id, s.active, &held);
            s
        })
        .collect()
}

/// Claude: a live process holds it (`held`, from `live::claude_open`). Codex: no such
/// record, so a recent write (`active`) is taken to mean it may be open.
pub(crate) fn open(harness: &str, id: &str, active: bool, held: &HashSet<String>) -> bool {
    match harness {
        "claude" => held.contains(id),
        _ => active,
    }
}

/// [`open`] for one session, reading the live processes only for claude.
pub(crate) fn is_open(claude: &Path, harness: &str, id: &str, active: bool) -> bool {
    let held = match harness {
        "claude" => live::claude_open(claude, live::alive),
        _ => HashSet::new(),
    };
    open(harness, id, active, &held)
}

/// One session file's summary, as listed (headless or not).
pub(crate) fn read_one(harness: &str, path: &Path, now_ms: u64) -> Option<Session> {
    let at = mtime_ms(&std::fs::metadata(path).ok()?);
    read_file(harness, path, (HEAD_BYTES, TAIL_BYTES), true, at, now_ms).map(|r| r.session)
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
            r#"{"type":"user","entrypoint":"cli","message":{"content":[{"type":"text","text":"<ide_opened_file>a.rs</ide_opened_file>"},{"type":"text","text":"  fix the build\nthen test"}]}}"#,
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
    fn headless_is_how_it_started_and_tags_are_told_from_html() {
        let cli = r#"{"type":"user","cwd":"/w","entrypoint":"cli","message":{"content":"<div> overflows"}}"#;
        let resumed = r#"{"type":"user","entrypoint":"sdk-cli","message":{"content":"x"}}"#;
        let s = claude(&[cli, resumed]).finish("claude", 0, 0).unwrap();
        assert!(s.interactive);
        assert_eq!(s.title.as_deref(), Some("<div> overflows"));
        let tags = [
            "<command-name>/clear</command-name>",
            "<system-reminder>x</system-reminder>",
            "<environment_context>",
            "<INSTRUCTIONS>",
            "<image name=[Image #1]>",
        ];
        for t in tags {
            assert_eq!(typed(t), None, "{t}");
        }
    }

    #[test]
    fn a_claude_copy_counts_as_started_like_its_original() {
        // `claude -p --resume X --fork-session` rewrites `entrypoint` on the copied lines
        // but keeps each prompt's `turnOrigin`.
        let copied = r#"{"type":"user","cwd":"/w","entrypoint":"sdk-cli","turnOrigin":"human","message":{"content":"pick a color"}}"#;
        let added = r#"{"type":"user","entrypoint":"sdk-cli","turnOrigin":"sdk","message":{"content":"and now?"}}"#;
        let s = claude(&[copied, added]).finish("claude", 0, 0).unwrap();
        assert!(s.interactive);
        assert_eq!(s.title.as_deref(), Some("pick a color"));
        // A prompt's origin wins over an entrypoint seen first, on any line.
        let meta = r#"{"type":"user","isMeta":true,"entrypoint":"sdk-cli","message":{"content":"Caveat"}}"#;
        assert!(
            claude(&[meta, copied])
                .finish("claude", 0, 0)
                .unwrap()
                .interactive
        );
        let headless = r#"{"type":"user","cwd":"/w","entrypoint":"sdk-cli","turnOrigin":"sdk","message":{"content":"p"}}"#;
        assert!(
            !claude(&[headless])
                .finish("claude", 0, 0)
                .unwrap()
                .interactive
        );
    }

    #[test]
    fn a_codex_copy_takes_its_originals_title_and_start() {
        let root = temp();
        let day = root.join("2026").join("11").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = |id: &str, meta: &str, prompt: &str| {
            let user = format!(
                r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{prompt}"}}]}}}}"#
            );
            let path = day.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl"));
            std::fs::write(&path, format!("{meta}\n{user}\n")).unwrap();
            path
        };
        let (a, b, c) = (
            "aaaaaaaa-0000-7000-8000-000000000001",
            "bbbbbbbb-0000-7000-8000-000000000002",
            "cccccccc-0000-7000-8000-000000000003",
        );
        let meta = |id: &str, origin: &str, from: &str| {
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"/r","originator":"{origin}","forked_from_id":{from}}}}}"#
            )
        };
        rollout(a, &meta(a, "codex-tui", "null"), "port the renderer");
        rollout(
            b,
            &meta(b, "codex_exec", &format!("\"{a}\"")),
            "a message to a copy",
        );
        // A second session_meta (another session's) doesn't change whose file this is.
        let c_meta = format!(
            "{}\n{}",
            meta(c, "codex_exec", &format!("\"{b}\"")),
            meta(a, "codex-tui", "null")
        );
        let c_path = rollout(c, &c_meta, "a copy of the copy");
        let files: HashMap<String, (u64, PathBuf)> = [a, b, c]
            .map(|id| {
                (
                    id.to_string(),
                    (
                        0,
                        day.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl")),
                    ),
                )
            })
            .into();
        let r = read_file("codex", &c_path, (1 << 20, 0), false, 0, 0).unwrap();
        let (s, from) = (r.session, r.forked_from);
        assert_eq!(
            (s.id.as_str(), s.interactive, from.as_deref()),
            (c, false, Some(b))
        );
        let s = resolve_copy(s, from, |id| files.get(id).cloned(), 0);
        assert_eq!(
            (s.id.as_str(), s.title.as_deref(), s.interactive),
            (c, Some("port the renderer"), true)
        );
        // A copy started in a terminal stays interactive, whatever its original.
        let d = "dddddddd-0000-7000-8000-000000000004";
        let d_path = rollout(d, &meta(d, "codex-tui", &format!("\"{b}\"")), "tui copy");
        let r = read_file("codex", &d_path, (1 << 20, 0), false, 0, 0).unwrap();
        let (s2, from) = (r.session, r.forked_from);
        let s2 = resolve_copy(s2, from, |id| files.get(id).cloned(), 0);
        assert_eq!(
            (s2.title.as_deref(), s2.interactive),
            (Some("port the renderer"), true)
        );
        // Its original gone: listed as it is.
        let s = resolve_copy(s, Some("gone".into()), |_| None, 0);
        assert_eq!(s.id, c);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A message MIS delivered, as claude records it in the receiving session.
    fn peer(msg_id: &str, name: &str) -> String {
        serde_json::json!({"type": "user", "isMeta": true, "turnOrigin": "peer",
            "origin": {"kind": "peer", "from": format!("uds:/tmp/{msg_id}.sock"),
                "msg_id": msg_id, "name": name, "body": "add a test"},
            "message": {"content": "<cross-session-message>add a test</cross-session-message>"}})
        .to_string()
    }

    #[test]
    fn delivered_messages_are_answered_by_the_next_reply_or_a_message_back() {
        let got = |lines: &[&str]| {
            let h = claude(lines);
            let d = h.delivered.into_iter().map(|(_, d)| (d.msg_id, d.reply));
            (d.collect::<Vec<_>>(), h.turn, h.prompt)
        };
        let prompt = r#"{"type":"user","message":{"content":"fix the build"}}"#;
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":"tool_use"}}"#;
        let reply = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Added one.\nMore."}],"stop_reason":"end_turn"}}"#;
        let (m1, other) = (peer("m1", PEER_NAME), peer("m9", "a-peer"));
        // Taken in while idle: not a prompt, and the turn waits on the reply.
        let (d, turn, typed) = got(&[&m1, &other]);
        assert_eq!(d, [("m1".to_string(), None)], "only MIS's");
        assert_eq!((turn, typed), (TurnEnd::Unknown, None));
        let answered = [("m1".to_string(), Some("Added one.".to_string()))];
        assert_eq!(got(&[&m1, reply]).0, answered);
        // Mid-turn: text before a tool call doesn't answer it, the reply does.
        let before = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Running it."}],"stop_reason":"tool_use"}}"#;
        assert_eq!(
            got(&[prompt, call, &m1, before]).0,
            [("m1".to_string(), None)]
        );
        assert_eq!(got(&[prompt, call, &m1, before, reply]).0, answered);
        // Answered by a message back to its sender (MIS isn't there to hear it).
        let back = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"SendMessage","input":{"to":"uds:/tmp/m1.sock","message":"Done: added it."}}],"stop_reason":"tool_use"}}"#;
        let elsewhere = back.replace("m1.sock", "m9.sock");
        assert_eq!(got(&[&m1, &elsewhere]).0, [("m1".to_string(), None)]);
        let (d, ..) = got(&[&m1, back, reply]);
        assert_eq!(d, [("m1".to_string(), Some("Done: added it.".to_string()))]);
        // An earlier answer stays; only the latest few are kept.
        let m2 = peer("m2", PEER_NAME);
        let (d, ..) = got(&[&m1, reply, &m2]);
        assert_eq!(d[1], ("m2".to_string(), None));
        assert_eq!(d[0].1.as_deref(), Some("Added one."));
        let many: Vec<String> = (0..12).map(|i| peer(&format!("n{i}"), PEER_NAME)).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let (d, ..) = got(&many);
        assert_eq!((d.len(), d[0].0.as_str()), (MAX_DELIVERED, "n4"));
    }

    #[test]
    fn claude_turns_end_with_a_reply_and_run_through_tools() {
        let turn = |lines: &[&str]| {
            let h = claude(lines);
            (h.turn, h.last)
        };
        let prompt = r#"{"type":"user","message":{"content":"fix the build"}}"#;
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":"tool_use"}}"#;
        let result =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        let reply = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"\u001b[1mFixed\u001b[0m it.\nDetails follow."}],"stop_reason":"end_turn"}}"#;
        let done = r#"{"type":"system","subtype":"turn_duration"}"#;
        let meta = r#"{"type":"mode"}"#;
        assert_eq!(turn(&[]), (TurnEnd::Unknown, None));
        assert_eq!(turn(&[prompt]).0, TurnEnd::Running);
        // A permission prompt pending: the call has no result yet.
        assert_eq!(turn(&[prompt, call]).0, TurnEnd::Running);
        assert_eq!(turn(&[prompt, call, result]).0, TurnEnd::Running);
        let replied = (TurnEnd::Replied, Some("Fixed it.".to_string()));
        assert_eq!(turn(&[prompt, call, result, reply, done, meta]), replied);
        let before_call = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Running it."}],"stop_reason":"tool_use"}}"#;
        assert_eq!(turn(&[prompt, before_call]).0, TurnEnd::Running);
        // Text before a tool call is a snippet, not the end.
        let (end, last) = turn(&[prompt, reply, call]);
        assert_eq!(
            (end, last.as_deref()),
            (TurnEnd::Running, Some("Fixed it."))
        );
        // Interrupted, or ended with no reply.
        let stop = r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;
        assert_eq!(turn(&[prompt, call, stop]).0, TurnEnd::Ended);
        assert_eq!(turn(&[prompt, call, result, done]).0, TurnEnd::Ended);
        // Injected text, local commands and subagents' lines don't move it.
        let injected = [
            r#"{"type":"user","isMeta":true,"message":{"content":"Caveat"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"tool_use"}]}}"#,
        ];
        let mut lines = vec![prompt, reply];
        lines.extend(injected);
        assert_eq!(turn(&lines).0, TurnEnd::Replied);
        let long = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "é".repeat(500)
        );
        assert_eq!(turn(&[&long]).1.unwrap().chars().count(), LAST_CHARS);
    }

    #[test]
    fn codex_turns_run_from_task_started_to_complete() {
        let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"shell"}}"#;
        let said = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed."}]}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
        let aborted = r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#;
        assert_eq!(codex(&[started, call]).turn, TurnEnd::Running);
        // Commentary mid-turn doesn't end it.
        assert_eq!(codex(&[started, said, call]).turn, TurnEnd::Running);
        assert_eq!(codex(&[started, call, said]).turn, TurnEnd::Running);
        let h = codex(&[started, call, said, complete]);
        assert_eq!(
            (h.turn, h.last.as_deref()),
            (TurnEnd::Ended, Some("Renamed."))
        );
        assert_eq!(codex(&[started, call, aborted]).turn, TurnEnd::Ended);
        let prompt = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}}"#;
        assert_eq!(codex(&[said, complete, prompt]).turn, TurnEnd::Running);
    }

    #[test]
    fn a_skipped_middle_forgets_what_the_head_said_of_the_turn() {
        let dir = temp();
        let path = dir.join("c1.jsonl");
        let reply = r#"{"type":"assistant","cwd":"/w","message":{"content":[{"type":"text","text":"early"}]}}"#;
        let tail_title = r#"{"type":"ai-title","aiTitle":"T"}"#;
        let pad = format!("{{\"type\":\"pad\",\"x\":\"{}\"}}\n", "x".repeat(200));
        std::fs::write(&path, format!("{reply}\n{}{tail_title}\n", pad.repeat(10))).unwrap();
        let r = read_file("claude", &path, (300, 300), true, 0, 0).unwrap();
        assert_eq!((r.turn, r.last), (TurnEnd::Unknown, None));
        let r = read_file("claude", &path, (1 << 20, 300), true, 0, 0).unwrap();
        assert_eq!(
            (r.turn, r.last.as_deref()),
            (TurnEnd::Replied, Some("early"))
        );
        std::fs::remove_dir_all(dir).unwrap();
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
        let s = read_file("claude", &path, (100, 400), true, 0, 0)
            .unwrap()
            .session;
        assert_eq!((s.title.as_deref(), s.cwd), (Some("Tail title"), None));
        // A head long enough reaches the prompt and its cwd.
        let s = read_file("claude", &path, (1 << 20, 400), true, 0, 0)
            .unwrap()
            .session;
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
        let meta = r#"{"type":"session_meta","payload":{"cwd":"/r","originator":"codex_exec"}}"#;
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
        let got = list(&claude, &codex, true, now);
        let ids: Vec<_> = got
            .iter()
            .map(|s| (s.harness.as_str(), s.id.as_str()))
            .collect();
        assert_eq!(ids, [("codex", id), ("claude", "c1")]);
        assert!(got[0].active && !got[1].active);
        assert_eq!(got[1].title.as_deref(), Some("claude task"));
        // Interactive only: the `codex exec` run is left out.
        let got = list(&claude, &codex, false, now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "c1");
        // Missing roots list nothing.
        assert!(list(&claude.join("none"), &codex.join("none"), true, now).is_empty());
        for d in [claude, codex] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }
}
