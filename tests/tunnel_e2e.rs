//! End-to-end: a real tunnel client against a real (minimal) WebSocket server.
//!
//! These tests stand up a hub the only way that matters — a WebSocket that
//! behaves like roost's `/agent/ws`: it requires `hello` as the first frame,
//! pushes `request` frames, and drops the socket to force a reconnect. There is
//! no axum, no `goose serve`, and no ACP here: a stub [`QueryAnswerer`] answers
//! `status.get`, and the real `ActivityHub` is the thing being bridged.
//!
//! The hub is `tokio_tungstenite::accept_async` on a bare `TcpListener`, which is
//! deliberately *less* than roost: if the client works against this it works
//! against the real hub, because everything roost adds (registration, the
//! `(bootId, seq)` floor) is on the far side of the wire we assert here.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a_goose::activity::{ActivityEvent, ActivityHub};
use a2a_goose::tunnel::{Answer, Identity, QueryAnswerer, Tunnel};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// Answers exactly what the test expects, so a response body is checkable.
struct Stub;

impl QueryAnswerer for Stub {
    fn answer(&self, method: &str, _params: &Value) -> Answer {
        match method {
            "status.get" => Answer::Body(json!({ "acp": { "inFlight": 7 } })),
            _ => Answer::Unsupported,
        }
    }
}

fn identity() -> Identity {
    Identity {
        agent_id: "a2a-goose-dev".to_string(),
        host: "test-host".to_string(),
        kind: "a2a-goose".to_string(),
        agent_version: "0.1.0".to_string(),
        skills: vec!["ask".to_string()],
        capabilities: vec!["activity".to_string(), "status".to_string()],
    }
}

/// Build a tunnel pointed at `addr`, with a real activity hub and a stub
/// answerer. The hub is returned so the test can `record` onto it.
fn tunnel_for(addr: SocketAddr, answers: Arc<dyn QueryAnswerer>) -> (Tunnel, Arc<ActivityHub>) {
    let activity = Arc::new(ActivityHub::new(true, 32));
    let tunnel = Tunnel::new(
        identity(),
        format!("ws://{addr}/agent/ws"),
        Some("test-credential".to_string()),
        Arc::clone(&activity),
        answers,
    );
    (tunnel, activity)
}

/// Wait until `predicate` holds, or fail after `timeout`.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// Read frames until a text frame arrives, or the socket closes.
async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Option<Value> {
    while let Some(message) = socket.next().await {
        match message.ok()? {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
    None
}

type SharedFrames = Arc<Mutex<Vec<Value>>>;

/// A hub that accepts exactly one tunnel: reads the `hello`, asks `status.get`,
/// then records every frame it receives.
async fn start_one_shot_hub() -> (SocketAddr, JoinHandle<()>, SharedFrames) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let frames: SharedFrames = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&frames);
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        if let Some(hello) = next_text(&mut socket).await {
            recorded.lock().unwrap().push(hello);
        }
        // The hub's status poll, as roost sends it.
        let request =
            json!({ "type": "request", "id": "r-1", "method": "status.get", "params": {} });
        let _ = socket.send(Message::Text(request.to_string().into())).await;
        while let Some(frame) = next_text(&mut socket).await {
            recorded.lock().unwrap().push(frame);
        }
    });
    (addr, task, frames)
}

/// A hub that accepts tunnels forever and reads each one's `hello`.
async fn start_reconnect_hub() -> (SocketAddr, JoinHandle<()>, SharedFrames) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hellos: SharedFrames = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&hellos);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                if let Some(hello) = next_text(&mut socket).await {
                    recorded.lock().unwrap().push(hello);
                }
                // Drop the socket: a dropped tunnel, which must reconnect.
            });
        }
    });
    (addr, task, hellos)
}

fn find<'a>(frames: &'a [Value], type_tag: &str, id: Option<&str>) -> Option<&'a Value> {
    frames
        .iter()
        .find(|frame| frame["type"] == type_tag && id.is_none_or(|id| frame["id"] == id))
}

#[tokio::test]
async fn hello_activity_and_a_query_all_arrive_in_order() {
    let (addr, hub_task, frames) = start_one_shot_hub().await;
    let (tunnel, activity) = tunnel_for(addr, Arc::new(Stub));
    let boot_id = tunnel.boot_id().to_string();
    let client = tokio::spawn(tunnel.run());

    // (a) `hello` is the first frame, and carries the identity we built.
    let seen = Arc::clone(&frames);
    assert!(
        wait_for(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await,
        "the hub should have received hello"
    );
    let hello = frames.lock().unwrap()[0].clone();
    assert_eq!(hello["type"], "hello", "hello must be the first frame");
    assert_eq!(hello["agentId"], "a2a-goose-dev");
    assert_eq!(hello["host"], "test-host");
    assert_eq!(hello["kind"], "a2a-goose");
    assert_eq!(hello["protocolVersion"], 1);
    assert_eq!(hello["bootId"], boot_id);

    // (b) an activity recorded on the hub arrives as an opaque enveloped frame.
    activity.record(
        Some("ctx-1"),
        Some("task-1"),
        None,
        Some("ask"),
        ActivityEvent::ToolCall {
            id: "call_1".to_string(),
            title: Some("Read src/lib.rs".to_string()),
            tool_kind: Some("read".to_string()),
            status: Some("in_progress".to_string()),
        },
    );
    let seen = Arc::clone(&frames);
    assert!(
        wait_for(Duration::from_secs(5), || {
            find(&seen.lock().unwrap(), "activity", None).is_some()
        })
        .await,
        "the activity should have been published"
    );
    let activity_frame = find(&frames.lock().unwrap(), "activity", None)
        .unwrap()
        .clone();
    assert_eq!(activity_frame["bootId"], boot_id);
    assert_eq!(activity_frame["seq"], 0, "the feed's first seq is zero");
    assert_eq!(activity_frame["contextId"], "ctx-1");
    assert_eq!(activity_frame["taskId"], "task-1");
    assert_eq!(activity_frame["skill"], "ask");
    assert_eq!(activity_frame["event"]["type"], "tool_call");
    assert_eq!(activity_frame["event"]["title"], "Read src/lib.rs");

    // (c) the hub's `status.get` request is answered with the stub's body.
    let seen = Arc::clone(&frames);
    assert!(
        wait_for(Duration::from_secs(5), || {
            find(&seen.lock().unwrap(), "response", Some("r-1")).is_some()
        })
        .await,
        "the request should have been answered"
    );
    let response = find(&frames.lock().unwrap(), "response", Some("r-1"))
        .unwrap()
        .clone();
    assert_eq!(response["ok"], true);
    assert_eq!(response["body"]["acp"]["inFlight"], 7);

    client.abort();
    hub_task.abort();
}

#[tokio::test]
async fn a_dropped_tunnel_reconnects_with_the_same_boot_id() {
    let (addr, hub_task, hellos) = start_reconnect_hub().await;
    let (tunnel, _activity) = tunnel_for(addr, Arc::new(Stub));
    let first_boot = tunnel.boot_id().to_string();
    let client = tokio::spawn(tunnel.run());

    // The hub drops the first tunnel immediately; the client must dial back.
    let seen = Arc::clone(&hellos);
    assert!(
        wait_for(Duration::from_secs(10), || seen.lock().unwrap().len() >= 2).await,
        "the client should have reconnected after the drop"
    );
    let hellos = hellos.lock().unwrap();
    assert_eq!(hellos[0]["bootId"], first_boot);
    assert_eq!(
        hellos[1]["bootId"], first_boot,
        "a reconnect is the same boot, not a new one"
    );

    client.abort();
    hub_task.abort();
}

#[tokio::test]
async fn an_unreachable_hub_is_a_retry_and_never_a_crash() {
    // Nothing is listening on this address: the tunnel must keep retrying and
    // the task must stay alive (fail open).
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let (tunnel, _activity) = tunnel_for(addr, Arc::new(Stub));
    let client = tokio::spawn(tunnel.run());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !client.is_finished(),
        "an unreachable hub must not stop the tunnel task"
    );
    client.abort();
}
