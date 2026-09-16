//! The reuse policy, against a fake `goose serve`.
//!
//! [`a2a_goose::acp::pool`] proves the *rules* on their own — what a pool does
//! with a TTL, a cap, a busy context. What it cannot prove is the wiring, and the
//! wiring is where this feature is either real or not: that a caller's second
//! turn in the same context is answered on the *same goose session*, that a turn
//! which finds the context busy is not handed it anyway, and that a connection
//! that dies takes its sessions with it rather than leaving them to be handed
//! out again.
//!
//! It needs an ACP server to hold that conversation with, and CI has no goose.
//! So the server here is fake, and it is fake in the way that matters: HTTP POST
//! for requests, two SSE stream scopes, the `Acp-Connection-Id` header, replies
//! pushed on a stream rather than returned in the POST body — the four things S3
//! corrected the plan about. It counts what it was asked to do, which is exactly
//! the evidence the tests need and a real goose would not give up.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use a2a_goose::acp::{ACP_PATH, AcpTurns, CONNECTION_ID_HEADER, SESSION_ID_HEADER};
use a2a_goose::config::Config;
use a2a_goose::turn::{TurnError, TurnEvent, TurnRequest, Turns};

/// What the fake goose was asked to do. The counts are the assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    Initialize,
    New { session: String },
    Prompt { session: String, text: String },
    Close { session: String },
}

#[derive(Default)]
struct Fake {
    /// The connection-level SSE stream. One at a time: a reconnect replaces it,
    /// which is what a real connection being replaced would do.
    connection: Mutex<Option<mpsc::Sender<Event>>>,
    /// One SSE stream per session, opened by the client *before* its first
    /// session-scoped request (S3's ordering rule).
    sessions: Mutex<HashMap<String, mpsc::Sender<Event>>>,
    records: Mutex<Vec<Record>>,
    counter: AtomicUsize,
    /// How many times the client has `initialize`d. The count is how a test
    /// tells "the connection was kept" from "the connection was dropped",
    /// which is otherwise invisible from the outside.
    initializes: AtomicUsize,
    /// Every POST fails with no response at all — not an error status, which is
    /// a goose that is answering, but a transport that is gone. Only a panic can
    /// produce that from inside an HTTP framework, and it is the honest way to
    /// simulate a dead socket.
    hostile: AtomicBool,
    /// `session/prompt` answers 500: goose is there and refusing.
    fail_prompts: AtomicBool,
    /// Hold the next prompt open until `release`, so a second turn can arrive
    /// while the first is still running.
    hold_next: AtomicBool,
    prompt_seen: Notify,
    release: Notify,
}

impl Fake {
    fn record(&self, record: Record) {
        self.records.lock().expect("records").push(record);
    }

    fn records(&self) -> Vec<Record> {
        self.records.lock().expect("records").clone()
    }

    /// The session ids handed out, in order.
    fn created(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::New { session } => Some(session),
                _ => None,
            })
            .collect()
    }

    /// Every prompt, as (session, text).
    fn prompts(&self) -> Vec<(String, String)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Prompt { session, text } => Some((session, text)),
                _ => None,
            })
            .collect()
    }

    fn closes(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Close { session } => Some(session),
                _ => None,
            })
            .collect()
    }

    fn initializes(&self) -> usize {
        self.initializes.load(Ordering::SeqCst)
    }

    /// Pushes a frame onto the connection-level stream.
    async fn to_connection(&self, frame: Value) {
        let sender = self.connection.lock().expect("connection").clone();
        if let Some(sender) = sender {
            let _ = sender.send(event(frame)).await;
        }
    }

    /// Pushes a frame onto a session's stream.
    async fn to_session(&self, session: &str, frame: Value) {
        let sender = {
            let sessions = self.sessions.lock().expect("sessions");
            sessions.get(session).cloned()
        };
        if let Some(sender) = sender {
            let _ = sender.send(event(frame)).await;
        }
    }
}

fn event(frame: Value) -> Event {
    Event::default().data(frame.to_string())
}

/// Opens an SSE response over a fresh channel, storing the sender where the
/// right scope will find it.
fn stream(fake: &Fake, scope: Option<&str>) -> Response {
    let (sender, receiver) = mpsc::channel::<Event>(64);
    match scope {
        Some(session) => {
            fake.sessions
                .lock()
                .expect("sessions")
                .insert(session.to_string(), sender);
        }
        None => *fake.connection.lock().expect("connection") = Some(sender),
    }
    let body = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|event| (Ok::<_, Infallible>(event), receiver))
    });
    Sse::new(body).into_response()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The two halves of ACP's transport: a GET opens a stream, a POST sends a
/// request. Which stream a GET opens is decided by the session header, and a
/// POST's reply is never its body.
async fn acp_stream(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    stream(&fake, header(&headers, SESSION_ID_HEADER))
}

async fn acp_post(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    // Before anything is locked, so a panicking handler cannot poison the fake.
    if fake.hostile.load(Ordering::SeqCst) {
        panic!("the fake goose is gone: no response, no connection");
    }

    let Ok(frame) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let session = header(&headers, SESSION_ID_HEADER).map(str::to_string);

    match method {
        "initialize" => {
            fake.initializes.fetch_add(1, Ordering::SeqCst);
            fake.record(Record::Initialize);
            let mut response = StatusCode::OK.into_response();
            response.headers_mut().insert(
                HeaderName::from_bytes(CONNECTION_ID_HEADER.as_bytes()).expect("a header name"),
                HeaderValue::from_static("conn_fake"),
            );
            response
        }
        "session/new" => {
            let session = format!("sess_{}", fake.counter.fetch_add(1, Ordering::SeqCst));
            fake.record(Record::New {
                session: session.clone(),
            });
            fake.to_connection(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "sessionId": session,
                    "modes": { "currentModeId": "auto", "availableModes": [] },
                },
            }))
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        "session/prompt" => {
            let session = session.unwrap_or_default();
            let text = frame
                .pointer("/params/prompt/0/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            fake.record(Record::Prompt {
                session: session.clone(),
                text,
            });
            fake.prompt_seen.notify_one();

            if fake.fail_prompts.load(Ordering::SeqCst) {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            if fake.hold_next.swap(false, Ordering::SeqCst) {
                fake.release.notified().await;
            }

            fake.to_session(
                &session,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "ok" },
                        },
                    },
                }),
            )
            .await;
            fake.to_session(
                &session,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "stopReason": "end_turn",
                        "usage": { "totalTokens": 7, "inputTokens": 5, "outputTokens": 2 },
                    },
                }),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        "session/close" => {
            let session = session.unwrap_or_default();
            fake.record(Record::Close {
                session: session.clone(),
            });
            fake.to_session(
                &session,
                json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            )
            .await;
            StatusCode::ACCEPTED.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A fake goose, and the turns that talk to it.
struct World {
    fake: Arc<Fake>,
    turns: Arc<AcpTurns>,
    _server: tokio::task::JoinHandle<()>,
}

impl World {
    async fn start(tune: impl FnOnce(&mut Config)) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        let url = format!("http://{}", listener.local_addr().expect("a local address"));
        let fake = Arc::new(Fake::default());

        let app = Router::new()
            .route(ACP_PATH, get(acp_stream).post(acp_post))
            .with_state(Arc::clone(&fake));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut config = Config::default();
        config.goose.acp.url = url;
        // Small, so a regression fails a test rather than hanging it.
        config.goose.acp.timeouts.initialize_secs = 5;
        config.goose.acp.timeouts.prompt_secs = 20;
        config.goose.sessions.reuse_by_context = true;
        config.goose.sessions.idle_ttl_secs = 3600;
        config.goose.sessions.max_sessions = 16;
        config.registry.limits.max_concurrent_sessions = 4;
        config.registry.limits.max_wall_clock_seconds_per_task = 30;
        tune(&mut config);

        Self {
            fake,
            turns: Arc::new(AcpTurns::new(Arc::new(config))),
            _server: server,
        }
    }

    /// Runs one whole turn and returns what it produced.
    async fn run(&self, context: Option<&str>, prompt: &str) -> Result<Vec<TurnEvent>, TurnError> {
        let request = TurnRequest {
            context: context.map(str::to_string),
            cwd: PathBuf::from("/tmp"),
            prompt: prompt.to_string(),
            wall_clock: Duration::from_secs(20),
        };
        let mut events = Vec::new();
        let mut stream = self.turns.run(request);
        while let Some(item) = stream.next().await {
            events.push(item?);
        }
        Ok(events)
    }

    async fn turn(&self, context: Option<&str>) -> Result<Vec<TurnEvent>, TurnError> {
        self.run(context, "say ok").await
    }

    fn retained(&self) -> usize {
        self.turns.retained()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_context_keeps_its_session_between_turns() {
    // The feature, in one test: the second turn of a context is the first turn's
    // conversation, not a stranger with the same prompt.
    let world = World::start(|_| {}).await;

    let first = world.turn(Some("c1")).await.expect("the first turn runs");
    assert!(
        matches!(first.last(), Some(TurnEvent::Finished { stop_reason, .. }) if stop_reason == "end_turn"),
        "a turn still ends the way M1 ended it: {first:?}"
    );
    world.turn(Some("c1")).await.expect("the second turn runs");

    let prompts = world.fake.prompts();
    assert_eq!(prompts.len(), 2, "both turns reached goose");
    assert_eq!(
        prompts[0].0, prompts[1].0,
        "the same context must be answered on the same session"
    );
    assert_eq!(
        world.fake.created(),
        vec![prompts[0].0.clone()],
        "and it must be the only session either turn opened"
    );
    assert_eq!(
        world.retained(),
        1,
        "the session is still held for a third turn"
    );
    assert!(
        world.fake.closes().is_empty(),
        "a reused session is not closed between turns: {:?}",
        world.fake.closes()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_with_no_context_remembers_nothing() {
    // M1's behaviour, kept for callers that send no `contextId`: a fresh session
    // per turn, closed when the turn ends, and nothing retained. Reuse is opt-in
    // by the caller naming a context, and this is what "not opting in" costs.
    let world = World::start(|_| {}).await;

    world.turn(None).await.expect("first");
    world.turn(None).await.expect("second");

    assert_eq!(world.fake.created().len(), 2, "one session per turn");
    assert_eq!(world.fake.closes().len(), 2, "and each is closed behind it");
    assert_eq!(world.retained(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_contexts_get_two_sessions_and_keep_them_both() {
    let world = World::start(|_| {}).await;

    world.turn(Some("c1")).await.expect("c1");
    world.turn(Some("c2")).await.expect("c2");

    let prompts = world.fake.prompts();
    assert_eq!(prompts.len(), 2);
    assert_ne!(
        prompts[0].0, prompts[1].0,
        "two contexts are two conversations, not one shared session"
    );
    assert_eq!(world.retained(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_that_finds_its_context_busy_runs_alone_and_gives_up_its_session() {
    // The busy rule, end to end. A second turn for a context whose first turn is
    // still running must not be handed the same session — two interleaved prompts
    // on one session are a race, not a conversation — and must not overwrite the
    // first turn's memory when it finishes.
    let world = World::start(|config| {
        config.goose.acp.timeouts.prompt_secs = 60;
    })
    .await;

    world.fake.hold_next.store(true, Ordering::SeqCst);
    let first = tokio::spawn({
        let turns = Arc::clone(&world.turns);
        async move {
            let request = TurnRequest {
                context: Some("c1".to_string()),
                cwd: PathBuf::from("/tmp"),
                prompt: "the long one".to_string(),
                wall_clock: Duration::from_secs(60),
            };
            turns.run(request).collect::<Vec<_>>().await
        }
    });

    // Wait until the first turn is actually inside goose, so "concurrent" means
    // concurrent rather than "one after the other by luck".
    world.fake.prompt_seen.notified().await;
    world.turn(Some("c1")).await.expect("the second turn runs");

    world.fake.release.notify_one();
    let first = first.await.expect("the first turn finishes");
    assert!(
        first.iter().all(Result::is_ok),
        "the held turn must still complete: {first:?}"
    );

    let prompts = world.fake.prompts();
    assert_eq!(prompts.len(), 2);
    assert_ne!(
        prompts[0].0, prompts[1].0,
        "the second turn got its own session"
    );
    assert_eq!(
        world.fake.closes(),
        vec![prompts[1].0.clone()],
        "the throwaway session is closed, and the first turn's is kept"
    );
    assert_eq!(
        world.retained(),
        1,
        "the context still holds the session of the turn that owned it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_cap_closes_the_session_it_evicts() {
    // Eviction is "closed", not "forgotten": a session this host gives up is one
    // goose would otherwise keep for a conversation nobody will continue.
    let world = World::start(|config| {
        config.goose.sessions.max_sessions = 1;
    })
    .await;

    world.turn(Some("c1")).await.expect("c1");
    world.turn(Some("c2")).await.expect("c2");

    let created = world.fake.created();
    assert_eq!(created.len(), 2);
    assert_eq!(
        world.fake.closes(),
        vec![created[0].clone()],
        "the cap of one closed the session it evicted"
    );
    assert_eq!(world.retained(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_past_its_idle_ttl_is_closed_rather_than_reused() {
    let world = World::start(|config| {
        config.goose.sessions.idle_ttl_secs = 1;
    })
    .await;

    world.turn(Some("c1")).await.expect("c1");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    world.turn(Some("c1")).await.expect("c1 again");

    let created = world.fake.created();
    assert_eq!(created.len(), 2, "the expired session is not reused");
    assert_eq!(
        world.fake.closes(),
        vec![created[0].clone()],
        "it is closed on the turn that notices it has expired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_connection_takes_its_sessions_with_it() {
    // Rule 3. The connection is not repaired in place and the sessions are not
    // kept: the next turn reconnects — provable by counting `initialize`s — and
    // starts the context over on a session it opens itself.
    let world = World::start(|_| {}).await;

    world.turn(Some("c1")).await.expect("the connection works");
    assert_eq!(world.retained(), 1);
    assert_eq!(world.fake.initializes(), 1);

    world.fake.hostile.store(true, Ordering::SeqCst);
    let failed = world.turn(Some("c1")).await;
    assert!(failed.is_err(), "a turn on a dead connection must fail");
    assert_eq!(
        world.retained(),
        0,
        "and the session it was reusing must not be left for the next turn"
    );

    world.fake.hostile.store(false, Ordering::SeqCst);
    world
        .turn(Some("c1"))
        .await
        .expect("the turn after it works again");
    assert_eq!(
        world.fake.initializes(),
        2,
        "a dropped connection is replaced, not treated as live"
    );
    assert_eq!(
        world.fake.created().len(),
        2,
        "and the context starts a session it opens itself"
    );
    assert_eq!(world.retained(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goose_that_answers_with_an_error_is_not_a_dead_connection() {
    // The other half of the classification: an error *status* proves something
    // answered. Dropping the connection here would discard every other context's
    // session for a failure that says nothing about them.
    let world = World::start(|_| {}).await;

    world.turn(Some("c1")).await.expect("the connection works");
    world.fake.fail_prompts.store(true, Ordering::SeqCst);
    assert!(
        world.turn(Some("c1")).await.is_err(),
        "goose refusing a prompt is still a failed turn"
    );

    world.fake.fail_prompts.store(false, Ordering::SeqCst);
    world.turn(Some("c1")).await.expect("the next turn works");

    assert_eq!(
        world.fake.initializes(),
        1,
        "the connection was kept: an error status is not a lost connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_reuse_off_gives_back_exactly_m1() {
    let world = World::start(|config| {
        config.goose.sessions.reuse_by_context = false;
    })
    .await;

    world.turn(Some("c1")).await.expect("first");
    world.turn(Some("c1")).await.expect("second");

    assert_eq!(
        world.fake.created().len(),
        2,
        "every turn opens its own session"
    );
    assert_eq!(world.fake.closes().len(), 2, "and closes it");
    assert_eq!(world.retained(), 0);
}
