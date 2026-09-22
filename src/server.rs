//! The HTTP surface: the card LiteLLM fetches, the A2A JSON-RPC endpoint, and
//! the control routes.
//!
//! | Route | Purpose |
//! |---|---|
//! | `/.well-known/agent-card.json` | The card LiteLLM fetches at registration |
//! | `POST /` | A2A JSON-RPC (router from `a2a-server`) |
//! | `GET /healthz` | Liveness only — no dependency checks |
//! | `GET /status` | Deep: registry, skills, card hash, limits |
//! | `GET /sessions` | The sessions held for reuse (§6.6) — **bearer** |
//! | `DELETE /sessions/{contextId}` | Close and forget one — **bearer** |
//!
//! `/healthz` and `/status` are deliberately different questions. `/healthz`
//! answers *"should the supervisor restart me?"*, so it stays 200 when something
//! **else** is down: a dead LiteLLM, or a `goose serve` that is restarting, is
//! not a reason for launchd to bounce this process. `/status` answers *"is
//! anything wrong?"* for a human, and for a hang probe — which is why it is not
//! behind the bearer token. It carries no secret: no key, no token, no recipe
//! content, and goose's path is a fact the tailnet already implies.
//!
//! **The session routes are the exception, and sit behind the token with
//! `POST /`.** `GET /sessions` names the directories other callers are working
//! in, and `DELETE /sessions/{contextId}` ends a conversation's memory: neither
//! is a liveness question, and whoever asks either already holds the token.
//!
//! **Bearer auth is a layer, not a handler.** Constraint #3 requires a token on
//! every `POST /`, and §5.4 wants a **401** when it is wrong. The SDK's
//! `RequestAuthorizer` hook would give neither: its error goes back as a
//! JSON-RPC error inside a **200**, because that is what the transport does with
//! handler errors. A `tower` layer can return whatever status it likes, so the
//! token check lives there.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::AgentCard;
use a2a_server::agent_card::{StaticAgentCard, agent_card_router};
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::task_store::InMemoryTaskStore;
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::activity::{Activity, ActivityHub};
use crate::config::Config;
use crate::executor::{ADVERTISED_METHODS, GooseExecutor};
use crate::goose::{Goose, MIN_GOOSE_VERSION};
use crate::registry::Registry;
use crate::serve::ServeStatus;
use crate::skills::{Dispatch, SkillSet};
use crate::turn::{SessionClose, TurnHealth, Turns};

/// Everything a handler needs. Shared, and never mutated: the mutable state is
/// inside `registry`, inside the SDK's task store, and behind `turns`.
pub struct Agent {
    pub config: Config,
    pub skills: Arc<SkillSet>,
    pub card: AgentCard,
    pub card_hash: String,
    pub goose: Goose,
    pub registry: Registry,
    /// A read-only window onto goose's own `sessions.db`, which answers the
    /// hub's `history.*` queries (§[`crate::history`]). The conversation record
    /// is goose's, not this process's; this only maps it onto the wire shape.
    pub history: crate::history::HistoryStore,
    /// How a turn is actually run. Held here rather than built inside
    /// [`router`] so that `/status` can report on the connection the turns use,
    /// and so an integration test can substitute a fake and pin the A2A wire
    /// without a `goose serve` running.
    pub turns: Arc<dyn Turns>,
    /// The `goose serve` this process started and supervises, when
    /// `goose.acp.serve` is `own`. `None` means the host starts goose itself,
    /// which `/status` says out loud rather than leaving a reader to infer it
    /// from a missing field.
    pub serve: Option<Arc<ServeStatus>>,
    /// The activity feed `GET /events` streams (§`crate::activity`). Held here so
    /// the route and the writers share one instance: the executor and the ACP
    /// runner are handed clones of *this* hub at construction (`crate::main`).
    pub activity: Arc<ActivityHub>,
    pub started: Instant,
}

/// Builds the router. The token is passed separately from [`Agent`] so it cannot
/// be reached from a handler that only wants the configuration.
pub fn router(agent: Arc<Agent>, bearer_token: Arc<str>) -> Router {
    // The card endpoint is public discovery: LiteLLM fetches it before it has
    // any credential of ours, and the sweeper probes it to decide whether a
    // registry entry is still real.
    let card = agent_card_router(Arc::new(StaticAgentCard::new(agent.card.clone())));

    let handler = Arc::new(DefaultRequestHandler::new(
        // The executor is where a request is first understood, so it is one of
        // the two places that writes to the activity feed; the other is the ACP
        // runner, which `agent.turns` already carries a handle to.
        GooseExecutor::new(
            agent.skills.clone(),
            Arc::new(agent.config.clone()),
            agent.turns.clone(),
        )
        .with_activity(Arc::clone(&agent.activity)),
        InMemoryTaskStore::new(),
    ));
    let a2a = jsonrpc_router(handler).layer(middleware::from_fn_with_state(
        bearer_token.clone(),
        require_bearer,
    ));

    let control = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .with_state(agent.clone());

    // The session control surface (§6.6), and the one part of it that is
    // **behind the token**.
    //
    // `/status` is open because it carries no secret and is what a human — or a
    // hang probe — asks when something is already wrong. These two are not the
    // same question: `GET /sessions` names every context this host is holding
    // and the *directory* each one is rooted in, which is a map of what other
    // callers are working on, and `DELETE` ends somebody's conversation memory.
    // A caller doing either already has the token; nobody else has a reason to.
    let sessions = Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/{context_id}", delete(close_session))
        // The live activity feed carries working directories and answer sizes
        // (and, for a debugging tool, is *meant* to be followed), so it sits
        // behind the token with the session routes rather than in the open
        // control group with `/status`.
        .route("/events", get(events))
        .layer(middleware::from_fn_with_state(bearer_token, require_bearer))
        .with_state(agent);

    card.merge(a2a).merge(control).merge(sessions)
}

/// Serves until the process is asked to stop.
pub async fn serve(
    listener: tokio::net::TcpListener,
    agent: Arc<Agent>,
    bearer_token: Arc<str>,
) -> std::io::Result<()> {
    axum::serve(listener, router(agent, bearer_token)).await
}

/// How long open HTTP connections get to finish after a shutdown is requested.
///
/// Not a grace period for the *caller*: it is the bound that keeps a stream that
/// is designed never to end from holding the whole process open. See
/// [`serve_until`].
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Serves `listener` until `shutdown` resolves, then gives still-open
/// connections `grace` to finish before returning anyway.
///
/// This replaces a bare `axum::serve(..).with_graceful_shutdown(..)`, which
/// waits for **every** open connection to end before its future resolves.
/// `GET /events` is an SSE stream that is *meant* to never end, and an in-flight
/// A2A turn can stream for a long time, so "wait for every connection" is
/// effectively "wait forever" whenever a client is subscribed. The failure that
/// produced this function: on `SIGTERM` the host closed its `:10001` listener
/// (so `/healthz` went to 000) but never exited — the shutdown steps *after* the
/// server future, which stop the `goose serve` child and deregister, never ran,
/// and the process sat parked until the SSE client went away. A bounded drain is
/// what keeps the host supervisable.
///
/// The grace begins when `shutdown` resolves, not when this function is called:
/// the server runs unbounded until then.
pub async fn serve_until(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> std::io::Result<()> {
    // A `watch` rather than a `Notify`: the value is stored, so a waiter that
    // registers *after* the signal still observes it. `Notify::notify_waiters`
    // wakes only waiters already registered, and that lost wake-up is exactly
    // the race this function exists to remove.
    let (stopping, stop) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stopping.send(true);
    });

    let serving = axum::serve(listener, router).with_graceful_shutdown(wait_for_stop(stop.clone()));

    let deadline = async move {
        wait_for_stop(stop).await;
        tokio::time::sleep(grace).await;
    };

    tokio::select! {
        result = serving => result,
        _ = deadline => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "HTTP connections did not drain after shutdown; stopping anyway"
            );
            Ok(())
        }
    }
}

/// Resolves once `stop` holds `true`, including if it already did.
async fn wait_for_stop(mut stop: tokio::sync::watch::Receiver<bool>) {
    while !*stop.borrow() {
        if stop.changed().await.is_err() {
            // The sender went away without signalling. Treat that as a stop:
            // a dropped task must not be able to turn into a hang.
            return;
        }
    }
}

/// Liveness only, deliberately (§6.6).
async fn healthz() -> &'static str {
    "ok"
}

/// Deep status. See the module comment for why this is not authenticated.
async fn status(State(agent): State<Arc<Agent>>) -> Json<Value> {
    Json(status_payload(&agent))
}

pub fn status_payload(agent: &Agent) -> Value {
    let skills: Vec<Value> = agent
        .skills
        .iter()
        .map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                // The *kind* of dispatch, never its content (constraint #12).
                "dispatch": match &skill.dispatch {
                    Dispatch::Ask => "ask",
                    Dispatch::Recipe(_) => "recipe",
                    Dispatch::Instruction(_) => "instruction",
                },
                "default": skill.id == agent.skills.default_id(),
            })
        })
        .collect();

    json!({
        "status": "ok",
        "uptimeSecs": agent.started.elapsed().as_secs(),
        "goose": {
            "path": agent.goose.path.display().to_string(),
            "version": agent.goose.version.to_string(),
            "minVersion": MIN_GOOSE_VERSION.to_string(),
        },
        "card": {
            "name": agent.card.name,
            "version": agent.card.version,
            "protocolVersion": agent
                .card
                .supported_interfaces
                .first()
                .map(|interface| interface.protocol_version.clone())
                .unwrap_or_default(),
            "url": agent
                .card
                .supported_interfaces
                .first()
                .map(|interface| interface.url.clone())
                .unwrap_or_default(),
            // The whole point of hashing: this is what §6.2 compares to decide
            // whether the registry needs to converge.
            "hash": agent.card_hash,
        },
        "skills": skills,
        "methods": ADVERTISED_METHODS,
        "limits": agent.config.registry.limits,
        "attribution": agent.config.registry.attribution,
        "acp": {
            // `connected` and `idle` are both healthy: the connection is made on
            // the first turn, so a host that has served nothing yet is not a
            // host with a problem. `unconfigured` is the one that is a problem,
            // and it is reported rather than omitted so a reader can tell it
            // from a healthy-but-quiet host.
            "state": match agent.turns.health() {
                TurnHealth::Connected => "connected",
                TurnHealth::Idle => "idle",
                TurnHealth::Unavailable => "unconfigured",
            },
            "inFlight": agent.turns.in_flight(),
            "url": agent.config.goose.acp.url,
            // Whether this process found the key `secretEnv` names. Not the key
            // — whether there is one. A host that is about to send every turn
            // into a 401 can see that here rather than inferring it from a log
            // it may not be reading; the header is only attached when this is
            // true.
            "secretSet": crate::acp::secret_key(&agent.config.goose.acp).is_some(),
            // Who runs the ACP server, and — when that is this process — what it
            // is doing. `mode` is the config value, so `external` here means
            // exactly what it means in the file and nothing is inferred.
            "serve": match &agent.serve {
                Some(status) => {
                    let health = status.health();
                    json!({
                        "mode": agent.config.goose.acp.serve.as_str(),
                        "managed": true,
                        "state": health.state,
                        "pid": health.pid,
                        // Spawns after the first: a goose that has been
                        // restarted is visible here rather than only in the log.
                        "restarts": health.restarts,
                    })
                }
                None => json!({
                    "mode": agent.config.goose.acp.serve.as_str(),
                    "managed": false,
                }),
            },
        },
        // The launcher is the process that started this one, on a host that has
        // one. It is a separate question from goose (`acp.serve`) and from this
        // process's own version: it says whether the *supervisor* is the one the
        // release shipped, which nothing else here can see.
        "launcher": launcher(),
        "registry": agent.registry.state(),
        // Where a viewer goes for the live feed, and whether there is one. The
        // `url` is relative on purpose: the agent's public address is already
        // `card.url`, and a second absolute spelling here would be one more thing
        // to keep in step.
        "activity": {
            "enabled": agent.activity.enabled(),
            "backlog": agent.activity.backlog_cap(),
            "subscribers": agent.activity.subscribers(),
            "url": "/events",
        },
        "sessions": {
            "count": agent.turns.in_flight(),
            // Sessions held for a context, so that reuse is visible rather than
            // inferred: a host serving a multi-turn conversation reports a
            // steady 1 while `count` returns to 0 between turns, and a host
            // whose sessions are all being closed at the end of every turn
            // reports 0 here and 0 there — which is the difference an operator
            // needs to see when the symptom is "my agent forgets".
            "retained": agent.turns.retained(),
        },
    })
}

/// `GET /sessions` (§6.6) — the sessions this host is holding, and the two
/// counts `/status` already reports, so a reader comparing the routes is not
/// looking at two different definitions of "a session".
///
/// It lists *retained* sessions. A context with a turn in flight has no retained
/// session — it is checked out, see pool rule 1 — so it is visible as a gap
/// between `inFlight` and the length of this list rather than as a row that
/// would have to be invented for it.
async fn list_sessions(State(agent): State<Arc<Agent>>) -> Json<Value> {
    Json(sessions_payload(&agent))
}

pub fn sessions_payload(agent: &Agent) -> Value {
    let sessions: Vec<Value> = agent
        .turns
        .sessions()
        .into_iter()
        .map(|session| {
            json!({
                "contextId": session.context_id,
                "sessionId": session.session_id,
                "cwd": session.cwd.display().to_string(),
                "skillId": session.skill_id,
                "idleSecs": session.idle_secs,
            })
        })
        .collect();

    json!({
        "inFlight": agent.turns.in_flight(),
        "retained": agent.turns.retained(),
        "sessions": sessions,
    })
}

/// `DELETE /sessions/{contextId}` (§6.6) — close the session a context is
/// holding, and forget it.
///
/// Three answers, and the status code is the one that says which: `200` closed,
/// `409` a turn is running for this context (stop it with `tasks/cancel`, which
/// is addressed to the task the caller already has, rather than by racing the
/// turn's own teardown here), `404` nothing was held. `404` is not a failure —
/// deleting twice must be as safe as deleting once, which is the same shape S5
/// recorded for the registry.
///
/// The conversation is not deleted. `session/close` ends the *session*; the
/// transcript is goose's own `sessions.db` and is durable by design
/// (constraint #6). A caller who wants it back reattaches with `session/load`.
async fn close_session(
    State(agent): State<Arc<Agent>>,
    Path(context_id): Path<String>,
) -> Response {
    match agent.turns.close_session(context_id.clone()).await {
        SessionClose::Closed { session_id } => (
            StatusCode::OK,
            Json(json!({
                "contextId": context_id,
                "sessionId": session_id,
                "closed": true,
            })),
        )
            .into_response(),
        SessionClose::Busy => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "session_busy",
                "contextId": context_id,
                "message": "a turn is running for this context, so its session is checked out; \
                            cancel the task (`tasks/cancel`) rather than closing the session \
                            underneath it",
            })),
        )
            .into_response(),
        SessionClose::Absent => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "no_such_session",
                "contextId": context_id,
                "message": "this context holds no session: it never had one, or it was already \
                            closed",
            })),
        )
            .into_response(),
    }
}

/// `GET /events` — the live activity feed (§`crate::activity`), as SSE.
///
/// Two phases, one stream: the backlog a new subscriber is owed, then the live
/// events. A consumer must drop any live event whose `seq` it has already seen,
/// because [`crate::activity::ActivityHub::subscribe`] takes the receiver *before*
/// the backlog, so the seam duplicates rather than gaps (deliberate — a duplicate
/// is visible, a gap is silent). This handler does the dropping, so a browser-side
/// `EventSource` needs no `seq` bookkeeping of its own.
///
/// `403` rather than an empty stream when the feed is disabled: a feed that has
/// been switched off and a feed with nothing to say are different answers, and
/// only one of them is a thing to fix.
async fn events(State(agent): State<Arc<Agent>>) -> Response {
    if !agent.activity.enabled() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "activity_disabled",
                "message": "this host has observability.activity.enabled: false, so nothing is \
                            recorded and there is nothing to stream",
            })),
        )
            .into_response();
    }

    let (backlog, receiver) = agent.activity.subscribe();
    let last_replayed = backlog.last().map(|activity| activity.seq);

    let backlog_stream = futures::stream::iter(backlog.into_iter().map(activity_event));
    let live = futures::stream::unfold(
        (receiver, last_replayed),
        |(mut receiver, mut last)| async move {
            loop {
                match receiver.recv().await {
                    Ok(activity) => {
                        // Skip the duplicate at the seam; a `seq` is never reused,
                        // so `<=` also drops anything already replayed.
                        if last.is_some_and(|seen| activity.seq <= seen) {
                            continue;
                        }
                        last = Some(activity.seq);
                        return Some((activity_event(activity), (receiver, last)));
                    }
                    // A slow subscriber dropped frames but the stream is still
                    // live: keep reading rather than ending it.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(backlog_stream.chain(live))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// One activity as one SSE frame. Infallible by construction: an `Activity` is
/// owned plain data, so the fallback comment is unreachable rather than a plan.
///
/// The SSE `id` is the event's `seq`: a browser `EventSource` that reconnects
/// sends it back as `Last-Event-ID`, so a client can pick up where it left off
/// rather than re-reading the whole backlog. (The route does not *serve*
/// per-`Last-Event-ID` replay yet — the backlog covers the common case — but
/// emitting the id now means a client that wants resume can be written against
/// a stable contract.)
fn activity_event(activity: Activity) -> Result<Event, Infallible> {
    let id = activity.seq.to_string();
    Ok(Event::default()
        .id(id)
        .event("activity")
        .json_data(activity)
        .unwrap_or_else(|_| Event::default().comment("activity serialization failed")))
}

/// The variable names the launcher exports on the way to its `exec`.
///
/// The launcher is generated (LAUNCHING.md, *What the launcher tells the
/// payload*), and these three strings are the whole contract between it and this
/// process. They are the launcher's own names, not ours: `FETCH_LAUNCH_*` says
/// where the value came from, which matters because nothing else in this process
/// knows anything about it.
pub const LAUNCHER_PATH_ENV: &str = "FETCH_LAUNCH_PATH";
pub const LAUNCHER_VERSION_ENV: &str = "FETCH_LAUNCH_VERSION";
pub const LAUNCHER_SHA256_ENV: &str = "FETCH_LAUNCH_SHA256";

/// The launcher that started this process, as it described itself on the way in.
///
/// `managed: false` means there was no launcher: a test, a developer's
/// `cargo run`, or a payload started by hand. That is a fact worth reporting
/// rather than a gap to fill with a guess — the same shape `acp.serve` uses to
/// say that goose was not started by this process.
///
/// `version` is the release the launcher last verified itself against, which is
/// the only version a launcher has: it is fetched fresh from whichever release
/// is current rather than versioned on its own. `sha256` is the one to hold
/// against a release manifest's `launcher.sha256` — they are equal exactly when
/// the host's launcher is current, so this is what makes "we shipped a launcher"
/// checkable from outside the box. Both can lag the payload by a single start,
/// because a launcher self-update takes effect on the next one.
///
/// Read from the environment on every call rather than snapshotted: the values
/// are what this process was started with, and they cannot change under it.
fn launcher() -> Value {
    launcher_from(|name| std::env::var(name).ok())
}

fn launcher_from(read: impl Fn(&str) -> Option<String>) -> Value {
    // An empty export is the launcher saying "I could not work this out" (no
    // `sha256` tool on the host, `$0` it could not resolve), which is not the
    // same as a value, and must not read as one.
    let field = |name: &str| read(name).filter(|value| !value.is_empty());
    match field(LAUNCHER_PATH_ENV) {
        Some(path) => json!({
            "managed": true,
            "path": path,
            "version": field(LAUNCHER_VERSION_ENV).unwrap_or_default(),
            "sha256": field(LAUNCHER_SHA256_ENV).unwrap_or_default(),
        }),
        None => json!({ "managed": false }),
    }
}

/// Rejects anything without the bearer token.
///
/// The comparison is constant-time. `a == b` on a `String` short-circuits on the
/// first differing byte, which leaks the token's prefix to anyone who can
/// measure; a token check is exactly the place where that matters.
async fn require_bearer(
    State(expected): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// Equality with no data-dependent early exit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::config::ServeMode;

    /// The regression for the `SIGTERM` hang: an SSE subscriber must not hold
    /// the process open.
    ///
    /// `GET /events` is a stream that never ends *by design*, so the bare
    /// graceful shutdown this replaced waited on it forever. The observed
    /// failure was a host that closed its `:10001` listener on `SIGTERM` (so
    /// `/healthz` returned 000) and then sat parked — the goose child was never
    /// stopped and the process never exited. `serve_until` must return inside
    /// its grace with the connection still open.
    #[tokio::test]
    async fn an_open_sse_subscriber_does_not_hold_the_shutdown_open() {
        use tokio::io::AsyncWriteExt;

        let app = Router::new().route(
            "/never",
            get(|| async { Sse::new(futures::stream::pending::<Result<Event, Infallible>>()) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let (shutdown, wait_for_it) = tokio::sync::oneshot::channel::<()>();
        let grace = Duration::from_millis(300);
        let serving = tokio::spawn(serve_until(
            listener,
            app,
            async move {
                let _ = wait_for_it.await;
            },
            grace,
        ));

        // Subscribe and *hold* the connection: the route answers with an SSE
        // stream that never yields, so the socket stays open until we drop it.
        let mut subscriber = tokio::net::TcpStream::connect(addr).await.expect("connect");
        subscriber
            .write_all(b"GET /never HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        // Let the server accept and start the stream before asking it to stop.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = shutdown.send(());
        let started = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), serving)
            .await
            .expect("serve_until returned despite the open subscriber")
            .expect("the serving task did not panic");
        result.expect("a clean return");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the drain was bounded by the grace, not by the subscriber: took {:?}",
            started.elapsed()
        );

        drop(subscriber);
    }

    fn agent() -> Agent {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
        // Name a variable nothing sets, so the registry is `unconfigured`
        // regardless of what the environment running the tests happens to hold.
        // The default name is `LITELLM_MASTER_KEY`, which a dev container has —
        // and a test whose result depends on that is a test that fails on a
        // developer's machine and passes in CI, which is the wrong way round.
        config.registry.master_key_env = "A2A_GOOSE_TEST_UNSET_MASTER_KEY".to_string();
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = crate::card::assemble(&config, &skills);
        let card_hash = crate::card::hash(&card);
        // Built before the move into `Agent`, and from *this* config, so it
        // reads the unset key env above rather than the ambient environment.
        let registry = Registry::new(&config);
        Agent {
            config,
            skills: Arc::new(skills),
            card,
            card_hash,
            goose: Goose {
                path: "/usr/local/bin/goose".into(),
                version: crate::goose::Version::new(1, 50, 0),
            },
            // A path that does not exist: these tests never ask for history,
            // and a store that fails open is exactly what history should do
            // when there is no database.
            history: crate::history::HistoryStore::new("/nonexistent/a2a-goose/sessions.db"),
            registry,
            // `/status` must not need a `goose serve` to answer, so a fake is
            // the right thing for a status test: it is the *unavailable* case.
            turns: Arc::new(crate::turn::NoTurns),
            serve: None,
            // A disabled hub: `/status` reports the feed's shape without a
            // running subscriber, and nothing in these tests records.
            activity: Arc::new(ActivityHub::disabled()),
            started: Instant::now(),
        }
    }

    /// `/status` says who starts goose, and — when that is this process — what
    /// the child it started is doing. The `external` branch reports `managed:
    /// false` rather than omitting the field, because the two are different
    /// states and a reader should not have to infer which one they are in.
    #[test]
    fn status_says_who_starts_goose_and_what_that_server_is_doing() {
        let mut managed = agent();
        managed.config.goose.acp.serve = ServeMode::Own;
        managed.serve = Some(Arc::new(ServeStatus::new()));
        let payload = status_payload(&managed);
        assert_eq!(payload["acp"]["serve"]["mode"], "own");
        assert_eq!(payload["acp"]["serve"]["managed"], true);
        assert_eq!(payload["acp"]["serve"]["state"], "starting");
        assert!(payload["acp"]["serve"]["pid"].is_null());
        assert_eq!(payload["acp"]["serve"]["restarts"], 0);

        let mut external = agent();
        external.config.goose.acp.serve = ServeMode::External;
        let payload = status_payload(&external);
        assert_eq!(payload["acp"]["serve"]["mode"], "external");
        assert_eq!(payload["acp"]["serve"]["managed"], false);
    }

    #[test]
    fn status_names_the_goose_it_verified_and_the_card_it_serves() {
        let payload = status_payload(&agent());
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["goose"]["version"], "1.50.0");
        assert_eq!(payload["goose"]["path"], "/usr/local/bin/goose");
        assert_eq!(payload["card"]["name"], "a2a-goose");
        assert_eq!(payload["card"]["protocolVersion"], "1.0");
        assert_eq!(
            payload["card"]["url"],
            "http://mac-studio.tail86fd19.ts.net:10001"
        );
        assert_eq!(payload["card"]["hash"].as_str().map(str::len), Some(64));
        assert_eq!(payload["registry"]["state"], "unconfigured");
        assert_eq!(payload["acp"]["state"], "unconfigured");
    }

    #[test]
    fn status_lists_the_skills_and_marks_the_default() {
        let payload = status_payload(&agent());
        let skills = payload["skills"].as_array().expect("skills array");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["id"], "ask");
        assert_eq!(skills[0]["dispatch"], "ask");
        assert_eq!(skills[0]["default"], true);
    }

    #[test]
    fn status_reports_the_loop_bounds_and_never_a_budget() {
        let payload = status_payload(&agent());
        assert_eq!(payload["limits"]["maxIterationsPerTask"], 12);
        assert_eq!(payload["limits"]["maxConcurrentSessions"], 4);
        let rendered = payload.to_string();
        for forbidden in ["budget", "maxBudget", "dollar", "usd"] {
            assert!(
                !rendered.contains(forbidden),
                "the status payload must not imply a dollar control: {rendered}"
            );
        }
    }

    #[test]
    fn status_says_how_many_sessions_are_being_held() {
        // `NoTurns` holds none, which is the honest answer for a process with no
        // ACP runner: not "unknown", and not a number that implies reuse is
        // happening.
        let payload = status_payload(&agent());
        assert_eq!(payload["sessions"]["count"], 0);
        assert_eq!(payload["sessions"]["retained"], 0);
    }

    #[test]
    fn status_reports_the_launcher_that_started_this_process() {
        // The pair an operator holds against a release manifest's
        // `launcher.sha256`: equal exactly when the host's launcher is current.
        let payload = launcher_from(|name| match name {
            LAUNCHER_PATH_ENV => {
                Some("/Users/nick/.local/share/a2a-goose/fetch-launch.sh".to_string())
            }
            LAUNCHER_VERSION_ENV => Some("0.1.18".to_string()),
            LAUNCHER_SHA256_ENV => {
                Some("9270793a4c4f6410b526872c57fd81759c2266562fa74ea8e26aed1690220da2".to_string())
            }
            _ => None,
        });

        assert_eq!(payload["managed"], true);
        assert_eq!(
            payload["path"],
            "/Users/nick/.local/share/a2a-goose/fetch-launch.sh"
        );
        assert_eq!(payload["version"], "0.1.18");
        assert_eq!(
            payload["sha256"],
            "9270793a4c4f6410b526872c57fd81759c2266562fa74ea8e26aed1690220da2"
        );
    }

    #[test]
    fn status_says_so_when_there_is_no_launcher() {
        // A test, a developer's `cargo run`, a payload started by hand: there is
        // no launcher to report, and inventing one would make the digest
        // meaningless on the hosts where it matters.
        assert_eq!(launcher_from(|_| None), json!({ "managed": false }));
        // An empty export is the launcher saying it could not work the value
        // out, which is not a value.
        assert_eq!(
            launcher_from(|name| match name {
                LAUNCHER_PATH_ENV => Some(String::new()),
                _ => Some("0.1.18".to_string()),
            }),
            json!({ "managed": false })
        );
    }

    #[test]
    fn status_keeps_a_launcher_that_could_not_describe_itself() {
        // A host with no `sha256sum` still has a launcher, and the path is how
        // an operator finds it; the empty fields are the honest answer.
        let payload = launcher_from(|name| match name {
            LAUNCHER_PATH_ENV => Some("/opt/goose/fetch-launch.sh".to_string()),
            LAUNCHER_VERSION_ENV => Some(String::new()),
            LAUNCHER_SHA256_ENV => Some(String::new()),
            _ => None,
        });

        assert_eq!(payload["managed"], true);
        assert_eq!(payload["path"], "/opt/goose/fetch-launch.sh");
        assert_eq!(payload["version"], "");
        assert_eq!(payload["sha256"], "");
    }

    #[test]
    fn the_bearer_check_is_exact_and_constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn healthz_is_liveness_only() {
        assert_eq!(healthz().await, "ok");
    }

    #[test]
    fn status_advertises_the_activity_feed_and_where_to_follow_it() {
        let payload = status_payload(&agent());
        // `agent()` installs a disabled hub, so the shape is reported without a
        // running subscriber: the feed's existence, its bound, and its route.
        assert_eq!(payload["activity"]["enabled"], false);
        assert_eq!(payload["activity"]["url"], "/events");
        assert_eq!(payload["activity"]["subscribers"], 0);
        assert!(payload["activity"]["backlog"].as_u64().unwrap_or(0) >= 1);
    }

    #[tokio::test]
    async fn a_disabled_feed_is_refused_rather_than_streamed_empty() {
        // A feed that has been switched off and a feed with nothing to say are
        // different answers; only one is a thing to fix, so `/events` says which
        // rather than serving an empty stream that reads like quiet.
        let response = events(State(Arc::new(agent()))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let payload: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["error"], "activity_disabled");
    }

    #[test]
    fn an_activity_serialises_with_a_type_tag_and_the_turns_identity() {
        let hub = ActivityHub::new(true, 8);
        hub.record(
            Some("ctx-1"),
            Some("task-1"),
            Some("sess_1"),
            Some("ask"),
            crate::activity::ActivityEvent::ToolCall {
                id: "call_1".to_string(),
                title: Some("Read a file".to_string()),
                tool_kind: Some("read".to_string()),
                status: Some("in_progress".to_string()),
            },
        );
        let activity = hub.recent().pop().expect("one event");
        let json = serde_json::to_value(&activity).expect("serialises");

        // The discriminator a viewer switches on, the camelCase envelope, and the
        // flattened step fields - the shape `/events` promises.
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["contextId"], "ctx-1");
        assert_eq!(json["taskId"], "task-1");
        assert_eq!(json["sessionId"], "sess_1");
        assert_eq!(json["skill"], "ask");
        assert_eq!(json["id"], "call_1");
        assert_eq!(json["toolKind"], "read");
    }
}
