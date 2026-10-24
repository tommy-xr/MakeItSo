//! Headless orchestrator: tracks workers, queues tasks, assigns them to idle
//! workers with a matching harness, and fans agent events out to clients.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use mis_types::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

struct WorkerEntry {
    info: WorkerInfo,
    busy: bool,
    tx: mpsc::UnboundedSender<OrchMsg>,
}

struct TaskEntry {
    task: Task,
    status: TaskStatus,
    worker: Option<WorkerId>,
}

#[derive(Default)]
struct Inner {
    workers: HashMap<WorkerId, WorkerEntry>,
    tasks: Vec<TaskEntry>, // insertion order = submission order
    queue: VecDeque<TaskId>,
}

#[derive(Clone)]
pub struct App {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<ServerMsg>,
    token: Option<String>,
}

impl App {
    /// `token`, if set, must be presented as `Authorization: Bearer <token>`.
    pub fn new(token: Option<String>) -> Self {
        Self {
            inner: Default::default(),
            events: broadcast::channel(256).0,
            token,
        }
    }

    fn status(&self) -> ServerMsg {
        let s = self.inner.lock().unwrap();
        ServerMsg::Status {
            workers: s
                .workers
                .iter()
                .map(|(id, w)| WorkerSummary { id: *id, info: w.info.clone(), busy: w.busy })
                .collect(),
            tasks: s
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

    fn submit(&self, prompt: String, cwd: Option<String>, harness: Option<String>) -> TaskId {
        let task = Task {
            id: Uuid::new_v4(),
            prompt,
            cwd,
            harness: harness.unwrap_or_else(|| "claude".into()),
        };
        let id = task.id;
        let mut s = self.inner.lock().unwrap();
        s.tasks.push(TaskEntry { task, status: TaskStatus::Queued, worker: None });
        s.queue.push_back(id);
        drop(s);
        self.schedule();
        id
    }

    /// Hand queued tasks to idle workers that support the task's harness.
    fn schedule(&self) {
        let mut s = self.inner.lock().unwrap();
        let mut remaining = VecDeque::new();
        while let Some(id) = s.queue.pop_front() {
            let Some(ti) = s.tasks.iter().position(|t| t.task.id == id) else { continue };
            let harness = s.tasks[ti].task.harness.clone();
            let picked = s
                .workers
                .iter()
                .find(|(_, w)| !w.busy && w.info.harnesses.contains(&harness))
                .map(|(id, _)| *id);
            match picked {
                Some(wid) => {
                    let task = s.tasks[ti].task.clone();
                    let w = s.workers.get_mut(&wid).unwrap();
                    w.busy = true;
                    let _ = w.tx.send(OrchMsg::Assign { task });
                    s.tasks[ti].status = TaskStatus::Running;
                    s.tasks[ti].worker = Some(wid);
                }
                None => remaining.push_back(id),
            }
        }
        s.queue = remaining;
    }

    fn on_worker_event(&self, wid: WorkerId, task: TaskId, event: AgentEvent) {
        {
            let mut s = self.inner.lock().unwrap();
            // Only the assigned worker may report on a running task.
            let Some(t) = s.tasks.iter_mut().find(|t| t.task.id == task) else { return };
            if t.worker != Some(wid) || t.status != TaskStatus::Running {
                tracing::warn!(%wid, %task, "ignoring event for task not running on this worker");
                return;
            }
            if let AgentEvent::Finished { ok, .. } = &event {
                t.status = if *ok { TaskStatus::Done } else { TaskStatus::Failed };
                if let Some(w) = s.workers.get_mut(&wid) {
                    w.busy = false;
                }
            }
        }
        let _ = self.events.send(ServerMsg::Event { task, worker: wid, event });
        self.schedule();
    }

    fn remove_worker(&self, wid: WorkerId) {
        let mut s = self.inner.lock().unwrap();
        s.workers.remove(&wid);
        // A task mid-flight on a vanished worker is lost; fail it and tell followers.
        let lost: Vec<TaskId> = s
            .tasks
            .iter_mut()
            .filter(|t| t.worker == Some(wid) && t.status == TaskStatus::Running)
            .map(|t| { t.status = TaskStatus::Failed; t.task.id })
            .collect();
        drop(s);
        for task in lost {
            let event = AgentEvent::Finished { ok: false, summary: Some("worker disconnected".into()) };
            let _ = self.events.send(ServerMsg::Event { task, worker: wid, event });
        }
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        if headers.contains_key("origin") {
            return false;
        }
        match &self.token {
            None => true,
            Some(t) => headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("Bearer {t}"))
                .unwrap_or(false),
        }
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/ws/worker", get(ws_worker))
        .route("/ws/client", get(ws_client))
        .with_state(app)
}

pub async fn serve(listener: TcpListener, app: App) -> anyhow::Result<()> {
    axum::serve(listener, router(app)).await?;
    Ok(())
}

async fn ws_worker(ws: WebSocketUpgrade, State(app): State<App>, headers: HeaderMap) -> impl IntoResponse {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |sock| worker_conn(sock, app)).into_response()
}

async fn ws_client(ws: WebSocketUpgrade, State(app): State<App>, headers: HeaderMap) -> impl IntoResponse {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |sock| client_conn(sock, app)).into_response()
}

async fn recv_json<T: serde::de::DeserializeOwned>(sock: &mut WebSocket) -> Option<T> {
    while let Some(Ok(msg)) = sock.next().await {
        if let Message::Text(t) = msg {
            match serde_json::from_str(&t) {
                Ok(v) => return Some(v),
                Err(e) => tracing::warn!("bad message: {e}"),
            }
        }
    }
    None
}

async fn send_json<T: serde::Serialize>(sock: &mut WebSocket, v: &T) -> bool {
    sock.send(Message::Text(serde_json::to_string(v).unwrap().into())).await.is_ok()
}

async fn worker_conn(mut sock: WebSocket, app: App) {
    let Some(WorkerMsg::Hello { info }) = recv_json::<WorkerMsg>(&mut sock).await else {
        return;
    };
    let wid = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.inner.lock().unwrap().workers.insert(wid, WorkerEntry { info: info.clone(), busy: false, tx });
    tracing::info!(%wid, host = %info.hostname, ?info.harnesses, "worker joined");
    if !send_json(&mut sock, &OrchMsg::Welcome { id: wid }).await {
        app.remove_worker(wid);
        return;
    }
    app.schedule();
    loop {
        tokio::select! {
            Some(out) = rx.recv() => {
                if !send_json(&mut sock, &out).await { break; }
            }
            incoming = recv_json::<WorkerMsg>(&mut sock) => match incoming {
                Some(WorkerMsg::Event { task, event }) => app.on_worker_event(wid, task, event),
                Some(WorkerMsg::Hello { .. }) => {}
                None => break,
            }
        }
    }
    tracing::info!(%wid, "worker left");
    app.remove_worker(wid);
}

/// Next broadcast event; a lagged subscriber gets an Error instead of silently stalling.
async fn next_event(sub: Option<&mut broadcast::Receiver<ServerMsg>>) -> ServerMsg {
    let Some(r) = sub else { return std::future::pending().await };
    loop {
        match r.recv().await {
            Ok(ev) => return ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                return ServerMsg::Error { message: format!("dropped {n} events; re-run status") }
            }
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
    }
}

async fn client_conn(mut sock: WebSocket, app: App) {
    let mut sub: Option<broadcast::Receiver<ServerMsg>> = None;
    loop {
        tokio::select! {
            incoming = recv_json::<ClientMsg>(&mut sock) => match incoming {
                Some(ClientMsg::Status) => { send_json(&mut sock, &app.status()).await; }
                Some(ClientMsg::Submit { prompt, cwd, harness }) => {
                    let task = app.submit(prompt, cwd, harness);
                    send_json(&mut sock, &ServerMsg::Submitted { task }).await;
                }
                Some(ClientMsg::Subscribe) => sub = Some(app.events.subscribe()),
                None => break,
            },
            ev = next_event(sub.as_mut()) => {
                if !send_json(&mut sock, &ev).await { break; }
            }
        }
    }
}
