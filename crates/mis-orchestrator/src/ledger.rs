//! Pure fleet state: workers, tasks, queue. Every transition returns the
//! effects the shell must perform. No I/O, no clocks, no locks here.

use std::collections::{HashMap, VecDeque};

use mis_types::*;
use uuid::Uuid;

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
}

struct Worker {
    info: WorkerInfo,
    /// None while offline (inside the reconnect grace period).
    conn: Option<ConnId>,
    /// Bumped on every (re)connect so a stale grace timer can't expire a live worker.
    generation: u64,
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
            session: self.session.clone(),
            summary: self.summary.clone(),
        }
    }

    /// Append to the transcript, evicting oldest events past `MAX_TRANSCRIPT_BYTES`.
    fn record(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Started { session: Some(s) } => self.session = Some(s.clone()),
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

    pub fn submit(
        &mut self,
        prompt: String,
        cwd: Option<String>,
        harness: Option<String>,
    ) -> (TaskId, Vec<Effect>) {
        let task = Task {
            id: Uuid::new_v4(),
            prompt,
            cwd,
            harness: harness.unwrap_or_else(|| "claude".into()),
        };
        let id = task.id;
        self.tasks.push(TaskEntry::new(task));
        self.queue.push_back(id);
        (id, self.schedule())
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
            worker: wid,
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
                    worker: wid,
                    event,
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
            roots: vec![],
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
        let (task, eff) = l.submit("p".into(), None, None);
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
        let (next, eff) = l.submit("q".into(), None, None);
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
        let (foreign, eff) = l.submit("f".into(), None, None);
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
        let (task, eff) = l.submit("p".into(), None, None);
        assert!(assigned(&eff).is_empty());
        assert_eq!(task_status(&l, task), TaskStatus::Queued);
        let (_, eff) = l.connect(wid, info(), &[], 2);
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
        l.connect(wid, info(), &[], 2);
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
}
