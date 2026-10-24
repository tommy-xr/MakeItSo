//! Fake worker + client drive the orchestrator end to end over real WebSockets.

use futures_util::{SinkExt, StreamExt};
use mis_orchestrator::App;
use mis_types::*;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn send<T: serde::Serialize>(ws: &mut Ws, v: &T) {
    ws.send(Message::Text(serde_json::to_string(v).unwrap().into())).await.unwrap();
}
async fn recv<T: serde::de::DeserializeOwned>(ws: &mut Ws) -> T {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await.expect("timeout").unwrap().unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

async fn start() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mis_orchestrator::serve(listener, App::new(None)));
    format!("ws://{addr}")
}

#[tokio::test]
async fn submit_assign_finish() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;

    // Task submitted with no workers stays queued.
    send(&mut client, &ClientMsg::Submit { prompt: "hi".into(), cwd: None, harness: None }).await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else { panic!() };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else { panic!() };
    assert!(workers.is_empty());
    assert_eq!(tasks[0].status, TaskStatus::Queued);

    // Worker joins and immediately receives the queued task.
    let (mut worker, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let info = WorkerInfo { hostname: "w".into(), os: "o".into(), arch: "a".into(), harnesses: vec!["claude".into()] };
    send(&mut worker, &WorkerMsg::Hello { info }).await;
    let OrchMsg::Welcome { id: wid } = recv(&mut worker).await else { panic!() };
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else { panic!() };
    assert_eq!(assigned.id, task);
    assert_eq!(assigned.harness, "claude");

    // Events stream through to the subscribed client; Finished marks it done.
    send(&mut worker, &WorkerMsg::Event { task, event: AgentEvent::Text { text: "working".into() } }).await;
    let ServerMsg::Event { worker: w, event: AgentEvent::Text { text }, .. } = recv(&mut client).await else { panic!() };
    assert_eq!((w, text.as_str()), (wid, "working"));
    send(&mut worker, &WorkerMsg::Event { task, event: AgentEvent::Finished { ok: true, summary: None } }).await;
    let ServerMsg::Event { event: AgentEvent::Finished { ok: true, .. }, .. } = recv(&mut client).await else { panic!() };

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers } = recv(&mut client).await else { panic!() };
    assert_eq!(tasks[0].status, TaskStatus::Done);
    assert_eq!(tasks[0].worker, Some(wid));
    assert!(!workers[0].busy);
}

#[tokio::test]
async fn harness_mismatch_stays_queued() {
    let base = start().await;
    let (mut worker, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let info = WorkerInfo { hostname: "w".into(), os: "o".into(), arch: "a".into(), harnesses: vec!["codex".into()] };
    send(&mut worker, &WorkerMsg::Hello { info }).await;
    let OrchMsg::Welcome { .. } = recv(&mut worker).await else { panic!() };

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Submit { prompt: "hi".into(), cwd: None, harness: Some("claude".into()) }).await;
    let ServerMsg::Submitted { .. } = recv(&mut client).await else { panic!() };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else { panic!() };
    assert_eq!(tasks[0].status, TaskStatus::Queued);
}
