//! Pure fleet state: workers, tasks, queue. Every transition returns the
//! effects the shell must perform. No I/O, no clocks, no locks here; `now` is
//! monotonic time from the shell, so wall-clock jumps can't fake silence.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

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
    /// Drop this connection's socket so a silent worker must reconnect with Hello.
    Close {
        conn: ConnId,
    },
}

struct Worker {
    info: WorkerInfo,
    /// None while offline (inside the reconnect grace period).
    conn: Option<ConnId>,
    /// Bumped on every (re)connect so a stale grace timer can't expire a live worker.
    generation: u64,
    /// Latest stats and the Unix second they arrived; cleared on (re)connect.
    stats: Option<(WorkerStats, u64)>,
    /// Last Hello or Stats; `tick` drops the worker when this gets too old.
    last_seen: Duration,
}

/// Per-task transcript cap: sum of each event's serialized JSON size. Oldest
/// events go first; the newest is always kept.
pub const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;
/// A single stored `ToolResult` keeps at most this many bytes of content.
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;
const TRUNCATED: &str = "…[truncated]";

struct TaskEntry {
    task: Task,
    status: TaskStatus,
    worker: Option<WorkerId>,
    session: Option<String>,
    /// Model the agent reported in `Started`; shown when none was requested.
    reported_model: Option<String>,
    summary: Option<String>,
    events: Vec<AgentEvent>,
    /// Serialized size of `events`.
    bytes: usize,
    truncated: bool,
}

impl TaskEntry {
    fn new(task: Task) -> Self {
        Self {
            task,
            status: TaskStatus::Queued,
            worker: None,
            session: None,
            reported_model: None,
            summary: None,
            events: vec![],
            bytes: 0,
            truncated: false,
        }
    }

    fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.task.id,
            prompt: self.task.prompt.clone(),
            status: self.status,
            worker: self.worker,
            harness: self.task.harness.clone(),
            model: self.task.model.clone().or(self.reported_model.clone()),
            session: self.session.clone(),
            summary: self.summary.clone(),
        }
    }

    /// Append to the transcript, evicting oldest events past `MAX_TRANSCRIPT_BYTES`.
    fn record(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Started { session, model } => {
                if session.is_some() {
                    self.session = session.clone();
                }
                self.reported_model = model.clone();
            }
            AgentEvent::Finished { summary, .. } => self.summary = summary.clone(),
            _ => {}
        }
        let event = match event {
            AgentEvent::ToolResult { content } if content.len() > MAX_TOOL_RESULT_BYTES => {
                let mut end = MAX_TOOL_RESULT_BYTES - TRUNCATED.len();
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                AgentEvent::ToolResult {
                    content: format!("{}{TRUNCATED}", &content[..end]),
                }
            }
            e => e.clone(),
        };
        self.bytes += size(&event);
        self.events.push(event);
        let mut evict = 0;
        while self.bytes > MAX_TRANSCRIPT_BYTES && evict < self.events.len() - 1 {
            self.bytes -= size(&self.events[evict]);
            evict += 1;
        }
        if evict > 0 {
            self.events.drain(..evict);
            self.truncated = true;
        }
    }
}

fn size(e: &AgentEvent) -> usize {
    serde_json::to_vec(e).map_or(0, |v| v.len())
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
                    stats: w.stats.as_ref().map(|(s, _)| s.clone()),
                    stats_at: w.stats.as_ref().map(|(_, at)| *at),
                })
                .collect(),
            tasks: self.tasks.iter().map(TaskEntry::summary).collect(),
        }
    }

    /// Task metadata, retained transcript (oldest first), and whether any was evicted.
    pub fn show(&self, task: TaskId) -> Option<(TaskSummary, &[AgentEvent], bool)> {
        let t = self.tasks.iter().find(|t| t.task.id == task)?;
        Some((t.summary(), &t.events, t.truncated))
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
        model: Option<String>,
    ) -> (TaskId, Vec<Effect>) {
        let id = Uuid::new_v4();
        let planning = harness.is_none();
        let plan_prompt = planning.then(|| plan::prompt(&prompt, &self.fleet()));
        let mut entry = TaskEntry::new(Task {
            id,
            prompt,
            cwd,
            // Empty until planned; Planning tasks are never queued or assigned.
            harness: harness.unwrap_or_default(),
            model,
        });
        if planning {
            entry.status = TaskStatus::Planning;
        }
        self.tasks.push(entry);
        if let Some(prompt) = plan_prompt {
            return (id, vec![Effect::Plan { task: id, prompt }]);
        }
        self.queue.push_back(id);
        (id, self.schedule())
    }

    /// Every harness a known worker offers, with the models every such worker
    /// advertises (scheduling ignores the model, so any of them may get the task);
    /// with no workers, every harness that exists and no models, so a plan made
    /// before the fleet connects still queues (on the harness default).
    pub fn fleet(&self) -> plan::Fleet {
        let mut fleet = plan::Fleet::new();
        // By id, so the surviving order (first = preferred) doesn't follow hash order.
        let mut workers: Vec<_> = self.workers.iter().collect();
        workers.sort_by_key(|(id, _)| **id);
        for (_, w) in workers {
            for h in &w.info.harnesses {
                let mine = w.info.models.get(h).cloned().unwrap_or_default();
                fleet
                    .entry(h.clone())
                    .and_modify(|m| m.retain(|x| mine.contains(x)))
                    .or_insert(mine);
            }
        }
        if fleet.is_empty() {
            fleet = mis_agent::HARNESSES
                .iter()
                .map(|h| (h.to_string(), vec![]))
                .collect();
        }
        fleet
    }

    /// Planner output for a Planning task. Started gives the planner's model, kept
    /// in case it answers; Finished carries the plan (the reply's JSON), which
    /// answers, queues, or fails the task.
    pub fn planning_event(&mut self, task: TaskId, event: AgentEvent) -> Vec<Effect> {
        let fleet = self.fleet();
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
            AgentEvent::Finished { ok, summary } => {
                let plan = match (ok, summary) {
                    (true, Some(reply)) => plan::parse(&reply, &t.task.prompt, &fleet),
                    (_, summary) => Err(summary.unwrap_or_else(|| "planner failed".into())),
                };
                match plan {
                    // Answered by the planner: Done, credited to it, no worker.
                    Ok(plan::Plan {
                        answer: Some(answer),
                        ..
                    }) => {
                        t.task.harness = "planner".into();
                        t.status = TaskStatus::Done;
                        let events = [
                            AgentEvent::Text {
                                text: answer.clone(),
                            },
                            AgentEvent::Finished {
                                ok: true,
                                summary: Some(answer),
                            },
                        ];
                        events.iter().for_each(|e| t.record(e));
                        events.into_iter().map(broadcast).collect()
                    }
                    Ok(p) => {
                        // The worker reports its own model.
                        t.reported_model = None;
                        t.task.harness = p.harness.clone();
                        t.task.prompt = p.prompt.clone();
                        t.task.model = p.model.clone();
                        t.status = TaskStatus::Queued;
                        self.queue.push_back(task);
                        let mut events = vec![];
                        if !p.reason.is_empty() {
                            events.push(AgentEvent::Text { text: p.reason });
                        }
                        events.push(AgentEvent::Planned {
                            harness: p.harness,
                            prompt: p.prompt,
                            model: p.model,
                        });
                        events.iter().for_each(|e| t.record(e));
                        let mut effects: Vec<_> = events.into_iter().map(broadcast).collect();
                        effects.extend(self.schedule());
                        effects
                    }
                    Err(why) => {
                        t.status = TaskStatus::Failed;
                        let event = AgentEvent::Finished {
                            ok: false,
                            summary: Some(why),
                        };
                        t.record(&event);
                        vec![broadcast(event)]
                    }
                }
            }
            AgentEvent::Started { model, .. } => {
                t.reported_model = model;
                vec![]
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
        now: Duration,
    ) -> (Vec<TaskId>, Vec<Effect>) {
        let generation = self.workers.get(&wid).map_or(0, |w| w.generation) + 1;
        self.workers.insert(
            wid,
            Worker {
                info,
                conn: Some(conn),
                generation,
                stats: None,
                last_seen: now,
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

    /// Heartbeat from `conn`: store the stats, stamped `unix_secs` for clients.
    /// Ignored unless `conn` is live.
    pub fn worker_stats(
        &mut self,
        wid: WorkerId,
        conn: ConnId,
        stats: WorkerStats,
        now: Duration,
        unix_secs: u64,
    ) {
        if let Some(w) = self.workers.get_mut(&wid).filter(|w| w.conn == Some(conn)) {
            w.stats = Some((stats, unix_secs));
            w.last_seen = now;
        }
    }

    /// Disconnect every online worker not heard from within `stale_after`, and
    /// close its socket. From there the normal grace/expire path applies.
    pub fn tick(&mut self, now: Duration, stale_after: Duration) -> Vec<Effect> {
        let stale: Vec<(WorkerId, ConnId)> = self
            .workers
            .iter()
            .filter(|(_, w)| now.saturating_sub(w.last_seen) > stale_after)
            .filter_map(|(wid, w)| Some((*wid, w.conn?)))
            .collect();
        stale
            .into_iter()
            .flat_map(|(wid, conn)| {
                let mut effects = self.disconnect(wid, conn);
                effects.push(Effect::Close { conn });
                effects
            })
            .collect()
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
        t.record(&event);
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
                let event = AgentEvent::Finished {
                    ok: false,
                    summary: Some(why.into()),
                };
                t.record(&event);
                Effect::Broadcast(ServerMsg::Event {
                    task: t.task.id,
                    worker: Some(wid),
                    event,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;
    const STALE: Duration = Duration::from_secs(35);

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn stats(cpu: u8) -> WorkerStats {
        WorkerStats {
            cpu_pct: cpu,
            ..Default::default()
        }
    }

    fn summary(l: &Ledger, wid: WorkerId) -> WorkerSummary {
        let ServerMsg::Status { workers, .. } = l.status() else {
            panic!()
        };
        workers.into_iter().find(|w| w.id == wid).unwrap()
    }

    #[test]
    fn stats_are_stored_and_reported() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1, T0);
        assert_eq!(summary(&l, wid).stats, None);
        assert_eq!(summary(&l, wid).stats_at, None);
        l.worker_stats(wid, 1, stats(64), secs(10), 1_700_000_000);
        let s = summary(&l, wid);
        assert_eq!(s.stats, Some(stats(64)));
        assert_eq!(s.stats_at, Some(1_700_000_000));
        // A stale connection's stats are ignored.
        l.worker_stats(wid, 9, stats(1), secs(11), 0);
        assert_eq!(summary(&l, wid).stats, Some(stats(64)));
    }

    #[test]
    fn tick_drops_silent_worker_through_disconnect_path() {
        let (mut l, wid, task) = running_setup();
        l.worker_stats(wid, 1, stats(5), secs(10), 0);
        assert!(l.tick(secs(45), STALE).is_empty()); // exactly at the limit: kept
        assert_eq!(
            l.tick(secs(46), STALE),
            vec![
                Effect::StartGrace { wid, generation: 1 },
                Effect::Close { conn: 1 }
            ]
        );
        assert!(!summary(&l, wid).online);
        // Already offline: not dropped twice. Task waits out the grace period.
        assert!(l.tick(secs(100), STALE).is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Running);
        let eff = l.expire(wid, 1);
        assert_eq!(finished(&eff, task), Some(false));
    }

    #[test]
    fn tick_keeps_worker_that_keeps_beating() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1, T0);
        for t in (15..200).step_by(15) {
            l.worker_stats(wid, 1, stats(1), secs(t), 0);
            assert!(l.tick(secs(t + 5), STALE).is_empty());
        }
        assert!(summary(&l, wid).online);
    }

    #[test]
    fn hello_counts_as_a_beat_and_reconnect_resets() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1, T0);
        l.worker_stats(wid, 1, stats(9), secs(1), 0);
        assert_eq!(l.tick(secs(40), STALE).len(), 2);
        l.connect(wid, info(), &[], 2, secs(50));
        let s = summary(&l, wid);
        assert!(s.online);
        assert_eq!((s.stats, s.stats_at), (None, None));
        assert!(l.tick(secs(80), STALE).is_empty());
        assert_eq!(l.tick(secs(90), STALE).len(), 2);
    }

    fn info() -> WorkerInfo {
        WorkerInfo {
            hostname: "h".into(),
            os: "o".into(),
            arch: "a".into(),
            harnesses: vec!["claude".into()],
            roots: vec![],
            models: [("claude".into(), vec!["haiku".into(), "sonnet".into()])].into(),
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
        l.connect(wid, info(), &[], 1, T0);
        let (task, eff) = l.submit("p".into(), None, Some("claude".into()), None);
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

        let (unknown, eff) = l.connect(wid, info(), &[task], 2, T0);
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
        let (next, eff) = l.submit("q".into(), None, Some("claude".into()), None);
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
        let (_, eff) = l.connect(wid, info(), &[], 2, T0);
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
        l.connect(other, info(), &[], 5, T0);
        let (foreign, eff) = l.submit("f".into(), None, Some("claude".into()), None);
        assert_eq!(assigned(&eff), vec![(5, foreign)]);

        let (unknown, _) = l.connect(wid, info(), &[task, foreign, Uuid::nil()], 2, T0);
        assert_eq!(unknown, vec![Uuid::nil()]);
        assert!(!l.is_busy(wid));
        assert!(l.is_busy(other));
    }

    #[test]
    fn stale_connection_cannot_report_or_disconnect() {
        let (mut l, wid, task) = running_setup();
        l.connect(wid, info(), &[task], 2, T0);
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
        l.connect(wid, info(), &[], 1, T0);
        l.disconnect(wid, 1);
        let (task, eff) = l.submit("p".into(), None, Some("claude".into()), None);
        assert!(assigned(&eff).is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Queued);
        let (_, eff) = l.connect(wid, info(), &[], 2, T0);
        assert_eq!(assigned(&eff), vec![(2, task)]);
    }

    fn text(t: &str) -> AgentEvent {
        AgentEvent::Text { text: t.into() }
    }

    #[test]
    fn transcript_accumulates_in_order_with_session_and_summary() {
        let (mut l, wid, task) = running_setup();
        let events = vec![
            AgentEvent::Started {
                session: Some("s1".into()),
                model: None,
            },
            text("hi"),
            AgentEvent::ToolResult {
                content: "out".into(),
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("all good".into()),
            },
        ];
        for e in &events {
            l.event(wid, 1, task, e.clone());
        }
        // Dropped: task is no longer running.
        l.event(wid, 1, task, text("late"));
        let (info, got, truncated) = l.show(task).unwrap();
        assert_eq!(got, events.as_slice());
        assert!(!truncated);
        assert_eq!(info.session.as_deref(), Some("s1"));
        assert_eq!(info.summary.as_deref(), Some("all good"));
        assert_eq!(info.harness, "claude");
        assert_eq!(info.worker, Some(wid));
        assert!(l.show(Uuid::nil()).is_none());
    }

    #[test]
    fn model_from_submit_else_from_started() {
        let started = |m: &str| AgentEvent::Started {
            session: None,
            model: Some(m.into()),
        };
        let model = |l: &Ledger, t| l.show(t).unwrap().0.model;
        // Not requested: the reported model fills in.
        let (mut l, wid, task) = running_setup();
        assert_eq!(model(&l, task), None);
        l.event(wid, 1, task, started("claude-sonnet-4-5"));
        assert_eq!(model(&l, task).as_deref(), Some("claude-sonnet-4-5"));
        l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        );
        // Requested: sent to the worker and kept over the reported one.
        let (next, eff) = l.submit("q".into(), None, Some("claude".into()), Some("opus".into()));
        let [Effect::Send {
            msg: OrchMsg::Assign { task: sent },
            ..
        }] = &eff[..]
        else {
            panic!("{eff:?}")
        };
        assert_eq!(sent.model.as_deref(), Some("opus"));
        l.event(wid, 1, next, started("claude-opus-4-1"));
        assert_eq!(model(&l, next).as_deref(), Some("opus"));
    }

    #[test]
    fn transcript_cap_evicts_oldest_and_flags_truncated() {
        let (mut l, wid, task) = running_setup();
        let chunk = "x".repeat(10 * 1024);
        for i in 0..40 {
            l.event(wid, 1, task, text(&format!("{i} {chunk}")));
        }
        let (_, got, truncated) = l.show(task).unwrap();
        assert!(truncated);
        let bytes: usize = got.iter().map(size).sum();
        assert!(bytes <= MAX_TRANSCRIPT_BYTES);
        assert!(got.len() < 40);
        // Newest kept, oldest gone, order preserved.
        assert_eq!(got.last(), Some(&text(&format!("39 {chunk}"))));
        let AgentEvent::Text { text: first } = &got[0] else {
            panic!()
        };
        assert!(first.starts_with(&format!("{} ", 40 - got.len())));
    }

    #[test]
    fn huge_tool_result_is_truncated_on_store() {
        let (mut l, wid, task) = running_setup();
        // Odd prefix puts the cut mid-character.
        let content = format!("a{}", "é".repeat(MAX_TOOL_RESULT_BYTES));
        l.event(wid, 1, task, AgentEvent::ToolResult { content });
        let (_, got, truncated) = l.show(task).unwrap();
        let AgentEvent::ToolResult { content } = &got[0] else {
            panic!()
        };
        assert!(content.ends_with(TRUNCATED));
        assert!(content.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(content.len() > MAX_TOOL_RESULT_BYTES - TRUNCATED.len() - 2);
        assert!(!truncated);
    }

    #[test]
    fn worker_loss_is_recorded_in_transcript() {
        let (mut l, wid, task) = running_setup();
        l.connect(wid, info(), &[], 2, T0);
        let (info, got, _) = l.show(task).unwrap();
        assert_eq!(info.summary.as_deref(), Some("worker restarted"));
        assert!(matches!(got, [AgentEvent::Finished { ok: false, .. }]));
    }

    #[test]
    fn oversized_event_is_kept_alone() {
        let (mut l, wid, task) = running_setup();
        l.event(wid, 1, task, text("small"));
        let huge = text(&"x".repeat(MAX_TRANSCRIPT_BYTES));
        l.event(wid, 1, task, huge.clone());
        let (_, got, truncated) = l.show(task).unwrap();
        assert_eq!(got, std::slice::from_ref(&huge));
        assert!(truncated);
    }

    #[test]
    fn planner_picks_harness_and_rewrites_prompt() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        let mut both = info();
        both.harnesses.push("codex".into());
        l.connect(wid, both, &[], 1, T0);
        let (task, eff) = l.submit("hey run as codex and say hi".into(), None, None, None);
        assert_eq!(task_status(&l, task), TaskStatus::Planning);
        let [Effect::Plan { task: t, prompt }] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(*t, task);
        assert!(prompt.contains("claude, codex"));
        assert!(l
            .planning_event(
                task,
                AgentEvent::Started {
                    session: None,
                    model: None
                }
            )
            .is_empty());
        assert!(l
            .planning_event(task, AgentEvent::Text { text: "{}".into() })
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
            event:
                AgentEvent::Planned {
                    harness,
                    prompt,
                    model: None,
                },
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
        // Planner reason and decision are part of the transcript.
        let (_, transcript, _) = l.show(task).unwrap();
        assert!(matches!(
            transcript,
            [AgentEvent::Text { .. }, AgentEvent::Planned { .. }]
        ));
        // Late planner output is ignored once queued.
        assert!(l
            .planning_event(task, AgentEvent::Text { text: "x".into() })
            .is_empty());
    }

    #[test]
    fn planned_model_is_sent_and_fleet_intersects_models() {
        let mut l = Ledger::default();
        // No worker known: harnesses, no models.
        assert!(l.fleet()["claude"].is_empty());
        // Only models every claude worker offers.
        let mut big = info();
        big.models
            .insert("claude".into(), vec!["sonnet".into(), "opus".into()]);
        big.harnesses.push("codex".into());
        l.connect(Uuid::new_v4(), info(), &[], 1, T0);
        l.connect(Uuid::new_v4(), big, &[], 2, T0);
        let fleet = l.fleet();
        assert_eq!(fleet["claude"], ["sonnet"]);
        assert!(fleet["codex"].is_empty());

        let (task, _) = l.submit("summarize the README".into(), None, None, None);
        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"claude","model":"sonnet","reason":"default"}"#.into()),
            },
        );
        let sent: Vec<_> = eff
            .iter()
            .filter_map(|e| match e {
                Effect::Send {
                    msg: OrchMsg::Assign { task },
                    ..
                } => Some(task.model.as_deref()),
                Effect::Broadcast(ServerMsg::Event {
                    event: AgentEvent::Planned { model, .. },
                    ..
                }) => Some(model.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [Some("sonnet"), Some("sonnet")]);
        assert_eq!(l.show(task).unwrap().0.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn planner_answer_finishes_task_without_worker() {
        let mut l = Ledger::default();
        l.connect(Uuid::new_v4(), info(), &[], 1, T0);
        let (task, _) = l.submit("what is 2+2?".into(), None, None, None);
        l.planning_event(
            task,
            AgentEvent::Started {
                session: None,
                model: Some("claude-sonnet-4-5".into()),
            },
        );
        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"answer":"4","reason":"arithmetic"}"#.into()),
            },
        );
        assert!(assigned(&eff).is_empty());
        assert_eq!(finished(&eff, task), Some(true));
        let (t, transcript, _) = l.show(task).unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        assert_eq!(
            (t.harness.as_str(), t.model.as_deref(), t.worker),
            ("planner", Some("claude-sonnet-4-5"), None)
        );
        assert_eq!(t.summary.as_deref(), Some("4"));
        assert_eq!(
            transcript,
            [
                text("4"),
                AgentEvent::Finished {
                    ok: true,
                    summary: Some("4".into())
                }
            ]
        );
        // An explicit harness skips the planner, so an answer can't land.
        let (q, _) = l.submit("what is 2+2?".into(), None, Some("claude".into()), None);
        let late = AgentEvent::Finished {
            ok: true,
            summary: Some(r#"{"answer":"4"}"#.into()),
        };
        assert!(l.planning_event(q, late).is_empty());
        assert_ne!(l.show(q).unwrap().0.harness, "planner");
    }

    #[test]
    fn dispatched_task_drops_planner_model() {
        let mut l = Ledger::default();
        l.connect(Uuid::new_v4(), info(), &[], 1, T0);
        let (task, _) = l.submit("fix the bug".into(), None, None, None);
        l.planning_event(
            task,
            AgentEvent::Started {
                session: None,
                model: Some("claude-sonnet-4-5".into()),
            },
        );
        l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"claude"}"#.into()),
            },
        );
        assert_eq!(l.show(task).unwrap().0.model, None);
    }

    #[test]
    fn bad_plan_fails_task() {
        let mut l = Ledger::default();
        l.connect(Uuid::new_v4(), info(), &[], 1, T0);
        let (task, _) = l.submit("p".into(), None, None, None);
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

    #[test]
    fn plan_without_workers_queues_until_one_joins() {
        let mut l = Ledger::default();
        let (task, eff) = l.submit("use codex".into(), None, None, None);
        let [Effect::Plan { prompt, .. }] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert!(prompt.contains("claude, codex"));
        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"codex","prompt":"x"}"#.into()),
            },
        );
        assert!(assigned(&eff).is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Queued);
        let mut codex = info();
        codex.harnesses = vec!["codex".into()];
        let (_, eff) = l.connect(Uuid::new_v4(), codex, &[], 3, T0);
        assert_eq!(assigned(&eff), vec![(3, task)]);
    }
}
