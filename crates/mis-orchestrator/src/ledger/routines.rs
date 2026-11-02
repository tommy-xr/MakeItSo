//! Routine runs as ledger transitions: each run is an ordinary task tagged with
//! its routine's name, and the routine's state is saved with the ledger.

use chrono::TimeZone;
use mis_types::*;
use uuid::Uuid;

use super::{Effect, Ledger};
use crate::routine::{self, Def, Routine, State};

/// Harness for routines that don't name one, as for unplanned tasks.
const DEFAULT_HARNESS: &str = "claude";

impl Ledger {
    /// Start every routine in `defs` due now; `tz` reads cron patterns. A run
    /// that can't start keeps why as the routine's error and is retried next tick.
    pub fn routine_tick<Tz: TimeZone>(&mut self, defs: &[Def], tz: &Tz) -> Vec<Effect> {
        let now = self.now_ms;
        for r in defs.iter().flatten() {
            let s = (self.routines.entry(r.name.clone())).or_insert_with(|| State::new(now));
            if r.paused(s) {
                s.since = now;
            }
        }
        let due = routine::due(
            defs,
            &self.routines,
            |r| self.routine_runs(&r.name),
            now,
            tz,
        );
        let due: Vec<Routine> = due.into_iter().cloned().collect();
        let (mut effects, mut started) = (vec![], false);
        for r in &due {
            if let Ok((_, e)) = self.begin_routine(r) {
                effects.extend(e);
                started = true;
            }
        }
        // Once, if any started: clients see new tasks even while they wait for a worker.
        if started {
            effects.push(Effect::Broadcast(self.status()));
        }
        effects
    }

    /// Run `r` now, as a task. Err: why it can't (e.g. its worker is offline),
    /// also kept as the routine's error. Callers check `routine_runs` first.
    pub fn start_routine(&mut self, r: &Routine) -> Result<(TaskId, Vec<Effect>), String> {
        let (id, mut effects) = self.begin_routine(r)?;
        effects.push(Effect::Broadcast(self.status()));
        Ok((id, effects))
    }

    /// `start_routine`, less telling clients: a tick tells them once.
    fn begin_routine(&mut self, r: &Routine) -> Result<(TaskId, Vec<Effect>), String> {
        let started = self.enqueue_routine(r);
        let now = self.now_ms;
        let s = (self.routines.entry(r.name.clone())).or_insert_with(|| State::new(now));
        match started {
            Ok((id, effects)) => {
                s.last_run = Some(now);
                s.last_task = Some(id);
                s.error = None;
                Ok((id, effects))
            }
            Err(why) => {
                s.error = Some(why.clone());
                Err(why)
            }
        }
    }

    fn enqueue_routine(&mut self, r: &Routine) -> Result<(TaskId, Vec<Effect>), String> {
        let harness = r.harness.clone().unwrap_or_else(|| DEFAULT_HARNESS.into());
        let (id, effects) = match &r.worker {
            Some(name) => {
                let wid = self.worker_named(name)?;
                let task = Task {
                    id: Uuid::new_v4(),
                    prompt: r.run_prompt(),
                    cwd: r.cwd.clone(),
                    harness,
                    model: r.model.clone(),
                    resume: None,
                    send_to: None,
                };
                (self.enqueue_pinned(wid, task, vec![])).map_err(|why| format!("{name}: {why}"))?
            }
            // Queued for a harness no worker runs, it would hold the routine busy for good.
            None if !self.known_fleet().contains_key(&harness) => {
                return Err(format!("no worker runs {harness}"));
            }
            // With a harness, `submit` queues it without planning.
            None => self.submit(
                r.run_prompt(),
                r.cwd.clone(),
                Some(harness),
                r.model.clone(),
                vec![],
            ),
        };
        let t = self.tasks.iter_mut().find(|t| t.task.id == id);
        t.expect("just added").routine = Some(r.name.clone());
        Ok((id, effects))
    }

    /// The worker a routine names: by its card name, else the one online worker
    /// with that hostname (card names gain an id suffix once a hostname repeats).
    fn worker_named(&self, name: &str) -> Result<WorkerId, String> {
        if let Some(c) = self.cards().into_iter().find(|c| c.name == name) {
            return Ok(c.id);
        }
        let online = |w: &&super::Worker| w.conn.is_some() && w.info.hostname == name;
        let hosts: Vec<_> = self.workers.iter().filter(|(_, w)| online(w)).collect();
        match hosts[..] {
            [(wid, _)] => Ok(*wid),
            [] => Err(format!("no worker named {name}")),
            _ => Err(format!("{} online workers are named {name}", hosts.len())),
        }
    }

    /// Whether a run of routine `name` hasn't finished, and when its last one ended.
    pub fn routine_runs(&self, name: &str) -> routine::Runs {
        let mine = || (self.tasks.iter()).filter(|t| t.routine.as_deref() == Some(name));
        let last = (self.routines.get(name))
            .and_then(|s| s.last_task)
            .and_then(|id| mine().find(|t| t.task.id == id));
        routine::Runs {
            busy: mine().any(|t| !t.status.finished()),
            ended: last.and_then(|t| t.finished_at),
        }
    }

    /// Pause or resume routine `name`, whatever its file says. Either way, runs
    /// missed so far are skipped.
    pub fn pause_routine(&mut self, name: &str, paused: bool) {
        let now = self.now_ms;
        let s = (self.routines.entry(name.to_string())).or_insert_with(|| State::new(now));
        s.paused = Some(paused);
        s.since = now;
    }

    /// `defs` as listed, with their schedules and last runs.
    pub fn routine_views<Tz: TimeZone>(&self, defs: &[Def], tz: &Tz) -> Vec<RoutineView> {
        defs.iter()
            .map(|d| {
                let name = d.as_ref().map_or("", |r| r.name.as_str());
                let state = d.as_ref().ok().and_then(|r| self.routines.get(&r.name));
                let last = state.and_then(|s| s.last_task);
                let status = last.and_then(|id| self.tasks.iter().find(|t| t.task.id == id));
                let runs = self.routine_runs(name);
                routine::view(d, state, runs, status.map(|t| t.status), self.now_ms, tz)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn defs(text: &str) -> Vec<Def> {
        routine::parse_all(vec![("r.md".into(), Ok(text.into()))])
    }

    fn worker(l: &mut Ledger, host: &str) -> WorkerId {
        let wid = Uuid::new_v4();
        let info = WorkerInfo {
            hostname: host.into(),
            harnesses: vec!["claude".into()],
            ..Default::default()
        };
        l.connect(wid, info, &[], 1, std::time::Duration::ZERO);
        wid
    }

    #[test]
    fn a_due_routine_queues_a_tagged_task_and_waits_for_it() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\nmodel: haiku\ncwd: /x\n---\nCount files.");
        let eff = l.routine_tick(&ds, &Utc);
        assert!(matches!(eff.last(), Some(Effect::Broadcast(_))));
        let ServerMsg::Status { tasks, .. } = l.status() else {
            panic!()
        };
        let [t] = &tasks[..] else { panic!("{tasks:?}") };
        assert_eq!(t.routine.as_deref(), Some("r"));
        assert_eq!(
            (t.harness.as_str(), t.model.as_deref()),
            ("claude", Some("haiku"))
        );
        assert!(t.prompt.contains("report only") && t.prompt.ends_with("Count files."));
        // Still queued: no second run, even long past its next.
        l.now_ms = 10 * 3600 * 1000;
        assert!(l.routine_tick(&ds, &Utc).is_empty());
        let v = &l.routine_views(&ds, &Utc)[0];
        assert_eq!(
            (v.last_task, v.last_status),
            (Some(t.id), Some(TaskStatus::Queued))
        );
    }

    #[test]
    fn a_cancelled_run_frees_the_routine() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\n---\ngo");
        l.routine_tick(&ds, &Utc);
        let id = l.tasks[0].task.id;
        l.cancel(id).unwrap();
        l.now_ms = 2 * 3600 * 1000;
        l.routine_tick(&ds, &Utc);
        assert_eq!(l.tasks.len(), 2, "a cancelled run must not block the next");
    }

    #[test]
    fn pinned_routine_needs_its_worker_and_retries() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\nworker: mac\n---\ngo");
        assert!(l.routine_tick(&ds, &Utc).is_empty());
        let v = &l.routine_views(&ds, &Utc)[0];
        assert_eq!(v.error.as_deref(), Some("no worker named mac"));
        assert_eq!(v.last_at, None);
        // Next tick, with the worker: it runs there, and the error clears.
        let wid = worker(&mut l, "mac");
        let eff = l.routine_tick(&ds, &Utc);
        assert!(matches!(&eff[0], Effect::Assign { task, .. } if task.cwd.is_none()));
        let v = &l.routine_views(&ds, &Utc)[0];
        assert_eq!(
            (v.error.as_deref(), v.last_status),
            (None, Some(TaskStatus::Running))
        );
        let ServerMsg::Status { tasks, .. } = l.status() else {
            panic!()
        };
        assert_eq!(tasks[0].worker, Some(wid));
    }

    #[test]
    fn a_harness_no_worker_runs_is_refused_not_queued_for_good() {
        let mut l = Ledger::default();
        worker(&mut l, "mac");
        let ds = defs("---\nevery: 1h\nharness: codex\n---\ngo");
        assert!(l.routine_tick(&ds, &Utc).is_empty());
        let v = &l.routine_views(&ds, &Utc)[0];
        assert_eq!(v.error.as_deref(), Some("no worker runs codex"));
        assert!(!l.routine_runs("r").busy);
    }

    #[test]
    fn worker_is_found_by_hostname_once_its_card_name_gains_a_suffix() {
        let mut l = Ledger::default();
        let old = worker(&mut l, "mac");
        l.disconnect(old, 1);
        let new = worker(&mut l, "mac");
        let ds = defs("---\nevery: 1h\nworker: mac\n---\ngo");
        let eff = l.routine_tick(&ds, &Utc);
        assert!(matches!(&eff[0], Effect::Assign { .. }), "{eff:?}");
        let ServerMsg::Status { tasks, .. } = l.status() else {
            panic!()
        };
        assert_eq!(tasks[0].worker, Some(new));
    }

    #[test]
    fn pause_and_resume_override_the_file() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\n---\ngo");
        l.pause_routine("r", true);
        assert!(l.routine_tick(&ds, &Utc).is_empty());
        assert!(l.routine_views(&ds, &Utc)[0].paused);
        l.pause_routine("r", false);
        assert!(!l.routine_tick(&ds, &Utc).is_empty());
    }

    #[test]
    fn state_survives_a_restart() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\n---\ngo");
        l.routine_tick(&ds, &Utc);
        l.pause_routine("r", true);
        let snap = l.snapshot(&Default::default());
        let r = Ledger::restore(&snap.ledger, |_| None, Default::default(), 0).unwrap();
        assert_eq!(r.ledger.routines, l.routines);
        assert!(r.ledger.routine_runs("r").busy);
    }
}
