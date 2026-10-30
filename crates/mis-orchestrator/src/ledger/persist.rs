//! The ledger's durable form: every task and known worker, versioned, without
//! connection state. Transcripts are saved apart, so a finished one (it never
//! changes again) is written once instead of on every save. Pure: `store` does
//! the files.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use mis_types::*;
use serde::{Deserialize, Serialize};

use super::{size, Effect, Ledger, TaskEntry, Worker};

/// Bump on any change this build's reader can't take; a mismatched file is set aside.
pub const VERSION: u32 = 1;
/// Finished tasks are forgotten this long after they end.
pub const KEEP_MS: u64 = 30 * 24 * 3600 * 1000;
/// Why a task still being planned when the orchestrator stopped failed.
pub const PLANNING_LOST: &str = "orchestrator restarted while planning; send it again";

#[derive(Serialize, Deserialize)]
struct Saved<'a> {
    version: u32,
    /// Known workers, restored offline.
    workers: BTreeMap<WorkerId, WorkerInfo>,
    /// In submission order, without transcripts.
    tasks: Cow<'a, [TaskEntry]>,
}

/// One save: the ledger file and each transcript that may have changed.
pub struct Snapshot {
    pub ledger: String,
    pub transcripts: Vec<Transcript>,
}

pub struct Transcript {
    pub task: TaskId,
    /// The task has finished: this is its transcript for good.
    pub done: bool,
    /// The retained events, a JSON array.
    pub json: String,
}

/// A saved ledger rebuilt, with what the shell must do first.
#[derive(Default)]
pub struct Restored {
    pub ledger: Ledger,
    pub effects: Vec<Effect>,
    /// Tasks finished when saved, so their saved transcripts are final.
    pub written: HashSet<TaskId>,
}

fn finished(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Done | TaskStatus::Failed)
}

fn to_json<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).expect("ledger types serialize")
}

impl Ledger {
    /// Everything to save, less the transcripts in `written`: finished ones
    /// already saved, which can't have changed.
    pub fn snapshot(&self, written: &HashSet<TaskId>) -> Snapshot {
        let saved = Saved {
            version: VERSION,
            workers: (self.workers.iter())
                .map(|(id, w)| (*id, w.info.clone()))
                .collect(),
            tasks: Cow::Borrowed(&self.tasks),
        };
        let transcripts = (self.tasks.iter())
            .filter(|t| !written.contains(&t.task.id))
            .map(|t| Transcript {
                task: t.task.id,
                done: finished(t.status),
                json: to_json(&t.events),
            })
            .collect();
        Snapshot {
            ledger: to_json(&saved),
            transcripts,
        }
    }

    /// Rebuild a saved ledger; Err if it isn't one this build reads. `transcript`
    /// gives a task's saved transcript; without one the task shows as truncated.
    /// A restart drops every connection, so workers come back offline with the
    /// usual reconnect grace: a running task carries on if its worker returns
    /// still running it, and fails like on any worker loss otherwise. The
    /// planner's process is gone, so tasks it was planning fail.
    pub fn restore(
        json: &str,
        transcript: impl Fn(TaskId) -> Option<String>,
        now: Duration,
        now_ms: u64,
    ) -> Result<Restored, String> {
        #[derive(Deserialize)]
        struct Header {
            version: u32,
        }
        let header: Header = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if header.version != VERSION {
            let v = header.version;
            return Err(format!("format version {v}; this build reads {VERSION}"));
        }
        let saved: Saved = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut l = Ledger {
            now_ms,
            ..Default::default()
        };
        let mut effects = vec![];
        for (wid, info) in saved.workers {
            let generation = 1;
            let w = Worker {
                info,
                conn: None,
                generation,
                stats: None,
                last_seen: now,
            };
            l.workers.insert(wid, w);
            effects.push(Effect::StartGrace { wid, generation });
        }
        l.tasks = saved.tasks.into_owned();
        let written = (l.tasks.iter())
            .filter(|t| finished(t.status))
            .map(|t| t.task.id)
            .collect();
        for t in &mut l.tasks {
            let events = transcript(t.task.id).and_then(|j| serde_json::from_str(&j).ok());
            match events {
                Some(events) => {
                    t.events = events;
                    t.bytes = t.events.iter().map(size).sum();
                }
                None => t.truncated = true,
            }
            if t.status == TaskStatus::Planning {
                t.status = TaskStatus::Failed;
                let summary = Some(PLANNING_LOST.into());
                t.record(&AgentEvent::Finished { ok: false, summary }, now_ms);
                effects.extend(t.notify());
            }
        }
        l.queue = (l.tasks.iter())
            .filter(|t| t.status == TaskStatus::Queued)
            .map(|t| t.task.id)
            .collect();
        Ok(Restored {
            ledger: l,
            effects,
            written,
        })
    }

    /// Forget tasks that finished over `KEEP_MS` ago; returns their ids.
    pub fn sweep(&mut self) -> (Vec<TaskId>, Vec<Effect>) {
        let cutoff = self.now_ms.saturating_sub(KEEP_MS);
        let old = |t: &TaskEntry| finished(t.status) && t.finished_at.is_some_and(|f| f < cutoff);
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.tasks).into_iter().partition(old);
        self.tasks = kept;
        let gone: Vec<TaskId> = gone.iter().map(|t| t.task.id).collect();
        let effects = match gone.is_empty() {
            true => vec![],
            false => vec![Effect::Broadcast(self.status())],
        };
        (gone, effects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    const NOW: Duration = Duration::from_secs(1);
    const DAY_MS: u64 = 24 * 3600 * 1000;

    fn info() -> WorkerInfo {
        WorkerInfo {
            hostname: "h".into(),
            os: "o".into(),
            arch: "a".into(),
            harnesses: vec!["claude".into()],
            roots: vec![],
            models: Default::default(),
            ..Default::default()
        }
    }

    fn submit(l: &mut Ledger, harness: Option<&str>) -> TaskId {
        let harness = harness.map(String::from);
        l.submit("do it".into(), None, harness, None, vec![]).0
    }

    fn status_of(l: &Ledger, id: TaskId) -> TaskSummary {
        let (s, _, _) = l.show(id).unwrap();
        s
    }

    /// Save `l` and restore it with every transcript.
    fn restored(l: &Ledger) -> Restored {
        let snap = l.snapshot(&HashSet::new());
        let files: HashMap<TaskId, String> = (snap.transcripts.into_iter())
            .map(|t| (t.task, t.json))
            .collect();
        Ledger::restore(&snap.ledger, |id| files.get(&id).cloned(), NOW, l.now_ms).unwrap()
    }

    fn round_trip(l: &Ledger) -> (Ledger, Vec<Effect>) {
        let r = restored(l);
        (r.ledger, r.effects)
    }

    /// A worker that finished a task with evidence and an answer (seen), failed
    /// one on a usage limit, and is running a third; a fourth waits for codex.
    fn busy_ledger() -> (Ledger, WorkerId, [TaskId; 4]) {
        let mut l = Ledger {
            now_ms: 1_000,
            ..Default::default()
        };
        let w = Uuid::new_v4();
        l.connect(w, info(), &[], 1, NOW);
        let done = submit(&mut l, Some("claude"));
        let limited = submit(&mut l, Some("claude"));
        let running = submit(&mut l, Some("claude"));
        let queued = submit(&mut l, Some("codex"));
        let pr = "https://github.com/o/r/pull/7";
        for e in [
            AgentEvent::Started {
                session: Some("s1".into()),
                model: Some("m".into()),
            },
            AgentEvent::Text {
                text: format!("opened {pr}"),
            },
            AgentEvent::Evidence(Link {
                kind: LinkKind::Test,
                url: String::new(),
                label: "cargo test".into(),
                ok: Some(true),
                detail: None,
                pr: None,
            }),
            AgentEvent::Usage {
                input: 10,
                output: 20,
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("did it".into()),
            },
        ] {
            l.event(w, 1, done, e);
        }
        let pr_status = PrStatus {
            state: PrState::Open,
            checks: Checks::Passing,
            failing: vec![],
            mergeable: Some(true),
            review: None,
            head: "abc".into(),
            observed_at: 0,
        };
        l.pr_status(w, 1, done, pr, pr_status);
        l.seen(done);
        // Already reset, so it doesn't hold the next task back.
        let hit = AgentEvent::Limited { resets_at: Some(0) };
        l.event(w, 1, limited, hit);
        let failed = AgentEvent::Finished {
            ok: false,
            summary: Some("limit".into()),
        };
        l.event(w, 1, limited, failed);
        l.event(w, 1, running, AgentEvent::Text { text: "hm".into() });
        (l, w, [done, limited, running, queued])
    }

    #[test]
    fn round_trip_keeps_tasks_transcripts_evidence_and_attention() {
        let (l, w, ids) = busy_ledger();
        let Restored {
            ledger: r,
            effects,
            written,
        } = restored(&l);
        assert_eq!(written, HashSet::from([ids[0], ids[1]]));
        for id in ids {
            assert_eq!(l.show(id), r.show(id), "task {id}");
        }
        let done = status_of(&r, ids[0]);
        assert_eq!(done.summary.as_deref(), Some("did it"));
        assert_eq!(done.links.len(), 2);
        assert!(done.links[0].pr.is_some(), "PR status kept");
        assert_eq!(done.attention, Attention::None, "seen kept");
        assert_eq!(status_of(&r, ids[1]).attention, Attention::Limited);
        let ServerMsg::Status { workers, .. } = r.status() else {
            panic!()
        };
        assert_eq!((workers[0].id, workers[0].online), (w, false));
        assert_eq!(
            effects,
            [Effect::StartGrace {
                wid: w,
                generation: 1
            }],
            "worker gets the reconnect grace"
        );
    }

    #[test]
    fn running_task_carries_on_if_its_worker_returns_with_it() {
        let (l, w, [_, _, running, queued]) = busy_ledger();
        let (mut r, _) = round_trip(&l);
        assert_eq!(status_of(&r, running).status, TaskStatus::Running);
        r.connect(w, info(), &[running], 9, NOW);
        let finish = AgentEvent::Finished {
            ok: true,
            summary: None,
        };
        r.event(w, 9, running, finish);
        assert_eq!(status_of(&r, running).status, TaskStatus::Done);
        assert_eq!(status_of(&r, queued).status, TaskStatus::Queued);
        // The stale restore grace can't expire the reconnected worker.
        assert!(r.expire(w, 1).is_empty());
    }

    #[test]
    fn running_task_fails_if_its_worker_returns_without_it_or_not_at_all() {
        let (l, w, [_, _, running, _]) = busy_ledger();
        let (mut r, _) = round_trip(&l);
        r.connect(w, info(), &[], 9, NOW);
        let s = status_of(&r, running);
        assert_eq!(s.status, TaskStatus::Failed);
        assert_eq!(s.summary.as_deref(), Some("worker restarted"));

        let (mut r, _) = round_trip(&l);
        r.expire(w, 1);
        let s = status_of(&r, running);
        assert_eq!(s.status, TaskStatus::Failed);
        assert_eq!(s.summary.as_deref(), Some("worker disconnected"));
    }

    #[test]
    fn queued_task_runs_once_a_worker_joins() {
        let (l, _, [.., queued]) = busy_ledger();
        let (mut r, _) = round_trip(&l);
        let mut codex = info();
        codex.harnesses = vec!["codex".into()];
        let (_, effects) = r.connect(Uuid::new_v4(), codex, &[], 9, NOW);
        assert!(matches!(&effects[..], [Effect::Assign { task, .. }] if task.id == queued));
    }

    #[test]
    fn planning_task_fails_and_notifies() {
        let mut l = Ledger::default();
        let id = submit(&mut l, None);
        let Restored {
            ledger: r,
            effects,
            written,
        } = restored(&l);
        assert!(
            written.is_empty(),
            "its transcript changed, so is saved again"
        );
        let s = status_of(&r, id);
        assert_eq!(s.status, TaskStatus::Failed);
        assert_eq!(s.summary.as_deref(), Some(PLANNING_LOST));
        assert_eq!(s.attention, Attention::Failed);
        assert!(matches!(&effects[..], [Effect::Notify { task, .. }] if *task == id));
    }

    #[test]
    fn missing_transcript_shows_truncated() {
        let (l, _, [done, ..]) = busy_ledger();
        let snap = l.snapshot(&HashSet::new());
        let r = Ledger::restore(&snap.ledger, |_| None, NOW, 0)
            .unwrap()
            .ledger;
        let (s, events, truncated) = r.show(done).unwrap();
        assert!(events.is_empty() && truncated);
        assert_eq!(s.summary.as_deref(), Some("did it"));
    }

    #[test]
    fn written_finished_transcripts_are_not_saved_again() {
        let (l, _, [done, limited, running, queued]) = busy_ledger();
        let snap = l.snapshot(&HashSet::new());
        let got: Vec<_> = snap.transcripts.iter().map(|t| (t.task, t.done)).collect();
        let want = [
            (done, true),
            (limited, true),
            (running, false),
            (queued, false),
        ];
        assert_eq!(got, want);
        let snap = l.snapshot(&HashSet::from([done]));
        assert_eq!(snap.transcripts.len(), 3);
    }

    #[test]
    fn other_versions_and_garbage_are_refused() {
        let restore = |json| Ledger::restore(json, |_| None, NOW, 0).map(|_| ());
        let err = restore(r#"{"version":2,"anything":[]}"#).unwrap_err();
        assert!(err.contains("version 2"), "{err}");
        assert!(restore("not json").is_err());
        assert!(restore(r#"{"version":1}"#).is_err());
        assert!(restore(r#"{"version":1,"workers":{},"tasks":[]}"#).is_ok());
    }

    #[test]
    fn sweep_forgets_tasks_a_month_after_they_end() {
        let mut l = Ledger::default();
        let w = Uuid::new_v4();
        l.connect(w, info(), &[], 1, NOW);
        let finish = |l: &mut Ledger, at_day: u64| {
            l.now_ms = at_day * DAY_MS;
            let id = submit(l, Some("claude"));
            let ok = AgentEvent::Finished {
                ok: true,
                summary: None,
            };
            l.event(w, 1, id, ok);
            id
        };
        let old = finish(&mut l, 1);
        let recent = finish(&mut l, 3);
        let running = submit(&mut l, Some("claude"));
        l.now_ms = 32 * DAY_MS;
        let (gone, effects) = l.sweep();
        assert_eq!(gone, [old]);
        assert!(matches!(
            &effects[..],
            [Effect::Broadcast(ServerMsg::Status { .. })]
        ));
        assert!(l.show(old).is_none());
        assert!(l.show(recent).is_some() && l.show(running).is_some());
        assert!(l.sweep().0.is_empty());
    }
}
