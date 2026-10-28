//! Pure fleet state: workers, tasks, queue. Every transition returns the
//! effects the shell must perform. No I/O, no clocks, no locks here; `now` is
//! monotonic time from the shell, so wall-clock jumps can't fake silence.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use mis_types::*;
use uuid::Uuid;

use crate::{plan, summary};

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
    /// Run the summarizer on `prompt`; feed its reply to `summarized(task, ..)`.
    Summarize {
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
/// Evidence rows kept per task; the least recently mentioned non-PR rows go first.
pub const MAX_LINKS: usize = 32;
/// A fresh reply's context keeps at most this much of the parent's prompt and answer.
const MAX_CONTEXT_CHARS: usize = 4_000;

struct TaskEntry {
    task: Task,
    status: TaskStatus,
    worker: Option<WorkerId>,
    session: Option<String>,
    /// Model the agent reported in `Started`; shown when none was requested.
    reported_model: Option<String>,
    /// Model the planner reported; credited if it answers the task itself.
    planner_model: Option<String>,
    summary: Option<String>,
    events: Vec<AgentEvent>,
    /// Serialized size of `events`.
    bytes: usize,
    truncated: bool,
    /// Unix millis: submitted, finished.
    started_at: u64,
    finished_at: Option<u64>,
    /// Latest `Usage` (input, output).
    tokens: Option<(u64, u64)>,
    /// Latest tool call or text, rendered short (see `step`).
    step: Option<String>,
    /// The task this one replies to.
    parent: Option<TaskId>,
    /// Only this worker may run it: it holds the session being resumed.
    pin: Option<WorkerId>,
    /// Evidence: GitHub links in the agent's output and items it reported, most
    /// recently mentioned last. Kept apart from `events`, so transcript eviction
    /// never drops them.
    links: Vec<Link>,
    /// The agent reported a usage limit.
    limited: bool,
    /// A client opened it after it finished.
    seen: bool,
}

/// What a task needs from the user; opening it once finished clears it.
fn attention(status: TaskStatus, limited: bool, seen: bool) -> Attention {
    match status {
        _ if seen => Attention::None,
        TaskStatus::Done => Attention::Unread,
        TaskStatus::Failed if limited => Attention::Limited,
        TaskStatus::Failed => Attention::Failed,
        _ => Attention::None,
    }
}

/// Activity text for an event worth showing: `> Bash ls`, `status: <text>`, or
/// the first line of text. The latest of these is the task's activity.
fn step(event: &AgentEvent) -> Option<String> {
    let line = match event {
        AgentEvent::ToolUse { name, input } => {
            // The argument that says what the call is about, else the first string one.
            let arg = ["command", "file_path", "pattern", "path", "url", "query"]
                .iter()
                .find_map(|k| input[*k].as_str())
                .or_else(|| input.as_object()?.values().find_map(|v| v.as_str()))
                .unwrap_or("");
            format!("> {name} {arg}")
        }
        AgentEvent::Text { text } if !text.trim().is_empty() => text.clone(),
        AgentEvent::Status { text } => format!("status: {text}"),
        _ => return None,
    };
    Some(first_line(line.trim_end(), 80))
}

impl TaskEntry {
    fn new(task: Task, now_ms: u64) -> Self {
        Self {
            task,
            status: TaskStatus::Queued,
            worker: None,
            session: None,
            reported_model: None,
            planner_model: None,
            summary: None,
            events: vec![],
            bytes: 0,
            truncated: false,
            started_at: now_ms,
            finished_at: None,
            tokens: None,
            step: None,
            parent: None,
            pin: None,
            links: vec![],
            limited: false,
            seen: false,
        }
    }

    /// What the task is doing now, for one-line displays.
    fn activity(&self) -> String {
        match self.status {
            TaskStatus::Planning => "planning…".into(),
            TaskStatus::Queued => "queued".into(),
            TaskStatus::Running => self.step.clone().unwrap_or_else(|| "started".into()),
            TaskStatus::Done => "done".into(),
            TaskStatus::Failed => "FAILED".into(),
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
            started_at: Some(self.started_at),
            finished_at: self.finished_at,
            tokens_in: self.tokens.map(|t| t.0),
            tokens_out: self.tokens.map(|t| t.1),
            activity: self.activity(),
            parent: self.parent,
            links: self.links.clone(),
            attention: attention(self.status, self.limited, self.seen),
        }
    }

    /// Append to the transcript, evicting oldest events past `MAX_TRANSCRIPT_BYTES`.
    /// `Usage` only updates the counts; it isn't kept. Links are collected first,
    /// from the full text, before any truncation; a link mentioned again moves to
    /// the end, so the agent's closing PR outlives links from tool output it read.
    fn record(&mut self, event: &AgentEvent, now_ms: u64) {
        if let Some(s) = step(event) {
            self.step = Some(s);
        }
        let text = match event {
            AgentEvent::Text { text } => Some(text),
            AgentEvent::ToolResult { content } => Some(content),
            AgentEvent::Finished { summary, .. } => summary.as_ref(),
            _ => None,
        };
        let mut links = text.map(|t| github_links(t)).unwrap_or_default();
        if let AgentEvent::Evidence(link) = event {
            links.push(link.clone());
        }
        for link in links {
            self.links.retain(|l| key(l) != key(&link));
            // Full: drop the oldest row that isn't a PR, the task's most telling link.
            if self.links.len() == MAX_LINKS {
                let i = self.links.iter().position(|l| l.kind != LinkKind::Pr);
                self.links.remove(i.unwrap_or(0));
            }
            self.links.push(link);
        }
        match event {
            AgentEvent::Usage { input, output } => {
                self.tokens = Some((*input, *output));
                return;
            }
            AgentEvent::Started { session, model } => {
                if session.is_some() {
                    self.session = session.clone();
                }
                self.reported_model = model.clone();
            }
            AgentEvent::Finished { summary, .. } => {
                self.summary = summary.clone();
                self.finished_at = Some(now_ms);
            }
            AgentEvent::Limited { .. } => self.limited = true,
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

/// Evidence rows that replace each other: tests and notes by title (a re-run
/// updates its verdict), links by URL (a reported PR and a mentioned one match).
fn key(l: &Link) -> (Option<LinkKind>, &str) {
    match l.kind {
        LinkKind::Test | LinkKind::Note => (Some(l.kind), &l.label),
        _ => (None, &l.url),
    }
}

fn size(e: &AgentEvent) -> usize {
    serde_json::to_vec(e).map_or(0, |v| v.len())
}

#[derive(Default)]
pub struct Ledger {
    /// Unix millis of the transition in progress, set by the shell before each
    /// one; only stamped on tasks for display.
    pub now_ms: u64,
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
            now_ms: Some(self.now_ms),
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
        let mut entry = TaskEntry::new(
            Task {
                id,
                prompt,
                cwd,
                // Empty until planned; Planning tasks are never queued or assigned.
                harness: harness.unwrap_or_default(),
                model,
                resume: None,
            },
            self.now_ms,
        );
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

    /// Reply `text` in `parent`'s finished thread. A parent run on a worker with
    /// a session is resumed there: same harness, model and cwd, queued until that
    /// worker is idle; Err if it is offline. Otherwise (answered by the planner,
    /// or no session) the reply is a fresh task carrying the exchange as context,
    /// planned like any other; `planner` false defaults it to claude instead.
    pub fn reply(
        &mut self,
        parent: TaskId,
        text: String,
        planner: bool,
    ) -> Result<(TaskId, Vec<Effect>), String> {
        let p = self
            .tasks
            .iter()
            .find(|t| t.task.id == parent)
            .ok_or_else(|| format!("unknown task {parent}"))?;
        let short = &parent.to_string()[..8];
        if !matches!(p.status, TaskStatus::Done | TaskStatus::Failed) {
            return Err(format!("task {short} hasn't finished; reply once it has"));
        }
        let by_planner = p.task.harness == "planner" || p.task.harness.is_empty();
        let (id, effects) = match (&p.session, p.worker) {
            (Some(session), Some(wid)) if !by_planner => {
                let host = &wid.to_string()[..8];
                let Some(w) = self.workers.get(&wid).filter(|w| w.conn.is_some()) else {
                    return Err(format!(
                        "worker {host} is offline; task {short}'s session lives there"
                    ));
                };
                if !w.info.harnesses.contains(&p.task.harness) {
                    return Err(format!("worker {host} no longer runs {}", p.task.harness));
                }
                let task = Task {
                    id: Uuid::new_v4(),
                    prompt: text,
                    resume: Some(session.clone()),
                    ..p.task.clone()
                };
                let mut entry = TaskEntry::new(task, self.now_ms);
                entry.pin = Some(wid);
                let id = entry.task.id;
                self.tasks.push(entry);
                self.queue.push_back(id);
                (id, self.schedule())
            }
            _ => {
                let prompt = reply_context(&p.task.prompt, p.summary.as_deref(), &text);
                let (harness, model) = match by_planner {
                    true => (None, None),
                    false => (Some(p.task.harness.clone()), p.task.model.clone()),
                };
                let harness = harness.or_else(|| (!planner).then(|| "claude".into()));
                self.submit(prompt, p.task.cwd.clone(), harness, model)
            }
        };
        let t = self.tasks.iter_mut().find(|t| t.task.id == id);
        t.expect("just added").parent = Some(parent);
        Ok((id, effects))
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
        let now = self.now_ms;
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
                        t.task.model = t.planner_model.take();
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
                        events.iter().for_each(|e| t.record(e, now));
                        events.into_iter().map(broadcast).collect()
                    }
                    Ok(p) => {
                        // The worker reports its own; the planner's would mislead.
                        t.tokens = None;
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
                        events.iter().for_each(|e| t.record(e, now));
                        // Not the planner's reason: the worker hasn't started yet.
                        t.step = None;
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
                        t.record(&event, now);
                        vec![broadcast(event)]
                    }
                }
            }
            AgentEvent::Started { model, .. } => {
                t.planner_model = model;
                vec![]
            }
            AgentEvent::Usage { input, output } => {
                t.tokens = Some((input, output));
                vec![broadcast(AgentEvent::Usage { input, output })]
            }
            AgentEvent::Limited { .. } => {
                t.record(&event, now);
                vec![broadcast(event)]
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
        // Replies waiting for this worker can't run if it dropped their harness.
        let harnesses = self.workers[&wid].info.harnesses.clone();
        effects.extend(
            self.fail_pinned(wid, "worker no longer runs this harness", |t| {
                !harnesses.contains(&t.task.harness)
            }),
        );
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
    /// Ignored unless `conn` is live. New usage-limit readings go out in a
    /// Status at once; load figures wait for the next one.
    pub fn worker_stats(
        &mut self,
        wid: WorkerId,
        conn: ConnId,
        stats: WorkerStats,
        now: Duration,
        unix_secs: u64,
    ) -> Vec<Effect> {
        let Some(w) = self.workers.get_mut(&wid).filter(|w| w.conn == Some(conn)) else {
            return vec![];
        };
        let changed = w.stats.take().map(|(s, _)| s.limits).unwrap_or_default() != stats.limits;
        w.stats = Some((stats, unix_secs));
        w.last_seen = now;
        if changed {
            vec![Effect::Broadcast(self.status())]
        } else {
            vec![]
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
        let mut effects = self.fail_running(wid, &[], "worker disconnected");
        // Replies waiting for this worker's session can never run now.
        effects.extend(self.fail_pinned(wid, "worker left; its session is gone", |_| true));
        effects
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
        let now = self.now_ms;
        let Some(t) = self.tasks.iter_mut().find(|t| t.task.id == task) else {
            return vec![];
        };
        if t.worker != Some(wid) || t.status != TaskStatus::Running {
            return vec![];
        }
        t.record(&event, now);
        let mut summarize = None;
        if let AgentEvent::Finished { ok, .. } = &event {
            t.status = if *ok {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
            // Truncated: the transcript was too long to be a short answer.
            let wanted = t.truncated || summary::wanted(&t.events, t.summary.as_deref());
            if *ok && wanted {
                summarize = Some(Effect::Summarize {
                    task,
                    prompt: summary::prompt(&t.task.prompt, &t.events),
                });
            }
        }
        let mut effects = vec![Effect::Broadcast(ServerMsg::Event {
            task,
            worker: Some(wid),
            event,
        })];
        effects.extend(summarize);
        effects.extend(self.schedule());
        effects
    }

    /// Summarizer reply for a Done task: replaces the agent's own summary unless
    /// blank, and broadcasts a fresh Status so open views pick it up.
    pub fn summarized(&mut self, task: TaskId, reply: &str) -> Vec<Effect> {
        let text = summary::parse(reply);
        let Some(t) = self.tasks.iter_mut().find(|t| t.task.id == task) else {
            return vec![];
        };
        if text.is_empty() || t.status != TaskStatus::Done {
            return vec![];
        }
        t.summary = Some(text);
        vec![Effect::Broadcast(self.status())]
    }

    /// A client opened `task`. Only a finished task can be seen; a fresh Status
    /// tells every client it no longer needs attention.
    pub fn seen(&mut self, task: TaskId) -> Vec<Effect> {
        let Some(t) = self.tasks.iter_mut().find(|t| t.task.id == task) else {
            return vec![];
        };
        if t.seen || !matches!(t.status, TaskStatus::Done | TaskStatus::Failed) {
            return vec![];
        }
        t.seen = true;
        vec![Effect::Broadcast(self.status())]
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
            let pin = self.tasks[ti].pin;
            let picked = self
                .workers
                .iter()
                .filter(|(wid, _)| pin.is_none_or(|p| p == **wid))
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
        self.fail_where(wid, why, |t| {
            t.worker == Some(wid) && t.status == TaskStatus::Running && !keep.contains(&t.task.id)
        })
    }

    /// Fail queued replies pinned to `wid` that match `pred`, and unqueue them.
    fn fail_pinned(
        &mut self,
        wid: WorkerId,
        why: &str,
        pred: impl Fn(&TaskEntry) -> bool,
    ) -> Vec<Effect> {
        let effects = self.fail_where(wid, why, |t| {
            t.pin == Some(wid) && t.status == TaskStatus::Queued && pred(t)
        });
        let tasks = &self.tasks;
        self.queue.retain(|id| {
            tasks
                .iter()
                .any(|t| t.task.id == *id && t.status == TaskStatus::Queued)
        });
        effects
    }

    /// Fail every task matching `pred`, telling followers why (as from `wid`).
    fn fail_where(
        &mut self,
        wid: WorkerId,
        why: &str,
        pred: impl Fn(&TaskEntry) -> bool,
    ) -> Vec<Effect> {
        let now = self.now_ms;
        self.tasks
            .iter_mut()
            .filter(|t| pred(t))
            .map(|t| {
                t.status = TaskStatus::Failed;
                let event = AgentEvent::Finished {
                    ok: false,
                    summary: Some(why.into()),
                };
                t.record(&event, now);
                Effect::Broadcast(ServerMsg::Event {
                    task: t.task.id,
                    worker: Some(wid),
                    event,
                })
            })
            .collect()
    }
}

/// Prompt for a reply that can't resume a session: the follow-up first (so
/// lists show it), then the earlier exchange it follows.
fn reply_context(prompt: &str, answer: Option<&str>, text: &str) -> String {
    let cut = |s: &str| summary::cut(s.trim(), MAX_CONTEXT_CHARS);
    format!(
        "{text}\n\n---\nThis follows up an earlier request.\nEarlier request:\n{}\n\n\
         Earlier answer:\n{}",
        cut(prompt),
        cut(answer.unwrap_or("(none)"))
    )
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
    fn new_limit_readings_are_broadcast_load_alone_is_not() {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1, T0);
        let window = |used_pct, observed_at| LimitWindow {
            name: "5h".into(),
            used_pct,
            resets_at: None,
            observed_at,
        };
        let with = |w: Vec<LimitWindow>| WorkerStats {
            limits: [("claude".to_string(), w)].into(),
            ..stats(5)
        };
        let broadcasts = |e: Vec<Effect>| {
            e.iter()
                .filter(|e| matches!(e, Effect::Broadcast(_)))
                .count()
        };
        assert_eq!(broadcasts(l.worker_stats(wid, 1, stats(5), secs(1), 0)), 0);
        assert_eq!(
            broadcasts(l.worker_stats(wid, 1, with(vec![window(10.0, 1)]), secs(2), 0)),
            1
        );
        // Same reading again, other load: nothing new to show.
        assert_eq!(
            broadcasts(l.worker_stats(
                wid,
                1,
                WorkerStats {
                    cpu_pct: 90,
                    ..with(vec![window(10.0, 1)])
                },
                secs(3),
                0
            )),
            0
        );
        assert_eq!(
            broadcasts(l.worker_stats(wid, 1, with(vec![window(12.0, 2)]), secs(4), 0)),
            1
        );
        let ServerMsg::Status { workers, .. } = l.status() else {
            unreachable!()
        };
        assert_eq!(
            workers[0].stats.as_ref().unwrap().limits["claude"][0].used_pct,
            12.0
        );
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
    }

    #[test]
    fn planner_model_is_not_shown_unless_it_answers() {
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
        assert_eq!(l.show(task).unwrap().0.model, None);
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
    fn step_renders_tool_calls_and_text_short() {
        let tool = |name: &str, input| AgentEvent::ToolUse {
            name: name.into(),
            input,
        };
        use serde_json::json;
        let cases = [
            (
                tool("Bash", json!({"description": "list", "command": "ls -la"})),
                Some("> Bash ls -la"),
            ),
            (
                tool("Grep", json!({"glob": "*.rs", "pattern": "fn main"})),
                Some("> Grep fn main"),
            ),
            (
                tool("Custom", json!({"n": 1, "x": "y"})),
                Some("> Custom y"),
            ),
            (tool("TodoWrite", json!({"todos": []})), Some("> TodoWrite")),
            (text("\n  first line\nsecond"), Some("first line")),
            (text(" \n"), None),
            (
                AgentEvent::ToolResult {
                    content: "out".into(),
                },
                None,
            ),
        ];
        for (event, want) in cases {
            assert_eq!(step(&event).as_deref(), want, "{event:?}");
        }
        let long = step(&text(&"x".repeat(200))).unwrap();
        assert_eq!(long.chars().count(), 80);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn activity_timing_and_tokens_follow_the_task() {
        let summary = |l: &Ledger, t| l.show(t).unwrap().0;
        let mut l = Ledger {
            now_ms: 1_000,
            ..Default::default()
        };
        let (planned, _) = l.submit("p".into(), None, None, None);
        assert_eq!(summary(&l, planned).activity, "planning…");
        let (queued, _) = l.submit("q".into(), None, Some("claude".into()), None);
        assert_eq!(summary(&l, queued).activity, "queued");

        let wid = Uuid::new_v4();
        l.connect(wid, info(), &[], 1, T0);
        assert_eq!(summary(&l, queued).activity, "started");
        l.event(wid, 1, queued, text("Looking around.\nMore."));
        assert_eq!(summary(&l, queued).activity, "Looking around.");
        let tool = AgentEvent::ToolUse {
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        l.event(wid, 1, queued, tool);
        // Results and usage keep the last step; usage replaces, and isn't kept.
        l.event(
            wid,
            1,
            queued,
            AgentEvent::ToolResult {
                content: "a".into(),
            },
        );
        for (input, output) in [(100, 5), (250, 9)] {
            let eff = l.event(wid, 1, queued, AgentEvent::Usage { input, output });
            assert_eq!(eff.len(), 1, "broadcast");
        }
        let s = summary(&l, queued);
        assert_eq!(s.activity, "> Bash ls");
        assert_eq!((s.tokens_in, s.tokens_out), (Some(250), Some(9)));
        assert_eq!((s.started_at, s.finished_at), (Some(1_000), None));
        assert!(!l
            .show(queued)
            .unwrap()
            .1
            .iter()
            .any(|e| matches!(e, AgentEvent::Usage { .. })));

        l.now_ms = 5_000;
        l.event(
            wid,
            1,
            queued,
            AgentEvent::Finished {
                ok: false,
                summary: None,
            },
        );
        let s = summary(&l, queued);
        assert_eq!(
            (s.activity.as_str(), s.finished_at),
            ("FAILED", Some(5_000))
        );

        // Planner tokens show on a task it answers, not on one it dispatches.
        let usage = AgentEvent::Usage {
            input: 4_000,
            output: 20,
        };
        // Broadcast so followers refresh.
        assert_eq!(l.planning_event(planned, usage.clone()).len(), 1);
        l.planning_event(
            planned,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"harness":"claude","reason":"r"}"#.into()),
            },
        );
        let s = summary(&l, planned);
        assert_eq!((s.tokens_in, s.activity.as_str()), (None, "started"));
        let (answered, _) = l.submit("2+2".into(), None, None, None);
        l.planning_event(answered, usage);
        l.planning_event(
            answered,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"answer":"4"}"#.into()),
            },
        );
        let s = summary(&l, answered);
        assert_eq!(
            (s.tokens_in, s.activity.as_str(), s.finished_at),
            (Some(4_000), "done", Some(5_000))
        );
    }

    #[test]
    fn finished_work_is_summarized_and_replaced() {
        let (mut l, wid, task) = running_setup();
        let summarize = |eff: &[Effect]| {
            eff.iter().find_map(|e| match e {
                Effect::Summarize { task, prompt } => Some((*task, prompt.clone())),
                _ => None,
            })
        };
        l.event(
            wid,
            1,
            task,
            AgentEvent::ToolUse {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        );
        let eff = l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some("raw".into()),
            },
        );
        let (t, prompt) = summarize(&eff).expect("summarize");
        assert_eq!(t, task);
        assert!(prompt.contains("> Bash"));
        // Agent's own summary stands until the better one arrives; still Done.
        assert_eq!(l.show(task).unwrap().0.summary.as_deref(), Some("raw"));
        assert!(l.summarized(task, " \n").is_empty(), "blank keeps it");
        let eff = l.summarized(task, "Listed files.\nAll fine.");
        assert!(matches!(
            &eff[..],
            [Effect::Broadcast(ServerMsg::Status { .. })]
        ));
        let s = l.show(task).unwrap().0;
        assert_eq!(
            (s.summary.as_deref(), s.status),
            (Some("Listed files. All fine."), TaskStatus::Done)
        );
        assert!(l.summarized(Uuid::new_v4(), "x").is_empty());
    }

    #[test]
    fn evicted_tool_calls_still_count_as_work() {
        let (mut l, wid, task) = running_setup();
        let big = "x".repeat(MAX_TOOL_RESULT_BYTES - 100);
        for _ in 0..(MAX_TRANSCRIPT_BYTES / big.len() + 2) {
            l.event(wid, 1, task, text(&big));
        }
        let eff = l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some("ok".into()),
            },
        );
        assert!(l.show(task).unwrap().2, "truncated");
        assert!(eff.iter().any(|e| matches!(e, Effect::Summarize { .. })));
    }

    #[test]
    fn short_answers_and_failures_are_not_summarized() {
        let summarizes = |eff: &[Effect]| eff.iter().any(|e| matches!(e, Effect::Summarize { .. }));
        // One short text answer.
        let (mut l, wid, task) = running_setup();
        l.event(wid, 1, task, text("hi"));
        let eff = l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some("hi".into()),
            },
        );
        assert!(!summarizes(&eff));
        // Failed after tools: the failure reason stays.
        let (mut l, wid, task) = running_setup();
        l.event(
            wid,
            1,
            task,
            AgentEvent::ToolUse {
                name: "Bash".into(),
                input: serde_json::json!({}),
            },
        );
        let fail = AgentEvent::Finished {
            ok: false,
            summary: Some("boom".into()),
        };
        assert!(!summarizes(&l.event(wid, 1, task, fail)));
        assert!(l.summarized(task, "better").is_empty());
        assert_eq!(l.show(task).unwrap().0.summary.as_deref(), Some("boom"));
        // Planner answers never reach `event`, so never summarize.
        let mut l = Ledger::default();
        let (task, _) = l.submit("2+2".into(), None, None, None);
        let eff = l.planning_event(
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"answer":"4"}"#.into()),
            },
        );
        assert!(!summarizes(&eff));
    }

    #[test]
    fn latest_status_or_step_is_the_activity() {
        let (mut l, wid, task) = running_setup();
        let activity = |l: &Ledger| l.show(task).unwrap().0.activity;
        let status = |t: &str| AgentEvent::Status { text: t.into() };
        l.event(wid, 1, task, text("Looking."));
        let eff = l.event(wid, 1, task, status("Counting repos"));
        assert_eq!(eff.len(), 1, "broadcast");
        assert_eq!(activity(&l), "status: Counting repos");
        // Results don't replace it; the next tool call does, and a newer status wins again.
        l.event(
            wid,
            1,
            task,
            AgentEvent::ToolResult {
                content: "x".into(),
            },
        );
        assert_eq!(activity(&l), "status: Counting repos");
        l.event(
            wid,
            1,
            task,
            AgentEvent::ToolUse {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        );
        assert_eq!(activity(&l), "> Bash ls");
        l.event(wid, 1, task, status("Writing summary"));
        assert_eq!(activity(&l), "status: Writing summary");
        assert!(l.show(task).unwrap().1.contains(&status("Counting repos")));
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

    /// `running_setup` with the task finished under session `s1`.
    fn finished_setup() -> (Ledger, WorkerId, TaskId) {
        let (mut l, wid, task) = running_setup();
        for e in [
            AgentEvent::Started {
                session: Some("s1".into()),
                model: None,
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("three".into()),
            },
        ] {
            l.event(wid, 1, task, e);
        }
        (l, wid, task)
    }

    fn sent(effects: &[Effect]) -> Vec<Task> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send {
                    msg: OrchMsg::Assign { task },
                    ..
                } => Some(task.clone()),
                _ => None,
            })
            .collect()
    }

    fn attention_of(l: &Ledger, task: TaskId) -> Attention {
        l.show(task).unwrap().0.attention
    }

    fn finish(l: &mut Ledger, wid: WorkerId, task: TaskId, ok: bool) {
        let summary = None;
        l.event(wid, 1, task, AgentEvent::Finished { ok, summary });
    }

    #[test]
    fn attention_needs_a_finish_and_clears_once_seen() {
        let (mut l, wid, task) = running_setup();
        assert_eq!(attention_of(&l, task), Attention::None);
        // Seen before it finishes: ignored, so the result still shows as unread.
        assert!(l.seen(task).is_empty());
        finish(&mut l, wid, task, true);
        assert_eq!(attention_of(&l, task), Attention::Unread);
        let eff = l.seen(task);
        let [Effect::Broadcast(ServerMsg::Status { tasks, .. })] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(tasks[0].attention, Attention::None);
        assert!(l.seen(task).is_empty(), "seen twice: nothing new to tell");
        assert!(l.seen(Uuid::new_v4()).is_empty());

        let (failed, _) = l.submit("q".into(), None, Some("claude".into()), None);
        finish(&mut l, wid, failed, false);
        assert_eq!(attention_of(&l, failed), Attention::Failed);
        l.seen(failed);
        assert_eq!(attention_of(&l, failed), Attention::None);
    }

    #[test]
    fn worker_loss_fails_into_attention() {
        let (mut l, wid, task) = running_setup();
        l.disconnect(wid, 1);
        l.expire(wid, 1);
        assert_eq!(attention_of(&l, task), Attention::Failed);
    }

    #[test]
    fn usage_limit_shows_once_it_fails_until_seen() {
        let limited = AgentEvent::Limited {
            resets_at: Some(1_800_000_000),
        };
        let (mut l, wid, task) = running_setup();
        l.event(wid, 1, task, limited.clone());
        assert_eq!(attention_of(&l, task), Attention::None, "still running");
        finish(&mut l, wid, task, false);
        assert_eq!(
            attention_of(&l, task),
            Attention::Limited,
            "outranks failed"
        );
        assert!(
            l.show(task).unwrap().1.contains(&limited),
            "kept in the transcript"
        );
        l.seen(task);
        assert_eq!(attention_of(&l, task), Attention::None);

        // Limited, then carried on and finished ok: just a result to read.
        let (next, _) = l.submit("q".into(), None, Some("claude".into()), None);
        l.event(wid, 1, next, limited);
        finish(&mut l, wid, next, true);
        assert_eq!(attention_of(&l, next), Attention::Unread);
    }

    #[test]
    fn planner_usage_limit_counts_too() {
        let mut l = Ledger::default();
        let (task, _) = l.submit("p".into(), None, None, None);
        let limited = AgentEvent::Limited { resets_at: None };
        assert_eq!(l.planning_event(task, limited.clone()).len(), 1);
        let failed = AgentEvent::Finished {
            ok: false,
            summary: None,
        };
        l.planning_event(task, failed);
        assert_eq!(attention_of(&l, task), Attention::Limited);
        assert!(l.show(task).unwrap().1.contains(&limited));
    }

    #[test]
    fn replies_carry_their_own_attention() {
        let (mut l, wid, parent) = finished_setup();
        l.seen(parent);
        let (reply, _) = l.reply(parent, "and tests?".into(), true).unwrap();
        assert_eq!(attention_of(&l, reply), Attention::None);
        finish(&mut l, wid, reply, false);
        assert_eq!(
            (attention_of(&l, parent), attention_of(&l, reply)),
            (Attention::None, Attention::Failed)
        );
    }

    #[test]
    fn reply_resumes_on_the_parents_worker_once_idle() {
        let (mut l, wid, parent) = finished_setup();
        // Busy the parent's worker; an idle second worker must not take the reply.
        let (busy, _) = l.submit("other".into(), None, Some("claude".into()), None);
        l.connect(Uuid::new_v4(), info(), &[], 2, T0);
        let (reply, eff) = l.reply(parent, "and tests?".into(), true).unwrap();
        assert!(assigned(&eff).is_empty());
        assert_eq!(l.show(reply).unwrap().0.parent, Some(parent));
        let eff = l.event(
            wid,
            1,
            busy,
            AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        );
        let [t] = &sent(&eff)[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(assigned(&eff), vec![(1, reply)]);
        assert_eq!(
            (t.prompt.as_str(), t.harness.as_str(), t.resume.as_deref()),
            ("and tests?", "claude", Some("s1"))
        );
    }

    #[test]
    fn reply_needs_a_finished_parent_and_its_worker_online() {
        let (mut l, wid, task) = running_setup();
        assert!(l
            .reply(task, "x".into(), true)
            .unwrap_err()
            .contains("hasn't finished"));
        assert!(l.reply(Uuid::nil(), "x".into(), true).is_err());
        let (mut l, _, parent) = {
            l.event(
                wid,
                1,
                task,
                AgentEvent::Started {
                    session: Some("s1".into()),
                    model: None,
                },
            );
            l.event(
                wid,
                1,
                task,
                AgentEvent::Finished {
                    ok: false,
                    summary: None,
                },
            );
            (l, wid, task)
        };
        l.disconnect(wid, 1);
        let why = l.reply(parent, "x".into(), true).unwrap_err();
        assert!(why.contains("offline"), "{why}");
    }

    #[test]
    fn queued_reply_fails_when_its_worker_leaves() {
        let (mut l, wid, parent) = finished_setup();
        l.submit("busy".into(), None, Some("claude".into()), None);
        let (reply, _) = l.reply(parent, "x".into(), true).unwrap();
        l.disconnect(wid, 1);
        let eff = l.expire(wid, 1);
        assert_eq!(finished(&eff, reply), Some(false));
        assert_eq!(task_status(&l, reply), TaskStatus::Failed);
        // Gone from the queue: a new worker doesn't pick it up.
        let (_, eff) = l.connect(Uuid::new_v4(), info(), &[], 3, T0);
        assert!(assigned(&eff).is_empty());
    }

    #[test]
    fn queued_reply_fails_when_its_worker_drops_the_harness() {
        let (mut l, wid, parent) = finished_setup();
        let (busy, _) = l.submit("busy".into(), None, Some("claude".into()), None);
        let (reply, _) = l.reply(parent, "x".into(), true).unwrap();
        let mut codex = info();
        codex.harnesses = vec!["codex".into()];
        let (_, eff) = l.connect(wid, codex, &[busy], 2, T0);
        assert_eq!(finished(&eff, reply), Some(false));
        assert!(!l.queue.contains(&reply));
        let why = l.reply(parent, "y".into(), true).unwrap_err();
        assert!(why.contains("no longer runs claude"), "{why}");
    }

    #[test]
    fn reply_to_planner_answer_is_planned_with_context() {
        let mut l = Ledger::default();
        l.connect(Uuid::new_v4(), info(), &[], 1, T0);
        let (parent, _) = l.submit("what is 2+2?".into(), None, None, None);
        l.planning_event(
            parent,
            AgentEvent::Finished {
                ok: true,
                summary: Some(r#"{"answer":"4"}"#.into()),
            },
        );
        let (reply, eff) = l.reply(parent, "and 3+3?".into(), true).unwrap();
        let [Effect::Plan { task, prompt }] = &eff[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(*task, reply);
        let t = l.show(reply).unwrap().0;
        assert_eq!((t.parent, t.status), (Some(parent), TaskStatus::Planning));
        assert!(t.prompt.starts_with("and 3+3?\n\n---\n"), "{}", t.prompt);
        assert!(t
            .prompt
            .ends_with("Earlier request:\nwhat is 2+2?\n\nEarlier answer:\n4"));
        assert!(prompt.contains("and 3+3?"));
        // No planner: straight to a claude worker, still with the context.
        let (reply, eff) = l.reply(parent, "and 4+4?".into(), false).unwrap();
        let [t] = &sent(&eff)[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(
            (t.id, t.harness.as_str(), t.resume.as_deref()),
            (reply, "claude", None)
        );
        assert!(t.prompt.starts_with("and 4+4?\n"));
    }

    #[test]
    fn reply_without_a_session_reruns_with_context_on_any_worker() {
        let (mut l, wid, parent) = running_setup();
        l.event(
            wid,
            1,
            parent,
            AgentEvent::Finished {
                ok: false,
                summary: Some("boom".into()),
            },
        );
        let (reply, eff) = l.reply(parent, "retry".into(), true).unwrap();
        let [t] = &sent(&eff)[..] else {
            panic!("{eff:?}")
        };
        assert_eq!(
            (t.id, t.harness.as_str(), t.resume.as_deref()),
            (reply, "claude", None)
        );
        assert!(t.prompt.ends_with("Earlier answer:\nboom"));
    }

    fn link_urls(l: &Ledger, task: TaskId) -> Vec<String> {
        let links = l.show(task).unwrap().0.links;
        links.into_iter().map(|l| l.url).collect()
    }

    #[test]
    fn links_survive_eviction_and_summary_replacement() {
        let (mut l, wid, task) = running_setup();
        let pr = "https://github.com/o/r/pull/42";
        let run = "https://github.com/o/r/actions/runs/7";
        l.event(wid, 1, task, text(&format!("Opened {pr}.")));
        let content = format!("{run}/job/1\n{}", "x".repeat(2 * MAX_TOOL_RESULT_BYTES));
        l.event(wid, 1, task, AgentEvent::ToolResult { content });
        // Push both out of the transcript.
        let chunk = "x".repeat(10 * 1024);
        for _ in 0..40 {
            l.event(wid, 1, task, text(&chunk));
        }
        let (_, events, truncated) = l.show(task).unwrap();
        assert!(truncated && !events.iter().any(|e| format!("{e:?}").contains(pr)));
        let sha = "https://github.com/o/r/commit/abcdef1";
        let eff = l.event(
            wid,
            1,
            task,
            AgentEvent::Finished {
                ok: true,
                summary: Some(format!("See {pr} and {sha}")),
            },
        );
        assert!(eff.iter().any(|e| matches!(e, Effect::Summarize { .. })));
        l.summarized(task, "Fixed it.");
        let s = l.show(task).unwrap().0;
        assert_eq!(s.summary.as_deref(), Some("Fixed it."));
        // The PR, mentioned again at the end, moves last.
        assert_eq!(link_urls(&l, task), [run, pr, sha]);
        let ServerMsg::Status { tasks, .. } = l.status() else {
            panic!()
        };
        assert_eq!(tasks[0].links, s.links);
    }

    #[test]
    fn links_are_bounded_keeping_the_latest_mentioned() {
        let (mut l, wid, task) = running_setup();
        let many: Vec<_> = (0..MAX_LINKS + 5)
            .map(|i| format!("https://github.com/o/r/pull/{i}"))
            .collect();
        // Tool output full of links, then the agent names its own PR (#0, seen first).
        let content = many.join(" ");
        l.event(wid, 1, task, AgentEvent::ToolResult { content });
        l.event(wid, 1, task, text(&many[0]));
        let got = link_urls(&l, task);
        assert_eq!(got.len(), MAX_LINKS);
        assert_eq!(got[..MAX_LINKS - 1], many[6..]);
        assert_eq!(got.last(), Some(&many[0]));
    }

    fn reported(kind: LinkKind, url: &str, label: &str, ok: Option<bool>) -> AgentEvent {
        AgentEvent::Evidence(Link {
            kind,
            url: url.into(),
            label: label.into(),
            ok,
            detail: None,
        })
    }

    #[test]
    fn reported_evidence_merges_with_mentioned_links() {
        let (mut l, wid, task) = running_setup();
        let pr = "https://github.com/o/r/pull/42";
        let preview = "https://preview.example.com/";
        l.event(wid, 1, task, text(&format!("Opened {pr}")));
        let test = |ok| reported(LinkKind::Test, "", "cargo test", Some(ok));
        for e in [
            test(false),
            reported(LinkKind::Note, "", "cargo test", None),
            reported(LinkKind::Url, preview, "preview", None),
            // As the worker sends a reported GitHub URL: classified, canonical.
            AgentEvent::Evidence(github_link(pr).unwrap()),
            test(true),
        ] {
            l.event(wid, 1, task, e);
        }
        // One PR row, moved last when reported; the re-run test replaced the failed one.
        let links = l.show(task).unwrap().0.links;
        let got: Vec<_> = links
            .iter()
            .map(|l| (l.kind, l.url.as_str(), l.ok))
            .collect();
        assert_eq!(
            got,
            [
                (LinkKind::Note, "", None),
                (LinkKind::Url, preview, None),
                (LinkKind::Pr, pr, None),
                (LinkKind::Test, "", Some(true)),
            ]
        );
        // Kept in the transcript too.
        let (_, events, _) = l.show(task).unwrap();
        assert!(matches!(events.last(), Some(AgentEvent::Evidence(_))));
    }

    #[test]
    fn reported_evidence_shares_the_bound_keeping_prs() {
        let (mut l, wid, task) = running_setup();
        let pr = "https://github.com/o/r/pull/1";
        l.event(wid, 1, task, text(pr));
        for i in 0..MAX_LINKS + 1 {
            l.event(
                wid,
                1,
                task,
                reported(LinkKind::Note, "", &i.to_string(), None),
            );
        }
        let links = l.show(task).unwrap().0.links;
        assert_eq!(links.len(), MAX_LINKS);
        assert_eq!((links[0].url.as_str(), links[1].label.as_str()), (pr, "2"));
    }
}
