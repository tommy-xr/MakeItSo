//! Workers' live sessions as ledger state: the latest each reported, kept (and saved)
//! after it goes offline, listed with the tasks that messaged them.

use std::collections::HashMap;

use mis_types::*;
use serde::{Deserialize, Serialize};

use super::{ConnId, Effect, Ledger};

/// One worker's report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Live {
    /// Its hostname when it reported, for once it is gone.
    pub host: String,
    pub sessions: Vec<LiveSession>,
}

/// Messages shown per session; the latest win.
const MAX_MESSAGES: usize = 20;
const MESSAGE_CHARS: usize = 140;
/// Copies of copies: how far back to look for the session a copy was made from.
const MAX_COPY_DEPTH: usize = 8;

/// Home's order: needing the user (waiting, stalled) first, then working, then the rest;
/// newest first within each.
fn rank(s: &LiveSession) -> (u8, std::cmp::Reverse<u64>) {
    let group = match s.state {
        LiveState::Waiting | LiveState::Stalled => 0,
        LiveState::Working => 1,
        LiveState::Idle | LiveState::Shell => 2,
    };
    (group, std::cmp::Reverse(s.updated_ms))
}

impl Ledger {
    /// `wid` reports its live sessions. Ignored unless `conn` is its live connection.
    pub fn live_sessions(
        &mut self,
        wid: WorkerId,
        conn: ConnId,
        sessions: Vec<LiveSession>,
    ) -> Vec<Effect> {
        let Some(w) = self.workers.get(&wid).filter(|w| w.conn == Some(conn)) else {
            return vec![];
        };
        let host = w.info.hostname.clone();
        self.live.insert(wid, Live { host, sessions });
        vec![]
    }

    /// Every worker's live sessions in Home's order, each with the tasks that messaged it.
    /// A copy MIS made to message a session (fork mode) isn't listed itself: its messages
    /// go with the session it was copied from.
    pub fn sessions(&self) -> Vec<SessionRow> {
        let mut rows = vec![];
        for (wid, live) in &self.live {
            let online = self.workers.get(wid).is_some_and(|w| w.conn.is_some());
            let sent: Vec<_> = (self.tasks.iter())
                .filter(|t| t.pin == Some(*wid))
                .filter_map(|t| Some((t, t.task.send_to.as_ref()?)))
                .collect();
            // A copy's id -> the session it continues.
            let copy_of: HashMap<String, String> = (sent.iter())
                .filter_map(|(t, to)| Some((t.session.clone()?, to.session.to_string())))
                .filter(|(copy, of)| copy != of)
                .collect();
            let root = |mut id: String| {
                for _ in 0..MAX_COPY_DEPTH {
                    match copy_of.get(&id) {
                        Some(of) => id = of.clone(),
                        None => break,
                    }
                }
                id
            };
            for s in live
                .sessions
                .iter()
                .filter(|s| !copy_of.contains_key(&s.id))
            {
                let mut messages: Vec<SessionMessage> = (sent.iter())
                    .filter(|(t, to)| {
                        t.task.harness == s.harness && root(to.session.to_string()) == s.id
                    })
                    .map(|(t, _)| SessionMessage {
                        task: t.task.id,
                        status: t.status,
                        text: first_line(&t.task.prompt, MESSAGE_CHARS),
                        answered: t.status == TaskStatus::Done,
                        at: t.started_at,
                    })
                    .collect();
                messages.sort_by_key(|m| m.at);
                messages.drain(..messages.len().saturating_sub(MAX_MESSAGES));
                rows.push(SessionRow {
                    worker: *wid,
                    worker_name: live.host.clone(),
                    online,
                    session: s.clone(),
                    messages,
                });
            }
        }
        rows.sort_by_key(|r| rank(&r.session));
        rows
    }

    /// The sessions to push to watchers, if they changed since last pushed.
    pub fn sessions_changed(&mut self) -> Option<Effect> {
        let now = self.sessions();
        if self.pushed.as_ref() == Some(&now) {
            return None;
        }
        self.pushed = Some(now.clone());
        Some(Effect::Broadcast(ServerMsg::Sessions { sessions: now }))
    }

    /// For a new watcher: the sessions now, counted as pushed.
    pub fn watch_sessions(&mut self) -> Vec<SessionRow> {
        let now = self.sessions();
        self.pushed = Some(now.clone());
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    const NOW: Duration = Duration::from_secs(1);

    fn info(host: &str) -> WorkerInfo {
        WorkerInfo {
            hostname: host.into(),
            harnesses: vec!["claude".into(), "codex".into()],
            ..Default::default()
        }
    }

    fn live(id: &str, state: LiveState, updated_ms: u64) -> LiveSession {
        LiveSession {
            harness: "claude".into(),
            id: id.into(),
            title: Some(format!("title {id}")),
            state,
            updated_ms,
            ..Default::default()
        }
    }

    fn ids(rows: &[SessionRow]) -> Vec<&str> {
        rows.iter().map(|r| r.session.id.as_str()).collect()
    }

    #[test]
    fn needing_the_user_comes_first_then_working_then_by_recency() {
        let mut l = Ledger::default();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        l.connect(a, info("studio"), &[], 1, NOW);
        l.connect(b, info("laptop"), &[], 2, NOW);
        use LiveState::*;
        l.live_sessions(
            a,
            1,
            vec![live("idle", Idle, 90), live("work", Working, 50)],
        );
        l.live_sessions(
            b,
            2,
            vec![live("wait", Waiting, 10), live("stall", Stalled, 20)],
        );
        // A stale connection can't report.
        l.live_sessions(b, 1, vec![]);
        let rows = l.sessions();
        assert_eq!(ids(&rows), ["stall", "wait", "work", "idle"]);
        assert_eq!(
            (rows[0].worker, rows[0].worker_name.as_str()),
            (b, "laptop")
        );
        assert!(rows.iter().all(|r| r.online));
    }

    #[test]
    fn an_offline_workers_sessions_stay_marked_offline() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        l.live_sessions(w, 1, vec![live("s", LiveState::Working, 5)]);
        l.disconnect(w, 1);
        assert!(!l.sessions()[0].online);
        // Past the grace the worker is forgotten; its sessions aren't.
        l.expire(w, 1);
        let rows = l.sessions();
        assert_eq!((ids(&rows), rows[0].online), (vec!["s"], false));
        assert_eq!(rows[0].worker_name, "studio");
    }

    #[test]
    fn messages_go_with_their_session_and_copies_with_the_original() {
        let mut l = Ledger {
            now_ms: 1_000,
            ..Default::default()
        };
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        let (orig, copy) = (Uuid::new_v4(), Uuid::new_v4());
        let to = |session, mode| SendTo {
            session,
            title: None,
            mode,
        };
        let (first, _) = l
            .send_to_session(
                w,
                "claude".into(),
                "add a test\nplease".into(),
                to(orig, SendMode::Fork),
            )
            .unwrap();
        // It ran in a copy and answered.
        for e in [
            AgentEvent::Started {
                session: Some(copy.to_string()),
                model: None,
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("added".into()),
            },
        ] {
            l.event(w, 1, first, e);
        }
        // A reply continues the copy; it still belongs to the original.
        l.now_ms = 2_000;
        let (reply, _) = l.reply(first, "and docs".into(), vec![], false).unwrap();
        // Another session, untouched.
        let other = live("other", LiveState::Idle, 1);
        l.live_sessions(
            w,
            1,
            vec![
                live(&orig.to_string(), LiveState::Waiting, 3),
                live(&copy.to_string(), LiveState::Idle, 2),
                other,
            ],
        );
        let rows = l.sessions();
        assert_eq!(ids(&rows), [orig.to_string().as_str(), "other"]);
        let got: Vec<_> = (rows[0].messages.iter())
            .map(|m| (m.task, m.text.as_str(), m.answered, m.at))
            .collect();
        assert_eq!(
            got,
            [
                (first, "add a test", true, 1_000),
                (reply, "and docs", false, 2_000)
            ]
        );
        assert_eq!(rows[0].messages[1].status, TaskStatus::Running);
        assert!(rows[1].messages.is_empty());
    }

    #[test]
    fn watchers_get_only_changes() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        assert!(l.watch_sessions().is_empty());
        assert_eq!(l.sessions_changed(), None);
        l.live_sessions(w, 1, vec![live("s", LiveState::Working, 5)]);
        let Some(Effect::Broadcast(ServerMsg::Sessions { sessions })) = l.sessions_changed() else {
            panic!("a change is pushed")
        };
        assert_eq!(ids(&sessions), ["s"]);
        assert_eq!(l.sessions_changed(), None);
        // Going offline changes the rows too.
        l.disconnect(w, 1);
        assert!(l.sessions_changed().is_some());
    }
}
