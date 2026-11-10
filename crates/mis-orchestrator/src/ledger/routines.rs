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
        let mode = r.mode(self.routines.get(&r.name).unwrap_or(&State::default()));
        let task = Task {
            id: Uuid::new_v4(),
            prompt: r.run_prompt(mode),
            cwd: r.cwd.clone(),
            harness,
            model: r.model.clone(),
            resume: None,
            send_to: None,
            read_only: mode != RoutineMode::Live,
            diff_base: None,
            merge: r.merge_rule(),
        };
        let (id, effects) = match &r.worker {
            Some(name) => {
                let wid = self.worker_named(name)?;
                (self.enqueue_pinned(wid, task, vec![])).map_err(|why| format!("{name}: {why}"))?
            }
            // Queued for a harness no worker runs, it would hold the routine busy for good.
            None if !self.known_fleet().contains_key(&task.harness) => {
                return Err(format!("no worker runs {}", task.harness));
            }
            None => self.enqueue(super::TaskEntry::new(task, self.now_ms)),
        };
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.task.id == id)
            .expect("just added");
        (t.routine, t.routine_mode) = (Some(r.name.clone()), Some(mode));
        Ok((id, effects))
    }

    /// Switch `r` live (it acts) or back to dry run; Err if its file doesn't say
    /// `act: true`. Live pins `r` as it is now: once its prompt, merge policy or
    /// placement changes, it runs dry (see `forget_live`). Runs already queued keep
    /// what they were queued with, which going live didn't approve: queued dry runs
    /// stay dry, and going dry cancels queued live ones.
    pub fn set_live(&mut self, r: &Routine, live: bool) -> Result<Vec<Effect>, String> {
        if !r.act {
            return Err(format!(
                "{} only reports: add `act: true` to {}",
                r.name, r.file
            ));
        }
        let now = self.now_ms;
        let s = (self.routines.entry(r.name.clone())).or_insert_with(|| State::new(now));
        (s.live, s.live_pin, s.changed) = (live, live.then(|| r.pin()), false);
        Ok(match live {
            true => vec![],
            false => self.cancel_queued_live(&r.name),
        })
    }

    /// Live lasts only while a valid file says `act: true` and pins what went
    /// live: dropping `act`, a rename or a broken file sends it back to dry run
    /// once `act` returns, and an edit to what went live (marked `changed`) does
    /// at once. Either way its queued live runs are cancelled. True if that ended one.
    pub fn forget_live(&mut self, defs: &[Def]) -> (bool, Vec<Effect>) {
        let mut ended = vec![];
        for (name, s) in &mut self.routines {
            if !s.live {
                continue;
            }
            match defs.iter().flatten().find(|r| &r.name == name && r.act) {
                Some(r) if s.live_pin.as_deref() == Some(r.pin().as_str()) => continue,
                Some(_) => s.changed = true,
                None => {}
            }
            (s.live, s.live_pin) = (false, None);
            ended.push(name.clone());
        }
        let effects = ended
            .iter()
            .flat_map(|n| self.cancel_queued_live(n))
            .collect();
        (!ended.is_empty(), effects)
    }

    /// Cancel routine `name`'s queued live runs: what they would run is no longer approved.
    fn cancel_queued_live(&mut self, name: &str) -> Vec<Effect> {
        let live = |t: &&super::TaskEntry| {
            t.status == TaskStatus::Queued
                && t.routine.as_deref() == Some(name)
                && t.routine_mode == Some(RoutineMode::Live)
        };
        let ids: Vec<TaskId> = self.tasks.iter().filter(live).map(|t| t.task.id).collect();
        let cancelled = ids.into_iter().filter_map(|id| self.cancel(id).ok());
        cancelled.flat_map(|(_, effects)| effects).collect()
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

    /// Forget what an earlier routine named `name` left (resumed, live, last
    /// run), so a new file of that name starts as the file says.
    pub fn reset_routine(&mut self, name: &str) {
        self.routines
            .insert(name.to_string(), State::new(self.now_ms));
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

    /// The newest task's prompt, read-only flag and recorded mode.
    fn last_run(l: &Ledger) -> (String, bool, Option<RoutineMode>) {
        let t = l.tasks.last().unwrap();
        (
            t.task.prompt.clone(),
            t.task.read_only,
            t.summary().routine_mode,
        )
    }

    #[test]
    fn acting_routines_run_dry_until_live_and_runs_keep_their_mode() {
        let mut l = Ledger::default();
        let report = defs("---\nevery: 1h\n---\ngo");
        l.routine_tick(&report, &Utc);
        let (p, ro, mode) = last_run(&l);
        assert!(p.contains("report only") && ro && mode == Some(RoutineMode::Report));
        let r = report[0].clone().unwrap();
        assert!(l
            .set_live(&r, true)
            .unwrap_err()
            .contains("add `act: true`"));

        let act = defs("---\nevery: 1h\nact: true\n---\ngo");
        let r = act[0].clone().unwrap();
        l.start_routine(&r).unwrap();
        let (p, ro, mode) = last_run(&l);
        assert!(p.contains("dry run") && ro && mode == Some(RoutineMode::DryRun));
        // Finished before going live: it keeps its mode.
        l.cancel(l.tasks[1].task.id).unwrap();
        l.set_live(&r, true).unwrap();
        assert_eq!(l.routine_views(&act, &Utc)[0].mode, Some(RoutineMode::Live));
        l.start_routine(&r).unwrap();
        let (p, ro, mode) = last_run(&l);
        assert!(p.contains("(scheduled run, live)") && !ro && mode == Some(RoutineMode::Live));
        // The earlier runs still say how they ran.
        assert_eq!(l.tasks[1].summary().routine_mode, Some(RoutineMode::DryRun));
        l.set_live(&r, false).unwrap();
        assert_eq!(
            l.routine_views(&act, &Utc)[0].mode,
            Some(RoutineMode::DryRun)
        );
    }

    #[test]
    fn queued_runs_never_go_live_and_going_dry_cancels_queued_live_ones() {
        let mut l = Ledger::default();
        worker(&mut l, "mac");
        // The worker is busy, so the dry run waits.
        l.submit("other".into(), None, Some("claude".into()), None, vec![]);
        let act = defs("---\nevery: 1h\nact: true\n---\ngo");
        let r = act[0].as_ref().unwrap();
        l.routine_tick(&act, &Utc);
        assert_eq!(l.tasks[1].status, TaskStatus::Queued);
        // Going live approves the file as it is now, not what that run was queued with.
        assert!(l.set_live(r, true).unwrap().is_empty());
        let (p, ro, mode) = last_run(&l);
        assert!(p.contains("(scheduled run, dry run)") && ro && mode == Some(RoutineMode::DryRun));
        // A live run queued, then back to dry run: it's cancelled, the dry one isn't.
        l.cancel(l.tasks[1].task.id).unwrap();
        l.start_routine(r).unwrap();
        assert_eq!(last_run(&l).2, Some(RoutineMode::Live));
        let effects = l.set_live(r, false).unwrap();
        assert!(!effects.is_empty());
        assert_eq!(l.tasks.last().unwrap().status, TaskStatus::Cancelled);
        let report = defs("---\nevery: 1h\n---\ngo");
        assert!(l.set_live(report[0].as_ref().unwrap(), false).is_err());
    }

    #[test]
    fn an_edit_cancels_queued_live_runs() {
        let mut l = Ledger::default();
        worker(&mut l, "mac");
        l.submit("other".into(), None, Some("claude".into()), None, vec![]);
        let act = defs("---\nevery: 1h\nact: true\n---\ngo");
        l.set_live(act[0].as_ref().unwrap(), true).unwrap();
        l.routine_tick(&act, &Utc);
        assert_eq!(last_run(&l).2, Some(RoutineMode::Live));
        let edited = defs("---\nevery: 1h\nact: true\n---\ngo further");
        let (ended, effects) = l.forget_live(&edited);
        assert!(ended && !effects.is_empty());
        assert_eq!(l.tasks.last().unwrap().status, TaskStatus::Cancelled);
    }

    #[test]
    fn dropping_act_from_the_file_ends_live() {
        let mut l = Ledger::default();
        let act = defs("---\nevery: 1h\nact: true\n---\ngo");
        l.set_live(act[0].as_ref().unwrap(), true).unwrap();
        l.routine_tick(&act, &Utc);
        assert_eq!(last_run(&l).2, Some(RoutineMode::Live));
        // `act` gone: report only, and live is forgotten...
        let report = defs("---\nevery: 1h\n---\ngo");
        assert_eq!(
            l.routine_views(&report, &Utc)[0].mode,
            Some(RoutineMode::Report)
        );
        assert!(l.forget_live(&report).0);
        assert!(!l.forget_live(&report).0);
        // ...so `act` back means dry run again.
        assert_eq!(
            l.routine_views(&act, &Utc)[0].mode,
            Some(RoutineMode::DryRun)
        );
    }

    #[test]
    fn state_survives_a_restart() {
        let mut l = Ledger::default();
        let ds = defs("---\nevery: 1h\n---\ngo");
        l.routine_tick(&ds, &Utc);
        l.pause_routine("r", true);
        let act = defs("---\nevery: 1h\nact: true\n---\ngo");
        l.set_live(act[0].as_ref().unwrap(), true).unwrap();
        let snap = l.snapshot(&Default::default());
        let r = Ledger::restore(&snap.ledger, |_| None, Default::default(), 0).unwrap();
        assert_eq!(r.ledger.routines, l.routines);
        assert!(r.ledger.routine_runs("r").busy);
        let v = &r.ledger.routine_views(&act, &Utc)[0];
        assert_eq!(v.mode, Some(RoutineMode::Live));
        // The queued run keeps the mode it was queued with.
        assert_eq!(
            r.ledger.tasks[0].summary().routine_mode,
            Some(RoutineMode::Report)
        );
    }

    const MERGE: &str = "---\nevery: 1h\nact: true\nmerge:\n  repos: [o/r]\n  authors: [me]\n  \
                         max_files: 5\n  max_lines: 50\n---\nMerge green PRs.";

    #[test]
    fn an_edit_after_going_live_drops_back_to_dry_run_until_live_again() {
        let mut l = Ledger::default();
        let ds = defs(MERGE);
        let r = ds[0].clone().unwrap();
        l.set_live(&r, true).unwrap();
        assert!(!l.forget_live(&ds).0, "unchanged");
        l.routine_tick(&ds, &Utc);
        let t = &l.tasks[0].task;
        assert!(!t.read_only);
        assert_eq!(t.merge, MergeRule::Tool(Box::new(r.merge.clone().unwrap())));
        // The policy is loosened: its next run is dry, before and after the file is re-read.
        let edited = defs(&MERGE.replace("max_lines: 50", "max_lines: 5000"));
        assert_eq!(
            l.routine_views(&edited, &Utc)[0].mode,
            Some(RoutineMode::DryRun)
        );
        assert!(l.forget_live(&edited).0);
        let v = &l.routine_views(&edited, &Utc)[0];
        assert!(v.changed && v.mode == Some(RoutineMode::DryRun));
        // Changing it back doesn't revive live: it needs going live again.
        assert_eq!(
            l.routine_views(&ds, &Utc)[0].mode,
            Some(RoutineMode::DryRun)
        );
        let e = edited[0].as_ref().unwrap();
        l.set_live(e, true).unwrap();
        let v = &l.routine_views(&edited, &Utc)[0];
        assert!(!v.changed && v.mode == Some(RoutineMode::Live));
        // Dropping `act` ends live without calling it a change.
        let report = defs("---\nevery: 1h\n---\nMerge green PRs.");
        assert!(l.forget_live(&report).0);
        assert!(!l.routines["r"].changed);
    }

    #[test]
    fn routine_runs_merge_only_by_their_policy_and_replies_as_the_user() {
        let mut l = Ledger::default();
        let wid = worker(&mut l, "mac");
        let plain = defs("---\nevery: 1h\nact: true\n---\ngo");
        l.routine_tick(&plain, &Utc);
        assert_eq!(l.tasks[0].task.merge, MergeRule::Never);
        let ds = defs(MERGE);
        l.start_routine(ds[0].as_ref().unwrap()).unwrap();
        let t = &l.tasks[1].task;
        assert!(
            t.read_only && matches!(t.merge, MergeRule::Tool(_)),
            "a dry run's merge_pr only says"
        );
        // A reply in the run's thread is the user's: no routine limits.
        let id = l.tasks[0].task.id;
        (l.tasks[0].status, l.tasks[0].session, l.tasks[0].worker) =
            (TaskStatus::Done, Some("s".into()), Some(wid));
        l.reply(id, "now merge it".into(), vec![], false).unwrap();
        let reply = &l.tasks.last().unwrap().task;
        assert_eq!(
            (reply.merge.clone(), reply.read_only),
            (MergeRule::Any, false)
        );
    }
}
