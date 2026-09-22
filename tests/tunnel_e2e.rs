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

/// A hub that accepts tunnels forever, reads each `hello`, then goes **silent**:
/// it holds the socket open without ever sending a frame. This is the shape of a
/// half-open peer as seen from the client — no FIN, no RST, just no data — so
/// the only thing that can notice it is the idle deadline.
async fn start_silent_hub() -> (SocketAddr, JoinHandle<()>, SharedFrames) {
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
                // Read (and discard) whatever the client sends, but never write:
                // the socket stays open and quiet, exactly like a peer that has
                // vanished from the network's point of view.
                while socket.next().await.is_some() {}
            });
        }
    });
    (addr, task, hellos)
}

/// A hub that accepts one tunnel, sends two heartbeat `ping`s (one with an id,
/// one bare), and records the `pong`s it gets back.
async fn start_pinging_hub() -> (SocketAddr, JoinHandle<()>, SharedFrames) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let pongs: SharedFrames = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&pongs);
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        if next_text(&mut socket).await.is_none() {
            return;
        }
        let _ = socket
            .send(Message::Text(
                json!({ "type": "ping", "id": "hb-1" }).to_string().into(),
            ))
            .await;
        let _ = socket
            .send(Message::Text(json!({ "type": "ping" }).to_string().into()))
            .await;
        while let Some(frame) = next_text(&mut socket).await {
            recorded.lock().unwrap().push(frame);
            let seen = recorded.lock().unwrap();
            if seen.iter().filter(|f| f["type"] == "pong").count() >= 2 {
                return;
            }
        }
    });
    (addr, task, pongs)
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

/// The failure this fix exists for: a hub that completes the handshake and then
/// never speaks again must be noticed in bounded time, not parked on forever.
///
/// The server here accepts the WebSocket and holds it open without a frame, so
/// no FIN/RST arrives — the same half-open shape as a killed container or a
/// dropped NAT path. With a short idle window the client must drop and redial on
/// its own; a second `hello` is the proof.
#[tokio::test]
async fn a_silent_hub_is_dropped_and_retried_within_the_idle_window() {
    let (addr, hub_task, hellos) = start_silent_hub().await;
    let (tunnel, _activity) = tunnel_for(addr, Arc::new(Stub));
    // A 400 ms window instead of the 90 s default: the assertion is about the
    // mechanism, not the number.
    let tunnel = tunnel.with_idle_timeout(Duration::from_millis(400));
    let client = tokio::spawn(tunnel.run());

    let seen = Arc::clone(&hellos);
    assert!(
        wait_for(Duration::from_secs(5), || seen.lock().unwrap().len() >= 2).await,
        "a silent hub must be dropped and retried within the idle window"
    );
    assert!(
        !client.is_finished(),
        "a half-open socket must be a retry, never a crash"
    );

    client.abort();
    hub_task.abort();
}

/// A `ping` must be answered with a `pong` at once, from the read loop itself —
/// not through the answerer or the activity feed, which may both be busy with a
/// turn. No turn is running here, and the activity hub is empty: the only reason
/// a `pong` can come back is that the tunnel answers the heartbeat directly.
#[tokio::test]
async fn a_ping_is_answered_with_a_pong_while_the_tunnel_is_idle() {
    let (addr, hub_task, pongs) = start_pinging_hub().await;
    let (tunnel, _activity) = tunnel_for(addr, Arc::new(Stub));
    let client = tokio::spawn(tunnel.run());

    let seen = Arc::clone(&pongs);
    assert!(
        wait_for(Duration::from_secs(5), || {
            let seen = seen.lock().unwrap();
            let echoed = seen
                .iter()
                .any(|f| f["type"] == "pong" && f["id"] == "hb-1");
            let bare = seen
                .iter()
                .any(|f| f["type"] == "pong" && f.get("id").is_none());
            echoed && bare
        })
        .await,
        "both pings must be answered: one echoes its id, one stays bare"
    );

    client.abort();
    hub_task.abort();
}
