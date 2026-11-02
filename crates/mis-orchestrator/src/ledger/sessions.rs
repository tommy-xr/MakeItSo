//! Workers' live sessions as ledger state: the latest each reported, kept (and saved)
//! after it goes offline, listed with the tasks that messaged them.

use std::collections::HashMap;

use mis_types::*;
use serde::{Deserialize, Serialize};

use super::{ConnId, Effect, Ledger, TaskEntry};

/// One worker's report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Live {
    /// Its hostname when it reported, for once it is gone.
    pub host: String,
    pub sessions: Vec<LiveSession>,
}

/// A delivered message is followed this long for its answer; after, it stays delivered.
const FOLLOW_MS: u64 = 24 * 3600 * 1000;

/// What a session's latest report says of a message delivered into it.
#[derive(Debug, PartialEq)]
enum Next {
    /// Nothing yet.
    Wait,
    /// In its conversation.
    Read,
    Answered(String),
    /// It was held, the session is back at rest with no dialog, and it never arrived:
    /// declined, or expired. (A busy one may yet take an approved message in.)
    Declined,
    /// It closed before taking the message in: continue it in place.
    Resend,
}

/// Pure: what `s` says of delivery `d`. Read once, its absence later means only that it
/// scrolled out of what the worker reads.
fn next(d: &super::Delivery, s: &LiveSession) -> Next {
    let busy = matches!(s.state, LiveState::Working | LiveState::Stalled);
    match s.delivered.iter().find(|x| x.msg_id == d.msg_id) {
        Some(Delivered { reply: Some(r), .. }) => Next::Answered(r.clone()),
        Some(_) if d.read => Next::Wait,
        Some(_) => Next::Read,
        None if d.read => Next::Wait,
        None if !s.open => Next::Resend,
        None if d.held && s.waiting_for.is_none() && !busy => Next::Declined,
        None => Next::Wait,
    }
}

/// Messages shown per session; the latest win.
const MAX_MESSAGES: usize = 20;
const MESSAGE_CHARS: usize = 140;

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
        let sessions = sessions.into_iter().take(MAX_LIVE).collect();
        self.live.insert(wid, Live { host, sessions });
        let (mut effects, changed) = self.follow(wid, true);
        effects.extend(self.schedule());
        if changed {
            effects.push(Effect::Broadcast(self.status()));
        }
        effects
    }

    /// Move `wid`'s delivered messages on by what its latest report says of their sessions:
    /// answered (done, with the answer as summary), declined (failed), or sent again in
    /// place when the session closed before taking it in. Only a report newer than the
    /// delivery may say it never arrived (`settle`). True if one moved on unannounced.
    pub(super) fn follow(&mut self, wid: WorkerId, settle: bool) -> (Vec<Effect>, bool) {
        let (now, live) = (self.now_ms, &self.live[&wid]);
        let mut effects = vec![];
        let mut changed = false;
        for t in (self.tasks.iter_mut()).filter(|t| t.pin == Some(wid)) {
            let (Some(d), Some(to)) = (&mut t.delivery, &t.task.send_to) else {
                continue;
            };
            let id = to.session.to_string();
            let s = (live.sessions.iter()).find(|s| s.harness == t.task.harness && s.id == id);
            let Some(s) = s.filter(|_| t.status == TaskStatus::Delivered) else {
                continue;
            };
            if now.saturating_sub(d.at) >= FOLLOW_MS {
                continue;
            }
            let (ok, summary) = match next(d, s) {
                Next::Wait => continue,
                Next::Declined | Next::Resend if !settle => continue,
                Next::Read => {
                    (d.read, d.held, changed) = (true, false, true);
                    continue;
                }
                Next::Answered(reply) => (true, reply),
                Next::Declined => (false, "not accepted in its terminal".into()),
                Next::Resend => {
                    t.status = TaskStatus::Queued;
                    t.delivery = None;
                    let text = "the session closed before reading it; continuing it in place";
                    t.record(&AgentEvent::Status { text: text.into() }, now);
                    self.queue.push_back(t.task.id);
                    changed = true;
                    continue;
                }
            };
            let event = AgentEvent::Finished {
                ok,
                summary: Some(summary),
            };
            t.status = if ok {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
            t.record(&event, now);
            effects.push(Effect::Broadcast(ServerMsg::Event {
                task: t.task.id,
                worker: Some(wid),
                event,
            }));
            effects.extend(t.notify());
        }
        (effects, changed)
    }

    /// `t` messages a codex session that may be open (mid-turn, or written in the last
    /// couple of minutes, as its worker reports): it waits in the queue until it goes quiet.
    pub(super) fn session_busy(&self, t: &TaskEntry) -> bool {
        let (Some(to), Some(wid)) = (&t.task.send_to, t.pin) else {
            return false;
        };
        if to.mode != SendMode::Deliver || t.task.harness != "codex" {
            return false;
        }
        let id = to.session.to_string();
        let sessions = self
            .live
            .get(&wid)
            .map(|l| &l.sessions[..])
            .unwrap_or_default();
        sessions.iter().any(|s| {
            s.harness == "codex" && s.id == id && (s.open || s.state == LiveState::Working)
        })
    }

    /// Every worker's live sessions in Home's order, each with the tasks that messaged it.
    /// A copy MIS made to message a session (fork mode) isn't listed itself: its messages
    /// go with the session it was copied from. An offline worker's sessions are kept until
    /// they would have left its own window (`LIVE_WINDOW_MS` after their last write).
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
            // Each step is one copy, so this many steps reach the original (or a cycle).
            let root = |mut id: String| {
                for _ in 0..copy_of.len() {
                    match copy_of.get(&id) {
                        Some(of) => id = of.clone(),
                        None => break,
                    }
                }
                id
            };
            let recent = |s: &&LiveSession| {
                online || self.now_ms.saturating_sub(s.updated_ms) < LIVE_WINDOW_MS
            };
            for s in (live.sessions.iter())
                .filter(|s| !copy_of.contains_key(&s.id))
                .filter(recent)
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
                        held: t.status == TaskStatus::Delivered
                            && t.delivery.as_ref().is_some_and(|d| d.held),
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
        // Until they'd have left the worker's own window.
        l.now_ms = 5 + LIVE_WINDOW_MS;
        assert!(l.sessions().is_empty());
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
                vec![],
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
    fn a_long_reply_chain_stays_with_the_original() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        let orig = Uuid::new_v4();
        let to = SendTo {
            session: orig,
            title: None,
            mode: SendMode::Fork,
        };
        let (mut task, _) =
            (l.send_to_session(w, "claude".into(), "m0".into(), to, vec![])).unwrap();
        // Each message runs in a copy of the last one's copy.
        for i in 1..=12 {
            let session = Some(Uuid::new_v4().to_string());
            l.event(
                w,
                1,
                task,
                AgentEvent::Started {
                    session,
                    model: None,
                },
            );
            let ok = AgentEvent::Finished {
                ok: true,
                summary: None,
            };
            l.event(w, 1, task, ok);
            task = l.reply(task, format!("m{i}"), vec![], false).unwrap().0;
        }
        l.live_sessions(w, 1, vec![live(&orig.to_string(), LiveState::Idle, 1)]);
        assert_eq!(l.sessions()[0].messages.len(), 13);
    }

    /// A worker that delivers, with a claude message to session `s` sent as `m1`; `event`
    /// ends it on the worker (None: still running).
    fn sent(event: Option<(bool, bool)>) -> (Ledger, WorkerId, Uuid, TaskId) {
        let mut l = Ledger {
            now_ms: 1_000,
            ..Default::default()
        };
        let w = Uuid::new_v4();
        let mut i = info("studio");
        i.deliver = true;
        l.connect(w, i, &[], 1, NOW);
        let s = Uuid::new_v4();
        let to = SendTo {
            session: s,
            title: None,
            mode: SendMode::Deliver,
        };
        let text = "add a test".into();
        let (task, _) = l
            .send_to_session(w, "claude".into(), text, to, vec![])
            .unwrap();
        if let Some((read, held)) = event {
            let msg_id = "m1".to_string();
            l.event(w, 1, task, AgentEvent::Delivered { msg_id, read, held });
        }
        (l, w, s, task)
    }

    /// Delivered into its inbox (not yet read), or held.
    fn delivered(held: bool) -> (Ledger, WorkerId, Uuid, TaskId) {
        sent(Some((false, held)))
    }

    fn open(s: Uuid, delivered: Vec<Delivered>) -> LiveSession {
        LiveSession {
            harness: "claude".into(),
            id: s.to_string(),
            open: true,
            delivered,
            ..Default::default()
        }
    }

    fn closed(s: Uuid) -> LiveSession {
        LiveSession {
            open: false,
            ..open(s, vec![])
        }
    }

    fn msg(reply: Option<&str>) -> Delivered {
        Delivered {
            msg_id: "m1".into(),
            reply: reply.map(String::from),
        }
    }

    fn status(l: &Ledger, t: TaskId) -> (TaskStatus, String, Option<String>) {
        let s = l.show(t).unwrap().0;
        (s.status, s.activity, s.summary)
    }

    #[test]
    fn a_delivered_message_frees_its_worker_and_is_done_when_answered() {
        let (mut l, w, s, task) = delivered(false);
        assert_eq!(status(&l, task).0, TaskStatus::Delivered);
        // The worker takes the next task meanwhile.
        let (next, eff) = l.submit("other".into(), None, Some("claude".into()), None, vec![]);
        assert!(matches!(&eff[..], [Effect::Assign { task, .. }] if task.id == next));
        // Not in the report yet, then in its conversation, then answered.
        let at = |l: &mut Ledger, d| l.live_sessions(w, 1, vec![open(s, d)]);
        assert!(at(&mut l, vec![]).is_empty());
        at(&mut l, vec![msg(None)]);
        assert_eq!(status(&l, task).1, "delivered");
        assert_eq!(l.sessions()[0].messages[0].status, TaskStatus::Delivered);
        let eff = at(&mut l, vec![msg(Some("Added one."))]);
        assert_eq!(
            status(&l, task),
            (TaskStatus::Done, "done".into(), Some("Added one.".into()))
        );
        // The usual push for a finished task, once.
        let pushed = |eff: &[Effect]| {
            eff.iter()
                .any(|e| matches!(e, Effect::Notify { title, .. } if title == "done · add a test"))
        };
        assert!(pushed(&eff));
        assert!(at(&mut l, vec![msg(Some("Added one."))]).is_empty());
        assert!(l.sessions()[0].messages[0].answered);
        // Already in the session: it can't be cancelled.
        let (mut l, _, _, task) = delivered(false);
        let refused = super::super::NotCancelled::Finished(TaskStatus::Delivered);
        assert_eq!(l.cancel(task).err(), Some(refused));
        // Answered before the worker said it delivered: done as soon as it does.
        let (mut l, w, s, task) = sent(None);
        l.live_sessions(w, 1, vec![open(s, vec![msg(Some("Done already."))])]);
        assert_eq!(status(&l, task).0, TaskStatus::Running);
        let ev = AgentEvent::Delivered {
            msg_id: "m1".into(),
            read: true,
            held: false,
        };
        assert!(pushed(&l.event(w, 1, task, ev)));
        assert_eq!(status(&l, task).2.as_deref(), Some("Done already."));
    }

    #[test]
    fn a_held_message_waits_for_approval_and_fails_if_declined() {
        let (mut l, w, s, task) = delivered(true);
        assert_eq!(status(&l, task).1, "waiting for approval in the terminal");
        let at = |state, why: Option<&str>| LiveSession {
            state,
            waiting_for: why.map(String::from),
            ..open(s, vec![])
        };
        l.live_sessions(
            w,
            1,
            vec![at(LiveState::Waiting, Some("permission prompt"))],
        );
        assert!(l.sessions()[0].messages[0].held);
        // Approved while busy: in its inbox until its next tool round.
        l.live_sessions(w, 1, vec![at(LiveState::Working, None)]);
        assert_eq!(status(&l, task).0, TaskStatus::Delivered);
        // At rest, no dialog, and it never arrived: declined (or expired).
        l.live_sessions(w, 1, vec![at(LiveState::Idle, None)]);
        let (st, _, why) = status(&l, task);
        assert_eq!(
            (st, why.as_deref()),
            (TaskStatus::Failed, Some("not accepted in its terminal"))
        );
        // Approved instead: in, and no longer held.
        let (mut l, w, s, task) = delivered(true);
        l.live_sessions(w, 1, vec![open(s, vec![msg(None)])]);
        assert_eq!(status(&l, task).1, "delivered");
        assert!(!l.sessions()[0].messages[0].held);
        // The terminal closed on it: not declined, continued in place.
        let (mut l, w, s, task) = delivered(true);
        l.live_sessions(w, 1, vec![closed(s)]);
        assert_eq!(status(&l, task).0, TaskStatus::Running);
    }

    #[test]
    fn a_session_closed_before_taking_it_in_is_continued_in_place() {
        let (mut l, w, s, task) = delivered(false);
        let eff = l.live_sessions(w, 1, vec![closed(s)]);
        // Sent again, now to a closed session: the worker continues it in place.
        assert!(matches!(&eff[..], [Effect::Assign { task: t, .. }, _] if t.id == task));
        assert_eq!(status(&l, task).0, TaskStatus::Running);
        // Read, then gone from what the worker reads (scrolled out) and closed: it stays
        // delivered, however long, and isn't sent twice.
        for (read, seen) in [(false, true), (true, false)] {
            let (mut l, w, s, task) = sent(Some((read, false)));
            if seen {
                l.live_sessions(w, 1, vec![open(s, vec![msg(None)])]);
            }
            assert!(l.live_sessions(w, 1, vec![closed(s)]).is_empty());
            assert_eq!(status(&l, task).0, TaskStatus::Delivered);
        }
        // Answered after a day: no longer followed.
        let (mut l, w, s, task) = sent(Some((true, false)));
        l.now_ms += 24 * 3600 * 1000;
        l.live_sessions(w, 1, vec![open(s, vec![msg(Some("late"))])]);
        assert_eq!(status(&l, task).0, TaskStatus::Delivered);
        // Kept like a finished task, then swept.
        l.now_ms += 40 * 24 * 3600 * 1000;
        assert_eq!(l.sweep().0, [task]);
    }

    #[test]
    fn a_message_to_a_busy_codex_session_waits_until_it_goes_quiet() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        let mut i = info("studio");
        i.deliver = true;
        l.connect(w, i, &[], 1, NOW);
        let s = Uuid::new_v4();
        let codex = |state, open| LiveSession {
            harness: "codex".into(),
            id: s.to_string(),
            state,
            open,
            ..Default::default()
        };
        l.live_sessions(w, 1, vec![codex(LiveState::Working, true)]);
        let to = SendTo {
            session: s,
            title: None,
            mode: SendMode::Deliver,
        };
        let text = "rename it".into();
        let (task, eff) = l
            .send_to_session(w, "codex".into(), text, to, vec![])
            .unwrap();
        assert!(eff.is_empty(), "held in the queue, not on the worker");
        // Its turn ended, but it was written a moment ago: maybe still open.
        assert!(l
            .live_sessions(w, 1, vec![codex(LiveState::Idle, true)])
            .is_empty());
        // Quiet, by its worker's clock: sent, to be continued in place.
        let eff = l.live_sessions(w, 1, vec![codex(LiveState::Idle, false)]);
        assert!(matches!(&eff[..], [Effect::Assign { task: t, .. }] if t.id == task));
    }

    #[test]
    fn a_worker_that_cant_deliver_gets_the_message_once_the_session_closes() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        let to = SendTo {
            session: Uuid::new_v4(),
            title: None,
            mode: SendMode::Deliver,
        };
        let (_, eff) = l
            .send_to_session(w, "claude".into(), "hi".into(), to, vec![])
            .unwrap();
        let [Effect::Assign { task, .. }] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(task.send_to.as_ref().unwrap().mode, SendMode::WhenClosed);
    }

    #[test]
    fn watchers_get_only_changes() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info("studio"), &[], 1, NOW);
        // The first diff is pushed, even empty.
        assert!(l.sessions_changed().is_some());
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
