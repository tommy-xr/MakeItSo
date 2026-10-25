//! Pure fleet state: workers, tasks, queue. Every transition returns the
//! effects the shell must perform. No I/O, no clocks, no locks here.

use std::collections::{HashMap, VecDeque};

use mis_types::*;
use uuid::Uuid;

use crate::plan;

/// Opaque handle for one worker connection; the shell maps it to a socket.
pub type ConnId = u64;

/// What the shell must do after a transition.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Send {
        conn: ConnId,
        msg: OrchMsg,
    },
    Broadcast(ServerMsg),
    /// Worker went offline; call `expire(wid, generation)` after the grace period.
    StartGrace {
        wid: WorkerId,
        generation: u64,
    },
    /// Run the planner on `prompt`; feed its events to `planning_event(task, ..)`.
    Plan {
        task: TaskId,
        prompt: String,
    },
}

struct Worker {
    info: WorkerInfo,
    /// None while offline (inside the reconnect grace period).
    conn: Option<ConnId>,
    /// Bumped on every (re)connect so a stale grace timer can't expire a live worker.
    generation: u64,
}

struct TaskEntry {
    task: Task,
    status: TaskStatus,
    worker: Option<WorkerId>,
}

#[derive(Default)]
pub struct Ledger {
    workers: HashMap<WorkerId, Worker>,
    tasks: Vec<TaskEntry>, // insertion order = submission order
    queue: VecDeque<TaskId>,
}

impl Ledger {
    pub fn status(&self) -> ServerMsg {
        ServerMsg::Status {
            workers: self
                .workers
                .iter()
                .map(|(id, w)| WorkerSummary {
                    id: *id,
                    info: w.info.clone(),
                    busy: self.is_busy(*id),
                    online: w.conn.is_some(),
                })
                .collect(),
            tasks: self
                .tasks
                .iter()
                .map(|t| TaskSummary {
                    id: t.task.id,
                    prompt: t.task.prompt.clone(),
                    status: t.status,
                    worker: t.worker,
                })
                .collect(),
        }
    }

    fn is_busy(&self, wid: WorkerId) -> bool {
        self.tasks
            .iter()
            .any(|t| t.worker == Some(wid) && t.status == TaskStatus::Running)
    }

    /// With a harness the task is queued; without one it goes to the planner,
    /// which picks the harness and rewrites the prompt (see `planning_event`).
    pub fn submit(
        &mut self,
        prompt: String,
        cwd: Option<String>,
        harness: Option<String>,
    ) -> (TaskId, Vec<Effect>) {
        let id = Uuid::new_v4();
        let planning = harness.is_none();
        self.tasks.push(TaskEntry {
            task: Task {
                id,
                prompt: prompt.clone(),
                cwd,
                harness: harness.unwrap_or_default(),
            },
            status: if planning {
                TaskStatus::Planning
            } else {
                TaskStatus::Queued
            },
            worker: None,
        });
        if planning {
            return (
                id,
                vec![Effect::Plan {
                    task: id,
                    prompt: plan::prompt(&prompt, &self.harnesses()),
                }],
            );
        }
        self.queue.push_back(id);
        (id, self.schedule())
    }

    /// Every harness any known worker offers.
    pub fn harnesses(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .workers
            .values()
            .flat_map(|w| w.info.harnesses.iter().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Planner output for a Planning task. Text streams to followers; Finished
    /// carries the plan, which queues the task or fails it.
    pub fn planning_event(&mut self, task: TaskId, event: AgentEvent) -> Vec<Effect> {
        let harnesses = self.harnesses();
        let Some(t) = self.tasks.iter_mut().find(|t| t.task.id == task) else {
            return vec![];
        };
        if t.status != TaskStatus::Planning {
            return vec![];
        }
        let broadcast = |event| {
            Effect::Broadcast(ServerMsg::Event {
                task,
                worker: None,
                event,
            })
        };
        match event {
            // The JSON answer itself is not relayed; its `reason` is, after parsing.
            AgentEvent::Text { ref text } if text.trim_start().starts_with('{') => vec![],
            AgentEvent::Text { .. } => vec![broadcast(event)],
            AgentEvent::Finished { ok, summary } => {
                let plan = match (ok, summary) {
                    (true, Some(reply)) => plan::parse(&reply, &harnesses),
                    (_, summary) => Err(summary.unwrap_or_else(|| "planner failed".into())),
                };
                match plan {
                    Ok(p) => {
                        t.task.harness = p.harness.clone();
                        t.task.prompt = p.prompt.clone();
                        t.status = TaskStatus::Queued;
                        self.queue.push_back(task);
                        let mut effects = vec![];
                        if !p.reason.is_empty() {
                            effects.push(broadcast(AgentEvent::Text { text: p.reason }));
                        }
                        effects.push(broadcast(AgentEvent::Planned {
                            harness: p.harness,
                            prompt: p.prompt,
                        }));
                        effects.extend(self.schedule());
                        effects
                    }
                    Err(why) => {
                        t.status = TaskStatus::Failed;
                        vec![broadcast(AgentEvent::Finished {
                            ok: false,
                            summary: Some(why),
                        })]
                    }
                }
            }
            _ => vec![],
        }
    }

    /// Worker (re)connects. Running tasks it still reports are kept; the rest are
    /// failed. Returns any reported task ids this ledger never saw, and the effects.
    pub fn connect(
        &mut self,
        wid: WorkerId,
        info: WorkerInfo,
        running: &[TaskId],
        conn: ConnId,
    ) -> (Vec<TaskId>, Vec<Effect>) {
        let generation = self.workers.get(&wid).map_or(0, |w| w.generation) + 1;
        self.workers.insert(
            wid,
            Worker {
                info,
                conn: Some(conn),
                generation,
            },
        );
        let unknown = running
            .iter()
            .copied()
            .filter(|id| !self.tasks.iter().any(|t| t.task.id == *id))
            .collect();
        let mut effects = self.fail_running(wid, running, "worker restarted");
        effects.extend(self.schedule());
        (unknown, effects)
    }

    /// Connection `conn` closed. Ignored if the worker already reconnected on another.
    pub fn disconnect(&mut self, wid: WorkerId, conn: ConnId) -> Vec<Effect> {
        let Some(w) = self.workers.get_mut(&wid) else {
            return vec![];
        };
        if w.conn != Some(conn) {
            return vec![];
        }
        w.conn = None;
        vec![Effect::StartGrace {
            wid,
            generation: w.generation,
        }]
    }

    /// Grace period ended. Drops the worker and fails its tasks unless it came back.
    pub fn expire(&mut self, wid: WorkerId, generation: u64) -> Vec<Effect> {
        let gone = self
            .workers
            .get(&wid)
            .is_some_and(|w| w.conn.is_none() && w.generation == generation);
        if !gone {
            return vec![];
        }
        self.workers.remove(&wid);
        self.fail_running(wid, &[], "worker disconnected")
    }

    /// Event from `conn` about `task`. Dropped unless `conn` is the worker's live
    /// connection and the task is running on that worker.
    pub fn event(
        &mut self,
        wid: WorkerId,
        conn: ConnId,
        task: TaskId,
        event: AgentEvent,
    ) -> Vec<Effect> {
        if !self.workers.get(&wid).is_some_and(|w| w.conn == Some(conn)) {
            return vec![];
        }
        let Some(t) = self.tasks.iter_mut().find(|t| t.task.id == task) else {
            return vec![];
        };
        if t.worker != Some(wid) || t.status != TaskStatus::Running {
            return vec![];
        }
        if let AgentEvent::Finished { ok, .. } = &event {
            t.status = if *ok {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
        }
        let mut effects = vec![Effect::Broadcast(ServerMsg::Event {
            task,
            worker: Some(wid),
            event,
        })];
        effects.extend(self.schedule());
        effects
    }

    /// Hand queued tasks to online, idle workers that support the task's harness.
    fn schedule(&mut self) -> Vec<Effect> {
        let mut effects = vec![];
        let mut remaining = VecDeque::new();
        while let Some(id) = self.queue.pop_front() {
            let Some(ti) = self.tasks.iter().position(|t| t.task.id == id) else {
                continue;
            };
            let harness = &self.tasks[ti].task.harness;
            let picked = self
                .workers
                .iter()
                .filter(|(wid, w)| w.conn.is_some() && !self.is_busy(**wid))
                .find(|(_, w)| w.info.harnesses.contains(harness))
                .map(|(wid, w)| (*wid, w.conn.unwrap()));
            match picked {
                Some((wid, conn)) => {
                    let t = &mut self.tasks[ti];
                    t.status = TaskStatus::Running;
                    t.worker = Some(wid);
                    effects.push(Effect::Send {
                        conn,
                        msg: OrchMsg::Assign {
                            task: t.task.clone(),
                        },
                    });
                }
                None => remaining.push_back(id),
            }
        }
        self.queue = remaining;
        effects
    }

    /// Fail every Running task on `wid` not in `keep`, telling followers why.
    fn fail_running(&mut self, wid: WorkerId, keep: &[TaskId], why: &str) -> Vec<Effect> {
        self.tasks
            .iter_mut()
            .filter(|t| {
                t.worker == Some(wid)
                    && t.status == TaskStatus::Running
                    && !keep.contains(&t.task.id)
            })
            .map(|t| {
                t.status = TaskStatus::Failed;
                Effect::Broadcast(ServerMsg::Event {
                    task: t.task.id,
                    worker: Some(wid),
                    event: AgentEvent::Finished {
                        ok: false,
                        summary: Some(why.into()),
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> WorkerInfo {
        WorkerInfo {
            hostname: "h".into(),
            os: "o".into(),
            arch: "a".into(),
            harnesses: vec!["claude".into()],
        }
    }

    fn finished(effects: &[Effect], task: TaskId) -> Option<bool> {
        effects.iter().find_map(|e| match e {
            Effect::Broadcast(ServerMsg::Event {
                task: t,
                event: AgentEvent::Finished { ok, .. },
                ..
            }) if *t == task => Some(*ok),
            _ => None,
        })
    }

    fn assigned(effects: &[Effect]) -> Vec<(ConnId, TaskId)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send {
                    conn,
                    msg: OrchMsg::Assign { task },
                } => Some((*conn, task.id)),
                _ => None,
            })
            .collect()
    }

    fn task_status(l: &Ledger, id: TaskId) -> TaskStatus {
        l.tasks.iter().find(|t| t.task.id == id).unwrap().status
    }

    /// Worker connected on conn 1 with one task running.
    fn running_setup() -> (Ledger, WorkerId, TaskId) {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1);
        let (task, eff) = l.submit("p".into(), None, Some("claude".into()));
        assert_eq!(assigned(&eff), vec![(1, task)]);
        (l, wid, task)
    }

    #[test]
    fn disconnect_then_reconnect_keeps_task_and_frees_worker_on_finish() {
        let (mut l, wid, task) = running_setup();
        assert_eq!(
            l.disconnect(wid, 1),
            vec![Effect::StartGrace { wid, generation: 1 }]
        );
        assert_eq!(task_status(&l, task), TaskStatus::Running);

        let (unknown, eff) = l.connect(wid, info(), &[task], 2);
        assert!(unknown.is_empty());
        assert_eq!(finished(&eff, task), None);
        assert!(l.is_busy(wid));

        // The expired grace timer from conn 1 must be a no-op now.
        assert!(l.expire(wid, 1).is_empty());
        assert!(l.workers.contains_key(&wid));

        let eff = l.event(
            wid,
            2,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        );
        assert_eq!(finished(&eff, task), Some(true));
        assert!(!l.is_busy(wid));
        let (next, eff) = l.submit("q".into(), None, Some("claude".into()));
        assert_eq!(assigned(&eff), vec![(2, next)]);
    }

    #[test]
    fn grace_expiry_fails_task_and_removes_worker() {
        let (mut l, wid, task) = running_setup();
        l.disconnect(wid, 1);
        let eff = l.expire(wid, 1);
        assert_eq!(finished(&eff, task), Some(false));
        assert_eq!(task_status(&l, task), TaskStatus::Failed);
        assert!(!l.workers.contains_key(&wid));
    }

    #[test]
    fn reconnect_without_task_fails_it() {
        let (mut l, wid, task) = running_setup();
        let (_, eff) = l.connect(wid, info(), &[], 2);
        assert_eq!(finished(&eff, task), Some(false));
        assert!(!l.is_busy(wid));
    }

    #[test]
    fn reported_unknown_terminal_or_foreign_tasks_do_not_pin_busy() {
        let (mut l, wid, task) = running_setup();
        l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        );
        l.disconnect(wid, 1); // offline, so the next task must land on `other`
        let other = Uuid::new_v4();
        l.connect(other, info(), &[], 5);
        let (foreign, eff) = l.submit("f".into(), None, Some("claude".into()));
        assert_eq!(assigned(&eff), vec![(5, foreign)]);

        let (unknown, _) = l.connect(wid, info(), &[task, foreign, Uuid::nil()], 2);
        assert_eq!(unknown, vec![Uuid::nil()]);
        assert!(!l.is_busy(wid));
        assert!(l.is_busy(other));
    }

    #[test]
    fn stale_connection_cannot_report_or_disconnect() {
        let (mut l, wid, task) = running_setup();
        l.connect(wid, info(), &[task], 2);
        assert!(l
            .event(
                wid,
                1,
                task,
                AgentEvent::Finished {
                    ok: true,
                    summary: None
                }
            )
            .is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Running);
        assert!(l.disconnect(wid, 1).is_empty());
        assert!(l.workers[&wid].conn == Some(2));
    }

    #[test]
    fn offline_worker_is_not_scheduled() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1);
        l.disconnect(wid, 1);
        let (task, eff) = l.submit("p".into(), None, Some("claude".into()));
        assert!(assigned(&eff).is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Queued);
        let (_, eff) = l.connect(wid, info(), &[], 2);
        assert_eq!(assigned(&eff), vec![(2, task)]);
    }

    #[test]
    fn planner_picks_harness_and_rewrites_prompt() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        let mut both = info();
        both.harnesses.push("codex".into());
        l.connect(wid, both, &[], 1);
        let (task, eff) = l.submit("hey run as codex and say hi".into(), None, None);
        assert_eq!(task_status(&l, task), TaskStatus::Planning);
        let [Effect::Plan { task: t, prompt }] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(*t, task);
        assert!(prompt.contains("claude, codex"));
        assert!(l
            .planning_event(task, AgentEvent::Started { session: None })
            .is_empty());

        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"codex","prompt":"say hi","reason":"asked"}"#.into()),
            },
        );
        assert_eq!(task_status(&l, task), TaskStatus::Running);
        let [Effect::Broadcast(ServerMsg::Event {
            worker: None,
            event: AgentEvent::Text { text },
            ..
        }), Effect::Broadcast(ServerMsg::Event {
            event: AgentEvent::Planned { harness, prompt },
            ..
        }), Effect::Send {
            conn: 1,
            msg: OrchMsg::Assign { task: sent },
        }] = &eff[..]
        else {
            panic!("{eff:?}")
        };
        assert_eq!(
            (text.as_str(), harness.as_str(), prompt.as_str()),
            ("asked", "codex", "say hi")
        );
        assert_eq!(
            (sent.harness.as_str(), sent.prompt.as_str()),
            ("codex", "say hi")
        );
        // Late planner output is ignored once queued.
        assert!(l
            .planning_event(task, AgentEvent::Text { text: "x".into() })
            .is_empty());
    }

    #[test]
    fn bad_plan_fails_task() {
        let mut l = Ledger::default();
        l.connect(Uuid::new_v4(), info(), &[], 1);
        let (task, _) = l.submit("p".into(), None, None);
        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"codex","prompt":"x"}"#.into()),
            },
        );
        assert_eq!(finished(&eff, task), Some(false));
        assert_eq!(task_status(&l, task), TaskStatus::Failed);
    }
}
