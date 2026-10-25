//! Fake worker + client drive the orchestrator end to end over real WebSockets.

use futures_util::{SinkExt, StreamExt};
use mis_orchestrator::App;
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn send<T: serde::Serialize>(ws: &mut Ws, v: &T) {
    ws.send(Message::Text(serde_json::to_string(v).unwrap().into()))
        .await
        .unwrap();
}
async fn recv<T: serde::de::DeserializeOwned>(ws: &mut Ws) -> T {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

fn info(harness: &str) -> WorkerInfo {
    WorkerInfo {
        hostname: "w".into(),
        os: "o".into(),
        arch: "a".into(),
        harnesses: vec![harness.into()],
        roots: vec![],
    }
}

/// Connect a worker and complete the handshake.
async fn join(base: &str, id: WorkerId, harness: &str, running: Vec<TaskId>) -> Ws {
    let (mut ws, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    send(
        &mut ws,
        &WorkerMsg::Hello {
            id,
            info: info(harness),
            running,
        },
    )
    .await;
    let OrchMsg::Welcome { id: got } = recv(&mut ws).await else {
        panic!()
    };
    assert_eq!(got, id);
    ws
}

async fn start() -> String {
    start_with_grace(std::time::Duration::from_millis(50)).await
}

async fn start_with_grace(grace: std::time::Duration) -> String {
    start_app(App::new(None).with_grace(grace)).await
}

async fn start_app(app: App) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mis_orchestrator::serve(listener, app));
    format!("ws://{addr}")
}

/// `/ws/client` upgrade request carrying a browser `Origin` header.
fn client_req_with_origin(
    base: &str,
    origin: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("{base}/ws/client"),
    )
    .unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    req
}

fn allow_mis_example(app: App) -> App {
    app.with_allowed_origins(vec!["https://mis.example.com".into()])
}

#[tokio::test]
async fn submit_assign_finish() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;

    // Task submitted with no workers stays queued.
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else {
        panic!()
    };
    assert!(workers.is_empty());
    assert_eq!(tasks[0].status, TaskStatus::Queued);

    // Worker joins and immediately receives the queued task.
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
    assert_eq!(assigned.harness, "claude");

    // Events stream through to the subscribed client; Finished marks it done.
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Text {
                text: "working".into(),
            },
        },
    )
    .await;
    let ServerMsg::Event {
        worker: w,
        event: AgentEvent::Text { text },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!((w, text.as_str()), (wid, "working"));
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Finished { ok: true, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Done);
    assert_eq!(tasks[0].worker, Some(wid));
    assert!(!workers[0].busy);
}

#[tokio::test]
async fn harness_mismatch_stays_queued() {
    let base = start().await;
    let _worker = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: Some("claude".into()),
        },
    )
    .await;
    let ServerMsg::Submitted { .. } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Queued);
}

#[tokio::test]
async fn worker_disconnect_fails_task_and_notifies() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;

    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    drop(worker);
    let ServerMsg::Event {
        task: t,
        event: AgentEvent::Finished { ok: false, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(t, task);
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Failed);
    assert!(workers.is_empty());
}

#[tokio::test]
async fn events_from_wrong_worker_are_ignored() {
    let base = start().await;
    let w1 = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let w2 = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };

    // Whichever worker got the task, the other one tries to finish it.
    let (mut assignee, mut other) = (w1, w2);
    let assign = tokio::time::timeout(std::time::Duration::from_millis(500), assignee.next()).await;
    if assign.is_err() {
        std::mem::swap(&mut assignee, &mut other);
        let OrchMsg::Assign { .. } = recv(&mut assignee).await else {
            panic!()
        };
    }
    send(
        &mut other,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Running);
}

#[tokio::test]
async fn browser_origin_rejected() {
    let base = start().await;
    let req = client_req_with_origin(&base, "http://evil.example");
    assert!(connect_async(req).await.is_err());
}

#[tokio::test]
async fn allowed_origin_connects() {
    let base = start_app(allow_mis_example(App::new(None))).await;
    let mut client = connect_async(client_req_with_origin(&base, "https://mis.example.com"))
        .await
        .unwrap()
        .0;
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!()
    };
}

#[tokio::test]
async fn unlisted_origin_rejected_with_allowlist() {
    let base = start_app(allow_mis_example(App::new(None))).await;
    let req = client_req_with_origin(&base, "https://evil.example");
    assert!(connect_async(req).await.is_err());
}

#[tokio::test]
async fn allowed_origin_still_needs_token() {
    let base = start_app(allow_mis_example(App::new(Some("s".into())))).await;
    let req = client_req_with_origin(&base, "https://mis.example.com");
    assert!(connect_async(req).await.is_err());
}

#[tokio::test]
async fn reconnect_keeps_reported_task_and_fails_unreported() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "a".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    // Reconnect, still running the task: ledger keeps one busy entry, task stays Running.
    let mut worker2 = join(&base, wid, "claude", vec![task]).await;
    drop(worker); // stale socket closing must not remove the new registration
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(workers.len(), 1);
    assert!(workers[0].busy);
    assert_eq!(tasks[0].status, TaskStatus::Running);

    // Events over the new connection still count.
    send(
        &mut worker2,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Text {
                text: "still here".into(),
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Text { text },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(text, "still here");

    // Reconnect after a restart (nothing running): task is failed and followers told.
    let _worker3 = join(&base, wid, "claude", vec![]).await;
    let ServerMsg::Event {
        task: t,
        event: AgentEvent::Finished { ok: false, summary },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(t, task);
    assert_eq!(summary.as_deref(), Some("worker restarted"));
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(workers.len(), 1);
    assert!(!workers[0].busy);
    assert_eq!(tasks[0].status, TaskStatus::Failed);
}

#[tokio::test]
async fn unknown_running_task_does_not_pin_worker_busy() {
    let base = start().await;
    let _worker = join(
        &base,
        uuid::Uuid::new_v4(),
        "claude",
        vec![uuid::Uuid::new_v4()],
    )
    .await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert!(!workers[0].busy);
}

#[tokio::test]
async fn disconnect_then_reconnect_within_grace_keeps_task() {
    let base = start_with_grace(std::time::Duration::from_secs(5)).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "a".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    worker.close(None).await.unwrap();
    drop(worker);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut worker = join(&base, wid, "claude", vec![task]).await;

    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Finished { ok: true, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };

    // Worker is free again: the next task is assigned to it.
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "b".into(),
            cwd: None,
            harness: None,
        },
    )
    .await;
    let ServerMsg::Submitted { task: next } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, next);
}
