//! Pushes for live sessions the user watches (`SessionRow::watched`) when one starts
//! needing them: into waiting (a prompt, or its turn ended) or stalled. Once per
//! transition and at most once per `COOLDOWN_MS` a session (a prompt may follow a lesser
//! push); never for a worker's first report after it (re)connects (that's what happened
//! while it was away), nor for a turn that just answered a message from MIS (that
//! message's task pushed already).

use mis_types::*;
use serde::{Deserialize, Serialize};

use super::{sessions::Live, Effect, Ledger};

/// A live session, as its worker lists it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub worker: WorkerId,
    pub harness: String,
    pub id: String,
}

/// A session pushes at most once this often, however it flaps; but a prompt (urgent)
/// still follows a push that wasn't.
const COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// Its turn ended this soon after it answered a message from MIS: the "done" push said so.
const ANSWERED_MS: u64 = 60 * 1000;
const NAME_CHARS: usize = 60;
/// The one line of a session a push carries: it transits a browser vendor's service.
const BODY_CHARS: usize = 120;

/// One session in a report, with what `alert` weighs.
struct Seen<'a> {
    s: &'a LiveSession,
    /// Its state in the worker's previous report; None if it wasn't in it.
    was: Option<LiveState>,
    watched: bool,
    /// Its last push: Unix millis, and whether urgent.
    alerted: Option<(u64, bool)>,
    /// Unix millis it last answered a message from MIS.
    answered: Option<u64>,
}

/// Pure: the push (title, body, urgent) `seen` owes at `now`, if any.
fn alert(seen: &Seen, now: u64) -> Option<(String, String, bool)> {
    let s = seen.s;
    let recent = |at: Option<u64>, ms| at.is_some_and(|at| now.saturating_sub(at) < ms);
    let urgent = s.state == LiveState::Waiting && s.waiting_for.is_some();
    let cooling = (seen.alerted)
        .is_some_and(|(at, was_urgent)| recent(Some(at), COOLDOWN_MS) && (was_urgent || !urgent));
    // Not in the last report (it fell out of the worker's list and came back): no
    // change seen.
    if !seen.watched || seen.was.is_none_or(|was| was == s.state) || cooling {
        return None;
    }
    let name = (s.name.as_deref().or(s.title.as_deref())).unwrap_or(&s.id);
    let name = first_line(name, NAME_CHARS);
    let last = first_line(s.last.as_deref().unwrap_or(""), BODY_CHARS);
    match s.state {
        LiveState::Waiting if !urgent && recent(seen.answered, ANSWERED_MS) => None,
        LiveState::Waiting => Some((
            format!("{name} is waiting for you"),
            (s.waiting_for.as_deref()).map_or(last, |w| first_line(w, BODY_CHARS)),
            urgent,
        )),
        LiveState::Stalled => {
            let quiet = now.saturating_sub(s.updated_ms) / 60_000;
            Some((
                format!("{name} may be stalled · quiet {quiet}m"),
                last,
                false,
            ))
        }
        LiveState::Working | LiveState::Idle | LiveState::Shell => None,
    }
}

impl Ledger {
    /// Turn live session `key`'s bell on (only while its worker lists it) or off.
    pub fn session_bell(&mut self, key: SessionKey, on: bool) -> Result<Vec<Effect>, String> {
        if !on {
            self.bells.remove(&key);
            return Ok(vec![]);
        }
        let listed = (self.live.get(&key.worker)).is_some_and(|l| {
            (l.sessions.iter()).any(|s| s.harness == key.harness && s.id == key.id)
        });
        if !listed {
            return Err("no such live session".into());
        }
        self.bells.insert(key);
        Ok(vec![])
    }

    pub(super) fn bell(&self, worker: WorkerId, s: &LiveSession) -> bool {
        let (harness, id) = (s.harness.clone(), s.id.clone());
        (self.bells).contains(&SessionKey {
            worker,
            harness,
            id,
        })
    }

    /// The pushes `wid`'s latest report owes, against its previous one (`was`).
    pub(super) fn alerts(&mut self, wid: WorkerId, was: Option<&Live>) -> Vec<Effect> {
        let now = self.now_ms;
        self.alerted
            .retain(|_, (at, _)| now.saturating_sub(*at) < COOLDOWN_MS);
        let state = |s: &LiveSession| {
            let mut sessions = was?.sessions.iter();
            Some(
                sessions
                    .find(|x| x.harness == s.harness && x.id == s.id)?
                    .state,
            )
        };
        let mut effects = vec![];
        for r in self.sessions().into_iter().filter(|r| r.worker == wid) {
            let key = SessionKey {
                worker: wid,
                harness: r.session.harness.clone(),
                id: r.session.id.clone(),
            };
            let answered = |m: &SessionMessage| {
                let t = (self.tasks.iter()).find(|t| m.answered && t.task.id == m.task);
                t?.finished_at
            };
            let seen = Seen {
                s: &r.session,
                was: state(&r.session),
                watched: r.watched,
                alerted: self.alerted.get(&key).copied(),
                answered: r.messages.iter().filter_map(answered).max(),
            };
            let Some((title, body, urgent)) = alert(&seen, now) else {
                continue;
            };
            self.alerted.insert(key.clone(), (now, urgent));
            effects.push(Effect::NotifySession {
                worker: wid,
                harness: key.harness,
                id: key.id,
                title,
                body,
                urgent,
            });
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    const MIN: u64 = 60 * 1000;
    const NOW: u64 = 1_000 * MIN;

    fn waiting(why: Option<&str>) -> LiveSession {
        LiveSession {
            harness: "claude".into(),
            id: "s".into(),
            name: Some("ci-fix".into()),
            title: Some("Upgrade the CI runners".into()),
            state: LiveState::Waiting,
            waiting_for: why.map(String::from),
            last: Some("Checking the workflow files.".into()),
            updated_ms: NOW - 12 * MIN,
            ..Default::default()
        }
    }

    fn seen(s: &LiveSession, was: Option<LiveState>) -> Seen<'_> {
        Seen {
            s,
            was,
            watched: true,
            alerted: None,
            answered: None,
        }
    }

    #[test]
    fn a_watched_session_pushes_on_turning_waiting_or_stalled() {
        let s = waiting(Some("permission prompt"));
        let got = alert(&seen(&s, Some(LiveState::Working)), NOW);
        let want = ("ci-fix is waiting for you", "permission prompt", true);
        assert_eq!(got, Some((want.0.into(), want.1.into(), want.2)));
        // Its turn ended: what it last said, not urgent; unnamed, by its title.
        let s = LiveSession {
            name: None,
            ..waiting(None)
        };
        let (title, body, urgent) = alert(&seen(&s, Some(LiveState::Working)), NOW).unwrap();
        assert_eq!(
            (title.as_str(), body.as_str(), urgent),
            (
                "Upgrade the CI runners is waiting for you",
                "Checking the workflow files.",
                false
            )
        );
        let s = LiveSession {
            state: LiveState::Stalled,
            ..waiting(None)
        };
        let (title, body, _) = alert(&seen(&s, Some(LiveState::Working)), NOW).unwrap();
        assert_eq!(title, "ci-fix may be stalled · quiet 12m");
        assert_eq!(body, "Checking the workflow files.");
        // Not in the last report (fell out of the worker's list): no change seen.
        assert_eq!(alert(&seen(&waiting(None), None), NOW), None);
    }

    #[test]
    fn no_push_while_it_stays_put_unwatched_or_at_rest() {
        let s = waiting(None);
        assert_eq!(alert(&seen(&s, Some(LiveState::Waiting)), NOW), None);
        let unwatched = Seen {
            watched: false,
            ..seen(&s, Some(LiveState::Working))
        };
        assert_eq!(alert(&unwatched, NOW), None);
        for state in [LiveState::Working, LiveState::Idle, LiveState::Shell] {
            let s = LiveSession {
                state,
                ..waiting(None)
            };
            assert_eq!(alert(&seen(&s, Some(LiveState::Waiting)), NOW), None);
        }
    }

    #[test]
    fn a_flapping_session_pushes_once_per_cooldown_but_a_prompt_gets_through() {
        let (ended, prompt) = (waiting(None), waiting(Some("permission prompt")));
        let at = |s, alerted| Seen {
            alerted: Some(alerted),
            ..seen(s, Some(LiveState::Working))
        };
        assert_eq!(alert(&at(&ended, (NOW - 9 * MIN, false)), NOW), None);
        assert!(alert(&at(&ended, (NOW - 10 * MIN, false)), NOW).is_some());
        // After a lesser push, a prompt still goes out; after a prompt, it waits.
        assert!(alert(&at(&prompt, (NOW - MIN, false)), NOW).is_some());
        assert_eq!(alert(&at(&prompt, (NOW - MIN, true)), NOW), None);
    }

    #[test]
    fn waiting_after_answering_a_message_from_mis_is_not_pushed_again() {
        let s = waiting(None);
        let at = |answered| Seen {
            answered: Some(answered),
            ..seen(&s, Some(LiveState::Working))
        };
        assert_eq!(alert(&at(NOW - 59_000), NOW), None);
        assert!(alert(&at(NOW - 60_000), NOW).is_some());
        // A prompt after the answer is news.
        let prompt = waiting(Some("permission prompt"));
        let s = Seen {
            answered: Some(NOW),
            ..seen(&prompt, Some(LiveState::Working))
        };
        assert!(alert(&s, NOW).is_some());
        // A stall isn't an answer.
        let stalled = LiveSession {
            state: LiveState::Stalled,
            ..waiting(None)
        };
        let s = Seen {
            answered: Some(NOW),
            ..seen(&stalled, Some(LiveState::Working))
        };
        assert!(alert(&s, NOW).is_some());
    }

    #[test]
    fn the_snippet_is_one_clipped_line() {
        let s = LiveSession {
            waiting_for: Some(format!("{}\nsecond line", "x".repeat(300))),
            ..waiting(None)
        };
        let (_, body, _) = alert(&seen(&s, Some(LiveState::Working)), NOW).unwrap();
        assert_eq!(body.chars().count(), BODY_CHARS);
        assert!(!body.contains('\n'));
    }

    fn info() -> WorkerInfo {
        WorkerInfo {
            hostname: "demo-mac".into(),
            harnesses: vec!["claude".into()],
            deliver: true,
            ..Default::default()
        }
    }

    fn live(state: LiveState) -> LiveSession {
        LiveSession {
            state,
            updated_ms: NOW,
            ..waiting(None)
        }
    }

    fn pushes(effects: &[Effect]) -> Vec<&str> {
        let titles = effects.iter().filter_map(|e| match e {
            Effect::NotifySession { title, .. } => Some(title.as_str()),
            _ => None,
        });
        titles.collect()
    }

    fn key(w: WorkerId) -> SessionKey {
        SessionKey {
            worker: w,
            harness: "claude".into(),
            id: "s".into(),
        }
    }

    /// A ledger at `NOW` with worker `w` reporting session `s` working.
    fn ledger() -> (Ledger, WorkerId) {
        let mut l = Ledger {
            now_ms: NOW,
            ..Default::default()
        };
        let w = Uuid::new_v4();
        l.connect(w, info(), &[], 1, Duration::from_secs(1));
        l.live_sessions(w, 1, vec![live(LiveState::Working)]);
        (l, w)
    }

    #[test]
    fn only_a_belled_session_pushes_and_the_bell_is_saved() {
        let (mut l, w) = ledger();
        assert!(pushes(&l.live_sessions(w, 1, vec![live(LiveState::Waiting)])).is_empty());
        l.live_sessions(w, 1, vec![live(LiveState::Working)]);
        let stranger = SessionKey {
            id: "nope".into(),
            ..key(w)
        };
        assert!(l.session_bell(stranger, true).is_err());
        l.session_bell(key(w), true).unwrap();
        assert!(l.sessions()[0].bell && l.sessions()[0].watched);
        let eff = l.live_sessions(w, 1, vec![live(LiveState::Waiting)]);
        assert_eq!(pushes(&eff), ["ci-fix is waiting for you"]);
        let [Effect::NotifySession {
            worker,
            harness,
            id,
            ..
        }, ..] = &eff[..]
        else {
            panic!("{eff:?}")
        };
        assert_eq!((*worker, harness.as_str(), id.as_str()), (w, "claude", "s"));
        // Kept across a restart.
        let snap = l.snapshot(&Default::default());
        let back = Ledger::restore(&snap.ledger, |_| None, Duration::ZERO, NOW).unwrap();
        assert!(back.ledger.bells.contains(&key(w)));
        // Off: quiet again.
        l.session_bell(key(w), false).unwrap();
        l.now_ms += COOLDOWN_MS;
        l.live_sessions(w, 1, vec![live(LiveState::Working)]);
        assert!(pushes(&l.live_sessions(w, 1, vec![live(LiveState::Waiting)])).is_empty());
    }

    #[test]
    fn a_reconnecting_workers_first_report_never_pushes() {
        let (mut l, w) = ledger();
        l.session_bell(key(w), true).unwrap();
        l.disconnect(w, 1);
        l.connect(w, info(), &[], 2, Duration::from_secs(2));
        // It turned waiting while away: not news.
        assert!(pushes(&l.live_sessions(w, 2, vec![live(LiveState::Waiting)])).is_empty());
        l.live_sessions(w, 2, vec![live(LiveState::Working)]);
        let eff = l.live_sessions(w, 2, vec![live(LiveState::Waiting)]);
        assert_eq!(pushes(&eff).len(), 1);
        // Likewise after the orchestrator restarts.
        let snap = l.snapshot(&Default::default());
        let mut l = Ledger::restore(&snap.ledger, |_| None, Duration::ZERO, NOW)
            .unwrap()
            .ledger;
        l.now_ms += COOLDOWN_MS;
        l.connect(w, info(), &[], 3, Duration::from_secs(3));
        assert!(pushes(&l.live_sessions(w, 3, vec![live(LiveState::Stalled)])).is_empty());
    }

    #[test]
    fn flapping_pushes_once_until_the_cooldown_passes() {
        let (mut l, w) = ledger();
        l.session_bell(key(w), true).unwrap();
        let mut n = 0;
        for state in [LiveState::Waiting, LiveState::Working].repeat(3) {
            n += pushes(&l.live_sessions(w, 1, vec![live(state)])).len();
        }
        assert_eq!(n, 1);
        l.now_ms += COOLDOWN_MS;
        assert_eq!(
            pushes(&l.live_sessions(w, 1, vec![live(LiveState::Waiting)])).len(),
            1
        );
    }

    #[test]
    fn a_declined_message_is_no_answer() {
        let (mut l, w) = ledger();
        let s = Uuid::new_v4();
        let at = |state| LiveSession {
            id: s.to_string(),
            open: true,
            ..live(state)
        };
        let to = SendTo {
            session: s,
            title: None,
            mode: SendMode::Deliver,
        };
        let (task, _) =
            (l.send_to_session(w, "claude".into(), "rename it".into(), to, vec![])).unwrap();
        let ev = AgentEvent::Delivered {
            msg_id: "m1".into(),
            read: false,
            held: true,
        };
        l.event(w, 1, task, ev);
        // Held, then at rest without it: declined in its terminal.
        l.live_sessions(w, 1, vec![at(LiveState::Idle)]);
        assert_eq!(l.show(task).unwrap().0.status, TaskStatus::Failed);
        l.live_sessions(w, 1, vec![at(LiveState::Working)]);
        let eff = l.live_sessions(w, 1, vec![at(LiveState::Waiting)]);
        assert_eq!(pushes(&eff), ["ci-fix is waiting for you"]);
    }

    #[test]
    fn a_session_messaged_from_mis_is_watched_for_a_day_and_its_answer_pushes_once() {
        let (mut l, w) = ledger();
        let s = Uuid::new_v4();
        let at = |state, reply: Option<&str>| LiveSession {
            id: s.to_string(),
            open: true,
            delivered: vec![Delivered {
                msg_id: "m1".into(),
                reply: reply.map(String::from),
            }],
            ..live(state)
        };
        let to = SendTo {
            session: s,
            title: None,
            mode: SendMode::Deliver,
        };
        let (task, _) =
            (l.send_to_session(w, "claude".into(), "add a test".into(), to, vec![])).unwrap();
        let ev = AgentEvent::Delivered {
            msg_id: "m1".into(),
            read: true,
            held: false,
        };
        l.event(w, 1, task, ev);
        l.live_sessions(w, 1, vec![at(LiveState::Working, None)]);
        assert!(l.sessions()[0].watched && !l.sessions()[0].bell);
        // It answers and its turn ends: the task's "done" push, not a second one.
        let eff = l.live_sessions(w, 1, vec![at(LiveState::Waiting, Some("Added."))]);
        let done = |e: &Effect| matches!(e, Effect::Notify { task: t, .. } if *t == task);
        assert_eq!(
            (eff.iter().filter(|e| done(e)).count(), pushes(&eff)),
            (1, vec![])
        );
        // Later it asks for something: that pushes.
        l.now_ms += ANSWERED_MS;
        l.live_sessions(w, 1, vec![at(LiveState::Working, Some("Added."))]);
        let eff = l.live_sessions(w, 1, vec![at(LiveState::Waiting, Some("Added."))]);
        assert_eq!(pushes(&eff), ["ci-fix is waiting for you"]);
        // A day after the message, no longer watched.
        l.now_ms = NOW + WATCH_MS;
        l.live_sessions(w, 1, vec![at(LiveState::Working, Some("Added."))]);
        assert!(!l.sessions()[0].watched);
        assert!(pushes(&l.live_sessions(w, 1, vec![at(LiveState::Waiting, None)])).is_empty());
    }
}
