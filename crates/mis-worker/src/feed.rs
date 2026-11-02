//! The live sessions feed: interactive sessions open now or written within
//! `LIVE_WINDOW_MS`, with what each is doing, for `WorkerMsg::LiveSessions`. Reading
//! files and processes is [`Feed::poll`]'s, which re-parses only files whose size or
//! mtime changed; deciding a session's state ([`state`]), which are listed ([`pick`]) and
//! whether to send ([`Feed::changed`]) is pure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mis_types::{LiveSession, LiveState, Session, LIVE_WINDOW_MS, MAX_LIVE, STALL_MS};

use crate::live::Proc;
use crate::sessions::{self, TurnEnd};

/// How often the feed is re-read.
pub const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// A longer tail than listing reads: one big tool result can fill 64 KB.
const TAIL_BYTES: u64 = 256 << 10;
/// Most files parsed in one poll; the rest wait for the next.
const MAX_SCANNED: usize = 1000;
const WAITING_FOR_CHARS: usize = 60;

/// What one file says, kept until its size or mtime changes.
#[derive(Clone)]
struct Facts {
    session: Session,
    turn: TurnEnd,
    last: Option<String>,
}

#[derive(Default)]
pub struct Feed {
    /// By path: (len, mtime) when parsed, and its facts; None for files not listed (headless).
    cache: HashMap<PathBuf, ((u64, u64), Option<Facts>)>,
    /// Last sent this connection; None until the first send.
    sent: Option<Vec<LiveSession>>,
    /// Files the last poll parsed (the rest came from the cache).
    pub parsed: usize,
}

/// What a session is doing and, if claude says, what it waits on. Claude's open process
/// says first: busy works (or stalls, when nothing was written for `STALL_MS`), waiting
/// waits, idle after a reply waits on the user. Otherwise (closed, codex, an unknown
/// status) the transcript says: a turn under way works, then stalls.
pub fn state(
    proc: Option<&Proc>,
    turn: TurnEnd,
    updated_ms: u64,
    now_ms: u64,
) -> (LiveState, Option<String>) {
    let quiet = now_ms.saturating_sub(updated_ms) >= STALL_MS;
    let busy = match quiet {
        true => LiveState::Stalled,
        false => LiveState::Working,
    };
    match proc.and_then(|p| p.status.as_deref()) {
        Some("busy") => (busy, None),
        Some("waiting") => {
            let why = proc.and_then(|p| p.waiting_for.as_deref());
            let why = why.map(|w| mis_types::first_line(w, WAITING_FOR_CHARS));
            (LiveState::Waiting, why)
        }
        Some("idle") if turn == TurnEnd::Replied => (LiveState::Waiting, None),
        Some("idle") => (LiveState::Idle, None),
        Some("shell") => (LiveState::Shell, None),
        _ if turn == TurnEnd::Running => (busy, None),
        _ => (LiveState::Idle, None),
    }
}

/// The ones to send: open, or written within `LIVE_WINDOW_MS`; newest first, at most `MAX_LIVE`.
pub fn pick(mut all: Vec<LiveSession>, now_ms: u64) -> Vec<LiveSession> {
    all.retain(|s| s.open || now_ms.saturating_sub(s.updated_ms) < LIVE_WINDOW_MS);
    all.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    all.truncate(MAX_LIVE);
    all
}

/// Newest write among a claude session's subagent transcripts (`<id>/subagents/*.jsonl`):
/// a session waiting on its subagents writes nothing itself.
fn subagents_ms(transcript: &Path) -> u64 {
    let dir = transcript.with_extension("").join("subagents");
    let files = sessions::jsonl_at(&dir, 0);
    files.into_iter().map(|(at, _)| at).max().unwrap_or(0)
}

impl Feed {
    /// A new connection: the next poll is sent whatever it holds.
    pub fn reconnected(&mut self) {
        self.sent = None;
    }

    /// `sessions` if they differ from what was last sent, now recorded as sent.
    pub fn changed(&mut self, sessions: Vec<LiveSession>) -> Option<Vec<LiveSession>> {
        if self.sent.as_ref() == Some(&sessions) {
            return None;
        }
        self.sent = Some(sessions.clone());
        Some(sessions)
    }

    /// Live sessions under both roots now; `procs` are claude's open sessions
    /// (`live::claude_procs`).
    pub fn poll(
        &mut self,
        claude: &Path,
        codex: &Path,
        procs: &HashMap<String, Proc>,
        now_ms: u64,
    ) -> Vec<LiveSession> {
        let stem = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned());
        let claude_files = sessions::jsonl_at(&claude.join("projects"), 1);
        let codex_files = sessions::jsonl_at(&codex.join("sessions"), 3);
        let held = |p: &Path| stem(p).is_some_and(|id| procs.contains_key(&id));
        let recent = |at: u64| now_ms.saturating_sub(at) < LIVE_WINDOW_MS;
        let mut files: Vec<_> = (claude_files.into_iter())
            .filter(|(at, p)| recent(*at) || held(p))
            .map(|f| ("claude", f))
            .chain(
                codex_files
                    .iter()
                    .filter(|(at, _)| recent(*at))
                    .cloned()
                    .map(|f| ("codex", f)),
            )
            .collect();
        files.sort_by_key(|(_, (at, _))| std::cmp::Reverse(*at));
        files.truncate(MAX_SCANNED);
        // Built on first need: a codex copy's original, by session id.
        let mut by_id: Option<HashMap<String, (u64, PathBuf)>> = None;
        let mut cache = HashMap::new();
        let mut out = vec![];
        self.parsed = 0;
        for (harness, (at, path)) in files {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let key = (meta.len(), at);
            let facts = match self.cache.remove(&path) {
                Some((k, facts)) if k == key => facts,
                _ => {
                    self.parsed += 1;
                    read(harness, &path, at, now_ms, &codex_files, &mut by_id)
                }
            };
            cache.insert(path.clone(), (key, facts.clone()));
            let Some(f) = facts else { continue };
            let proc = procs.get(&f.session.id).filter(|_| harness == "claude");
            let working = proc.is_some_and(|p| p.status.as_deref() == Some("busy"))
                || f.turn == TurnEnd::Running;
            let updated_ms = match harness == "claude" && working {
                true => at.max(subagents_ms(&path)),
                false => at,
            };
            let (state, waiting_for) = state(proc, f.turn, updated_ms, now_ms);
            let s = f.session;
            out.push(LiveSession {
                harness: s.harness,
                id: s.id,
                title: s.title,
                name: proc.and_then(|p| p.name.clone()),
                cwd: s.cwd,
                model: s.model,
                open: proc.is_some(),
                state,
                waiting_for,
                updated_ms,
                last: f.last,
            });
        }
        // Files gone or out of the window are forgotten.
        self.cache = cache;
        pick(out, now_ms)
    }
}

/// Parse one file; None if it isn't listed (headless, or nothing to show).
fn read(
    harness: &str,
    path: &Path,
    at: u64,
    now_ms: u64,
    codex_files: &[(u64, PathBuf)],
    by_id: &mut Option<HashMap<String, (u64, PathBuf)>>,
) -> Option<Facts> {
    let r = sessions::read_file(
        harness,
        path,
        (sessions::HEAD_BYTES, TAIL_BYTES),
        false,
        at,
        now_ms,
    )?;
    let session = match r.forked_from {
        None => r.session,
        from => {
            let ids = by_id.get_or_insert_with(|| sessions::codex_ids(codex_files));
            sessions::resolve_copy(r.session, from, |id| ids.get(id).cloned(), now_ms)
        }
    };
    session.interactive.then_some(Facts {
        session,
        turn: r.turn,
        last: r.last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use LiveState::*;

    const NOW: u64 = 100 * 3600 * 1000;
    const MIN: u64 = 60 * 1000;

    fn proc(status: &str, waiting_for: Option<&str>) -> Proc {
        Proc {
            status: Some(status.into()),
            waiting_for: waiting_for.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn an_open_claude_session_says_what_it_does() {
        let at = |p: &Proc, turn, ago| state(Some(p), turn, NOW - ago, NOW);
        let busy = proc("busy", None);
        assert_eq!(at(&busy, TurnEnd::Running, MIN), (Working, None));
        // Busy with nothing written for 10 min: stalled, whatever the transcript says.
        assert_eq!(at(&busy, TurnEnd::Replied, 10 * MIN), (Stalled, None));
        let prompt = proc("waiting", Some("permission prompt"));
        let why = Some("permission prompt".to_string());
        assert_eq!(at(&prompt, TurnEnd::Running, 30 * MIN), (Waiting, why));
        // Idle after its reply: waiting on the user. Idle otherwise: idle.
        let idle = proc("idle", None);
        assert_eq!(at(&idle, TurnEnd::Replied, MIN), (Waiting, None));
        assert_eq!(at(&idle, TurnEnd::Ended, MIN), (Idle, None));
        assert_eq!(at(&idle, TurnEnd::Unknown, MIN), (Idle, None));
        assert_eq!(
            at(&proc("shell", None), TurnEnd::Replied, MIN),
            (Shell, None)
        );
        // A status this build doesn't know: the transcript decides.
        assert_eq!(
            at(&proc("new", None), TurnEnd::Running, MIN),
            (Working, None)
        );
    }

    #[test]
    fn a_closed_or_codex_session_goes_by_its_transcript() {
        let at = |turn, ago| state(None, turn, NOW - ago, NOW).0;
        assert_eq!(at(TurnEnd::Running, 9 * MIN), Working);
        assert_eq!(at(TurnEnd::Running, 10 * MIN), Stalled);
        assert_eq!(at(TurnEnd::Replied, MIN), Idle);
        assert_eq!(at(TurnEnd::Ended, MIN), Idle);
    }

    #[test]
    fn picks_open_or_recent_newest_first_capped() {
        let s = |id: &str, ago: u64, open: bool| LiveSession {
            id: id.into(),
            updated_ms: NOW - ago,
            open,
            ..Default::default()
        };
        let got = pick(
            vec![
                s("old", 7 * 60 * MIN, false),
                s("old-open", 9 * 60 * MIN, true),
                s("new", MIN, false),
                s("mid", 60 * MIN, false),
            ],
            NOW,
        );
        let ids: Vec<_> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old-open"]);
        let many = (0..40).map(|i| s(&i.to_string(), i, false)).collect();
        assert_eq!(pick(many, NOW).len(), MAX_LIVE);
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mis-feed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, lines: &[&str], ago: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
        touch(path, ago);
    }

    fn touch(path: &Path, ago: u64) {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(NOW - ago);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(at).unwrap();
    }

    const PROMPT: &str = r#"{"type":"user","cwd":"/w","entrypoint":"cli","turnOrigin":"human","message":{"content":"fix the build"}}"#;
    const CALL: &str = r#"{"type":"assistant","message":{"model":"claude-opus","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
    const REPLY: &str = r#"{"type":"assistant","message":{"model":"claude-opus","content":[{"type":"text","text":"Fixed it."}]}}"#;

    /// Claude sessions in each state, with process records (one also holding secrets),
    /// and codex rollouts; every record's process counts as alive.
    fn fixture() -> (PathBuf, PathBuf) {
        let (claude, codex) = (temp(), temp());
        let proj = claude.join("projects").join("-w");
        let s = |id: &str| proj.join(format!("{id}.jsonl"));
        let running = [PROMPT, CALL];
        let replied = [PROMPT, REPLY];
        write(&s("busy"), &running, MIN);
        write(&s("stuck"), &running, 15 * MIN);
        write(&s("perm"), &running, 20 * MIN);
        write(&s("replied"), &replied, 30 * MIN);
        write(&s("shell"), &replied, 40 * MIN);
        write(&s("old-open"), &replied, 9 * 60 * MIN);
        // A parent quiet while its subagent works.
        write(&s("delegating"), &running, 30 * MIN);
        write(
            &proj.join("delegating/subagents/agent-a.jsonl"),
            &[CALL],
            MIN,
        );
        write(&s("closed-running"), &running, 2 * MIN);
        write(&s("closed-quiet"), &running, 20 * MIN);
        write(&s("closed-done"), &replied, 3 * MIN);
        write(&s("closed-old"), &replied, 7 * 60 * MIN);
        let headless = r#"{"type":"user","cwd":"/w","entrypoint":"sdk-cli","turnOrigin":"sdk","message":{"content":"a task"}}"#;
        write(&s("headless"), &[headless], MIN);
        let records = claude.join("sessions");
        std::fs::create_dir_all(&records).unwrap();
        for (pid, id, status, extra) in [
            (
                1,
                "busy",
                "busy",
                r#","name":"demo-fd","peerToken":"SECRET-TOKEN","messagingSocketPath":"/tmp/SECRET-SOCK""#,
            ),
            (2, "stuck", "busy", ""),
            (3, "perm", "waiting", r#","waitingFor":"permission prompt""#),
            (4, "replied", "idle", ""),
            (5, "shell", "shell", ""),
            (6, "old-open", "idle", ""),
            (7, "delegating", "busy", ""),
        ] {
            let json = format!(
                r#"{{"pid":{pid},"sessionId":"{id}","procStart":"x","status":"{status}","statusUpdatedAt":1{extra}}}"#
            );
            std::fs::write(records.join(format!("{pid}.json")), json).unwrap();
        }
        // Never opened.
        std::fs::write(records.join("1.abc.key"), "SECRET-KEY").unwrap();
        let day = codex.join("sessions/2026/11/02");
        let rollout = |id: &str| day.join(format!("rollout-2026-11-02T10-00-00-{id}.jsonl"));
        let meta = r#"{"type":"session_meta","payload":{"cwd":"/r","originator":"codex-tui"}}"#;
        let user = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"rename the crate"}]}}"#;
        let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let said = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed."}]}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
        let (x1, x2) = (
            "aaaaaaaa-0000-7000-8000-000000000001",
            "bbbbbbbb-0000-7000-8000-000000000002",
        );
        write(&rollout(x1), &[meta, user, started], 4 * MIN);
        write(
            &rollout(x2),
            &[meta, user, started, said, complete],
            5 * MIN,
        );
        (claude, codex)
    }

    #[test]
    fn polls_each_state_from_files_and_records() {
        let (claude, codex) = fixture();
        let procs = crate::live::claude_procs(&claude, |_, _| true);
        let mut feed = Feed::default();
        let got = feed.poll(&claude, &codex, &procs, NOW);
        let states: Vec<_> = got
            .iter()
            .map(|s| (s.id.as_str(), s.state, s.open))
            .collect();
        let x1 = "aaaaaaaa-0000-7000-8000-000000000001";
        let x2 = "bbbbbbbb-0000-7000-8000-000000000002";
        assert_eq!(
            states,
            [
                ("busy", Working, true),
                ("delegating", Working, true),
                ("closed-running", Working, false),
                ("closed-done", Idle, false),
                (x1, Working, false),
                (x2, Idle, false),
                ("stuck", Stalled, true),
                ("perm", Waiting, true),
                ("closed-quiet", Stalled, false),
                ("replied", Waiting, true),
                ("shell", Shell, true),
                ("old-open", Waiting, true),
            ]
        );
        let busy = &got[0];
        assert_eq!(
            (
                busy.name.as_deref(),
                busy.cwd.as_deref(),
                busy.model.as_deref()
            ),
            (Some("demo-fd"), Some("/w"), Some("claude-opus"))
        );
        assert_eq!(busy.title.as_deref(), Some("fix the build"));
        let perm = got.iter().find(|s| s.id == "perm").unwrap();
        assert_eq!(perm.waiting_for.as_deref(), Some("permission prompt"));
        let replied = got.iter().find(|s| s.id == "replied").unwrap();
        assert_eq!(replied.last.as_deref(), Some("Fixed it."));
        assert_eq!(got[1].updated_ms, NOW - MIN, "its subagent's write");
        assert_eq!(got[4].last, None);
        assert_eq!(got[5].last.as_deref(), Some("Renamed."));
        // Secrets in the records are never read into anything sent.
        let wire = serde_json::to_string(&got).unwrap();
        assert!(!wire.contains("SECRET"), "{wire}");
        assert!(!format!("{procs:?}").contains("SECRET"));
        for d in [claude, codex] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn unchanged_files_are_not_parsed_again() {
        let (claude, codex) = fixture();
        let procs = crate::live::claude_procs(&claude, |_, _| true);
        let mut feed = Feed::default();
        let first = feed.poll(&claude, &codex, &procs, NOW);
        // The headless one is parsed once too; the one out of the window never.
        assert_eq!(feed.parsed, 13);
        assert_eq!(feed.poll(&claude, &codex, &procs, NOW), first);
        assert_eq!(feed.parsed, 0);
        // The busy session replies: only its file is read again.
        let path = claude.join("projects/-w/busy.jsonl");
        write(&path, &[PROMPT, CALL, REPLY], MIN / 2);
        let idle = HashMap::from([("busy".to_string(), proc("idle", None))]);
        let got = feed.poll(&claude, &codex, &idle, NOW);
        assert_eq!(feed.parsed, 1);
        let busy = got.iter().find(|s| s.id == "busy").unwrap();
        assert_eq!(
            (busy.state, busy.last.as_deref()),
            (Waiting, Some("Fixed it."))
        );
        for d in [claude, codex] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn only_changes_are_sent_and_a_reconnect_sends_again() {
        let mut f = Feed::default();
        assert_eq!(f.changed(vec![]), Some(vec![]), "the first, even empty");
        assert_eq!(f.changed(vec![]), None);
        let one = vec![LiveSession {
            id: "a".into(),
            ..Default::default()
        }];
        assert_eq!(f.changed(one.clone()), Some(one.clone()));
        assert_eq!(f.changed(one.clone()), None);
        f.reconnected();
        assert_eq!(f.changed(one.clone()), Some(one));
    }
}
