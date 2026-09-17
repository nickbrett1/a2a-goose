//! [`Turns`] over ACP: one `goose serve` connection, one session per context.
//!
//! This is the only place that turns [`crate::turn`]'s vocabulary into ACP
//! method calls. Decisions worth reading before the code:
//!
//! - **The connection is shared and long-lived.** [S4] proved goose multiplexes
//!   concurrent sessions on one connection, so connecting is amortised and the
//!   demultiplexer in [`crate::acp::transport`] does the routing.
//! - **A session belongs to a `contextId`, and its reuse policy lives in
//!   [`crate::acp::pool`].** A turn with a context and no session of its own
//!   running gets the context's session; the turn after it gets the same one,
//!   which is what gives a conversation its memory. Which session that is, when
//!   it is given up and what evicts it are [`crate::acp::pool`]'s business; what
//!   this module adds is the wiring — a session only exists on the connection
//!   that opened it, so the pool lives *inside* [`Connection`] and cannot be
//!   forgotten separately from it.
//! - **A dead connection is dropped, not repaired in place.** If the connection
//!   fails at the transport layer, the cache *and the pool* are cleared, so the
//!   next turn opens a fresh connection with its own `initialize` and every
//!   context starts a new session. A half-open connection that answers nothing
//!   would otherwise poison every subsequent turn — and `goose serve` restarting
//!   under a supervised process is a normal event, not an exceptional one. The
//!   cost is real and is worth naming: a reconnect loses every context's session,
//!   so the conversations lose their label (goose's own `sessions.db` keeps the
//!   history). It is still the right trade, because the alternative is handing
//!   out sessions that cannot answer.
//! - **The wall-clock bound lives here, not at the A2A edge.** A bound checked
//!   between events cannot fire on the turn that has gone quiet, which is the
//!   only kind of turn that needs one.
//!
//! [S4]: ../../spikes/S4.md

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::acp::client::{AcpClient, Session};
use crate::acp::pool::{Acquired, Claim, Idle, Pool};
use crate::acp::transport::AcpError;
use crate::config::Config;
use crate::turn::{
    ContextUsage, Limit, SessionClose, SessionInfo, TurnError, TurnEvent, TurnHealth, TurnRequest,
    Turns, Usage,
};

/// How many A2A frames may be buffered for a caller that is reading slowly.
/// The same trade as the transport's: a bounded buffer that drops the *turn*
/// (rather than the frames) once it is full, because an unbounded one is a
/// memory leak with a friendly name.
const EVENT_BUFFER: usize = 256;

/// The `stopReason` goose sends when a turn ran to completion.
pub const STOP_END_TURN: &str = "end_turn";

/// A message for a poisoned pool lock. Poisoning means a panic while a turn held
/// the pool, which is a bug rather than a condition to recover from — but a turn
/// that has already failed should not fail *again* on the way out.
const POOL_POISONED: &str = "the session pool lock is poisoned";

/// Turns over ACP.
pub struct AcpTurns {
    config: Arc<Config>,
    connection: Arc<Connection>,
    permits: Arc<Semaphore>,
    capacity: usize,
}

/// A session kept for a context, and the stream its updates arrive on.
///
/// The receiver is stored *with* the session rather than inside
/// [`crate::acp::client::Session`] so that both can be borrowed at once: the
/// prompt needs `&Session` and the update loop needs `&mut Receiver`, and
/// keeping them in one struct would make that a borrow error.
struct Retained {
    session: Session,
    updates: mpsc::Receiver<Value>,
    /// The skill of the turn that last ran here, for `GET /sessions`.
    ///
    /// Kept beside the session rather than in the pool because the pool's
    /// [`Idle`] is policy — which session, until when — and a skill is not part
    /// of either question. It is refreshed on every retain, because the skill is
    /// chosen per turn and §6.3 is explicit that a context may legally run under
    /// a different one without forking its conversation.
    skill: String,
}

impl Retained {
    /// Lets the pool's eviction paths close what they dropped without knowing
    /// what a session is made of.
    async fn close(&self) {
        self.session.close().await;
    }
}

/// The connection shared by every turn, and the state that only makes sense
/// alongside it.
///
/// The pool is a field here rather than a sibling because a session is
/// meaningless on any connection but the one that opened it: there is no way to
/// lose one without losing the other, which is what rule 3 of
/// [`crate::acp::pool`] asks for and what this shape makes unfalsifiable.
#[derive(Default)]
struct Connection {
    client: Mutex<Option<Arc<AcpClient>>>,
    /// A `std` mutex, unlike the client's, and deliberately: nothing here is
    /// ever held across an `await` — the lock is taken, a decision is made, and
    /// it is dropped — so there is no reason to pay for an async one, and a
    /// synchronous lock is what [`Claim`]'s `Drop` can use. Lock order, when
    /// both are needed, is client before pool; no path takes them the other way
    /// round.
    pool: std::sync::Mutex<Pool<Retained>>,
    /// Read by `/status` without taking a lock: `/status` must never be the
    /// thing that blocks.
    live: AtomicBool,
}

impl AcpTurns {
    pub fn new(config: Arc<Config>) -> Self {
        // `max(1)` because a semaphore with zero permits is a deadlock, and the
        // config field is a `usize` that could be written as 0.
        let capacity = config.registry.limits.max_concurrent_sessions.max(1);
        Self {
            config,
            connection: Arc::new(Connection::default()),
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// How many turns could run at once.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Turns for AcpTurns {
    fn run(&self, request: TurnRequest) -> BoxStream<'static, Result<TurnEvent, TurnError>> {
        let (events, received) = mpsc::channel(EVENT_BUFFER);
        let config = Arc::clone(&self.config);
        let connection = Arc::clone(&self.connection);
        let permits = Arc::clone(&self.permits);

        // The turn runs in its own task because `execute` is synchronous: the
        // SDK asks for a stream *now* and reads it as the turn proceeds.
        tokio::spawn(async move {
            if let Err(err) = run_turn(&config, &connection, &permits, request, &events).await {
                // The terminal error is the last thing the caller sees. A send
                // failure here means the caller is already gone, which is not
                // worth a log line.
                let _ = events.send(Err(err)).await;
            }
        });

        Box::pin(futures::stream::unfold(
            received,
            |mut received| async move { received.recv().await.map(|item| (item, received)) },
        ))
    }

    fn health(&self) -> TurnHealth {
        if self.connection.live.load(Ordering::Relaxed) {
            TurnHealth::Connected
        } else {
            // Wired but cold: the first turn will connect. Reporting this as
            // "unconfigured" would send an operator looking for a config bug
            // that is not there.
            TurnHealth::Idle
        }
    }

    fn in_flight(&self) -> usize {
        self.capacity - self.permits.available_permits()
    }

    fn retained(&self) -> usize {
        // A poisoned lock reads as zero rather than panicking: `/status` is what
        // an operator asks when something is already wrong.
        self.connection.pool.lock().map_or(0, |pool| pool.len())
    }

    fn sessions(&self) -> Vec<SessionInfo> {
        // One lock, one pass, then the lock is gone: everything below renders
        // from copies, so a serialisation of this listing cannot hold the pool
        // against the next turn.
        let now = Instant::now();
        self.connection
            .pool
            .lock()
            .map(|pool| {
                pool.iter()
                    .map(|(context, idle)| SessionInfo {
                        context_id: context.to_string(),
                        session_id: idle.session.session.id().to_string(),
                        cwd: idle.cwd.clone(),
                        skill_id: idle.session.skill.clone(),
                        idle_secs: now.saturating_duration_since(idle.last_used).as_secs(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn close_session(&self, context: String) -> BoxFuture<'static, SessionClose> {
        let connection = Arc::clone(&self.connection);
        Box::pin(async move {
            // The lock is taken, one session is removed, and it is dropped
            // before anything is awaited: `session/close` is I/O, and holding a
            // `std` mutex across an await is the one thing this pool's lock
            // order forbids.
            let taken = match connection.pool.lock() {
                Ok(mut pool) => pool.take(&context),
                // A poisoned pool is somebody else's bug. Reporting "nothing
                // here" is the honest answer for a control route that must not
                // add a second panic on its way out.
                Err(poisoned) => poisoned.into_inner().take(&context),
            };

            match taken {
                Some(idle) => {
                    let session_id = idle.session.session.id().to_string();
                    // Closed, never forgotten: this is the same eviction the
                    // pool describes, and it means the same thing here. The
                    // session is *not* deleted — goose's `sessions.db` is the
                    // record, and it is durable by design (constraint #6).
                    idle.session.close().await;
                    tracing::info!(
                        %context,
                        %session_id,
                        "closed the session held for a context, on request"
                    );
                    SessionClose::Closed { session_id }
                }
                None => {
                    // Not held. Either a turn has it checked out, or the context
                    // has no session at all — and the caller is told which,
                    // because `tasks/cancel` is the answer to only one of them.
                    let busy = connection
                        .pool
                        .lock()
                        .map(|pool| pool.is_claimed(&context))
                        .unwrap_or(false);
                    if busy {
                        SessionClose::Busy
                    } else {
                        SessionClose::Absent
                    }
                }
            }
        })
    }
}

/// Claims a context for this turn and hands over its idle session, if it has one
/// that may be reused.
///
/// The returned guard must live as long as the turn: it is what makes the
/// context look busy to a second turn that arrives meanwhile, and its `Drop` is
/// what releases it — on the happy path, on an error, and on a timeout alike.
fn take_session<'a>(
    config: &Config,
    connection: &'a Connection,
    context: &str,
    cwd: &Path,
) -> (Acquired<Retained>, Claim<'a, Retained>, Vec<Idle<Retained>>) {
    let sessions = &config.goose.sessions;
    let mut pool = connection.pool.lock().expect(POOL_POISONED);
    let (acquired, dropped) = pool.acquire(
        context,
        cwd,
        Instant::now(),
        Duration::from_secs(sessions.idle_ttl_secs),
    );
    (
        acquired,
        Claim::new(&connection.pool, context.to_string()),
        dropped,
    )
}

/// Puts a finished session back as its context's own. Returns whatever the pool
/// evicted to make room, for the caller to close.
fn retain_session(
    config: &Config,
    connection: &Connection,
    context: String,
    retained: Retained,
    cwd: PathBuf,
) -> Vec<Idle<Retained>> {
    let idle = Idle {
        session: retained,
        cwd,
        last_used: Instant::now(),
    };
    connection.pool.lock().expect(POOL_POISONED).retain(
        &context,
        idle,
        config.goose.sessions.max_sessions,
    )
}

/// Forgets the connection and everything that lived on it.
///
/// The sessions are dropped rather than closed: `session/close` is a request,
/// and the connection that would answer it is the one that has gone. Dropping
/// them closes their streams, and goose reaps the sessions from its own side.
async fn die(connection: &Connection) {
    let mut slot = connection.client.lock().await;
    *slot = None;
    connection.live.store(false, Ordering::Relaxed);
    discard_sessions(connection);
}

/// Empties the session pool, for a caller that already holds the client lock.
///
/// A session is only meaningful on the connection that opened it, so the two
/// always go together - and both the paths that lose a connection take them
/// together here rather than one of them forgetting to.
fn discard_sessions(connection: &Connection) {
    let discarded = connection.pool.lock().expect(POOL_POISONED).clear();
    if discarded > 0 {
        tracing::info!(
            sessions = discarded,
            "discarded the session pool along with its connection"
        );
    }
}

async fn run_turn(
    config: &Config,
    connection: &Connection,
    permits: &Semaphore,
    request: TurnRequest,
    events: &mpsc::Sender<Result<TurnEvent, TurnError>>,
) -> Result<(), TurnError> {
    // The concurrency bound waits, and the wait is bounded by the turn's own
    // ceiling: an unbounded queue in front of a bounded pool hides the overload
    // it exists to make visible.
    let _permit = tokio::time::timeout(request.wall_clock, permits.acquire())
        .await
        .map_err(|_| TurnError::Limit {
            limit: Limit::WallClock,
            allowed: request.wall_clock.as_secs(),
            observed: request.wall_clock.as_secs(),
        })?
        .map_err(|_| {
            TurnError::Transport(
                "the session limiter was closed while this turn waited".to_string(),
            )
        })?;

    let client = connect_or_reuse(config, connection).await?;

    // A turn belongs to a context only if the caller named one and this host
    // keeps sessions at all. Otherwise it is exactly the turn M1 ran: a fresh
    // session, closed when it is done, with nothing remembered between turns.
    let context = match config.goose.sessions.reuse_by_context {
        true => request.context.clone(),
        false => None,
    };

    // `owns` is the pool's answer to "may this turn become the context's
    // session": true if it took the context's own session, or if it is the first
    // turn for that context. A turn that arrives while another is running gets a
    // throwaway instead — see rule 1 of `crate::acp::pool`.
    let mut owns = true;
    let mut acquired = None;
    let mut claim = None;
    if let Some(context) = context.as_deref() {
        let (take, guard, evicted) = take_session(config, connection, context, &request.cwd);
        owns = match take {
            Acquired::Reuse(idle) => {
                acquired = Some(idle);
                true
            }
            Acquired::Fresh { vacant } => vacant,
        };
        claim = Some(guard);
        for idle in evicted {
            // Evicted by the pool's own rules, on a connection that is alive:
            // closed rather than dropped, because an abandoned session is one
            // nothing will ever close.
            idle.session.close().await;
        }
    }

    // This turn's session: the context's, or a fresh one. A retained session
    // arrives with its update stream, so nothing is re-subscribed.
    let (session, mut updates) = match acquired {
        Some(idle) => {
            let Retained {
                session, updates, ..
            } = idle.session;
            (session, updates)
        }
        None => match client.new_session(&request.cwd).await {
            Ok(pair) => pair,
            Err(err) => {
                // Everything on this connection is now suspect, and the next
                // turn must not be handed any of it.
                if err.is_connection_loss() {
                    die(connection).await;
                }
                return Err(TurnError::Transport(err.to_string()));
            }
        },
    };

    let outcome = match tokio::time::timeout(
        request.wall_clock,
        pump(&session, &mut updates, &request, events),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(PumpError::Acp(err))) => {
            if err.is_connection_loss() {
                die(connection).await;
            }
            Err(TurnError::Transport(err.to_string()))
        }
        Ok(Err(PumpError::CallerGone)) => Err(TurnError::Transport(
            "the caller disconnected while the turn was running".to_string(),
        )),
        Err(_) => Err(TurnError::Limit {
            limit: Limit::WallClock,
            allowed: request.wall_clock.as_secs(),
            observed: request.wall_clock.as_secs(),
        }),
    };

    // A session is kept only if the turn finished cleanly *and* this turn is the
    // one that owns the context's memory. A failure does not keep it: after a
    // wall-clock timeout the prompt may still be running inside goose, and
    // handing the next turn a session with a prompt in flight would be worse
    // than making it start over. The context loses its session either way only
    // when a turn fails — and it loses the *label*, not the conversation, which
    // is goose's own `sessions.db` (hard constraint #1).
    match (context, outcome.is_ok() && owns) {
        (Some(context), true) => {
            let retained = Retained {
                session,
                updates,
                skill: request.skill.clone(),
            };
            for idle in retain_session(config, connection, context, retained, request.cwd.clone()) {
                idle.session.close().await;
            }
        }
        // Best effort, and after the outcome is decided, so a session that
        // cannot be closed does not turn a completed turn into a failed one.
        _ => session.close().await,
    }
    // Released last: the context stays busy until its session is back in the
    // pool, so a turn arriving in between runs a throwaway rather than racing
    // this one for the same session.
    drop(claim);

    outcome
}

/// Why a pump stopped before it saw the turn end.
enum PumpError {
    /// The ACP hop itself failed.
    Acp(AcpError),
    /// The caller stopped reading. The turn is abandoned, not broken.
    CallerGone,
}

impl From<AcpError> for PumpError {
    fn from(err: AcpError) -> Self {
        Self::Acp(err)
    }
}

/// Reads a live session's updates while awaiting the prompt's reply.
async fn pump(
    session: &Session,
    updates: &mut mpsc::Receiver<Value>,
    request: &TurnRequest,
    events: &mpsc::Sender<Result<TurnEvent, TurnError>>,
) -> Result<(), PumpError> {
    // Whatever is already buffered belongs to the turn *before* this one, and
    // must not be reported as this turn's news. This is not hypothetical: the
    // turn S3 recorded ends with a `session_info_update` that arrives *after*
    // the prompt's reply, so a reused session really does start with a previous
    // turn's frame already waiting. Frames still in flight cannot be told apart
    // from this turn's — they carry no turn id — but the ones that have landed
    // can be, and are.
    while updates.try_recv().is_ok() {}

    // Started *after* the stream is draining, and the ordering is why the stream
    // is handed to this function at all: a reply that arrives on a stream nobody
    // is reading is a reply that never arrives.
    let prompt = session.prompt(&request.prompt);
    futures::pin_mut!(prompt);

    let mut stream_open = true;
    loop {
        tokio::select! {
            // Biased so a reply that is already waiting is never delayed behind
            // a burst of notifications.
            biased;
            reply = &mut prompt => {
                let reply = reply?;
                for event in finish_events(&reply) {
                    emit(events, event).await?;
                }
                return Ok(());
            }
            update = updates.recv(), if stream_open => match update {
                Some(frame) => {
                    for event in update_events(&frame, session.id()) {
                        emit(events, event).await?;
                    }
                }
                // The session stream ended without the reply. Keep waiting on
                // the prompt: the transport's own timeout is what decides that
                // case, and inventing an error here would race it.
                None => stream_open = false,
            },
        }
    }
}

async fn emit(
    events: &mpsc::Sender<Result<TurnEvent, TurnError>>,
    event: TurnEvent,
) -> Result<(), PumpError> {
    events
        .send(Ok(event))
        .await
        .map_err(|_| PumpError::CallerGone)
}

async fn connect_or_reuse(
    config: &Config,
    connection: &Connection,
) -> Result<Arc<AcpClient>, TurnError> {
    let mut slot = connection.client.lock().await;
    if let Some(client) = slot.as_ref() {
        if client.is_alive() {
            return Ok(Arc::clone(client));
        }
        // The connection-level stream has ended, so goose has forgotten this
        // connection id: it is not that a request failed, it is that this
        // connection no longer exists, and the next request on it would be
        // answered `404` over healthy HTTP. A supervised `goose serve` restart
        // is the ordinary way that happens now, so the turn that follows one
        // reconnects here rather than surfacing a 404 to its caller.
        tracing::info!(
            "the ACP connection has ended (goose restarted?): reconnecting, which starts a fresh \
             session per context - the conversation stays in goose's own sessions.db, but the \
             model does not carry it into the new session"
        );
        *slot = None;
        connection.live.store(false, Ordering::Relaxed);
        discard_sessions(connection);
    }
    // Held across the connect on purpose: two turns arriving on a cold cache
    // should produce one connection, not two racing `initialize`s. Nothing needs
    // clearing on failure: a connection slot that is empty already means an empty
    // pool, because `die` is the only thing that empties one.
    let client = Arc::new(
        AcpClient::connect(&config.goose.acp)
            .await
            .map_err(|err| TurnError::Transport(err.to_string()))?,
    );
    *slot = Some(Arc::clone(&client));
    connection.live.store(true, Ordering::Relaxed);
    Ok(client)
}

/// The events a `session/update` frame carries for a given session.
///
/// Only two kinds are read, and both are read for a reason: the answer text, and
/// the context-window reading the token bound needs. `session_info_update` and
/// `available_commands_update` are carried for a UI's benefit; nothing here is
/// a UI.
pub fn update_events(frame: &Value, session_id: &str) -> Vec<TurnEvent> {
    if frame.pointer("/params/sessionId").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let Some(update) = frame.pointer("/params/update") else {
        return Vec::new();
    };

    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            let content = update.get("content");
            let is_text = content
                .and_then(|content| content.get("type"))
                .and_then(Value::as_str)
                == Some("text");
            match content
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            {
                // A chunk that is not text is not rendered as text: a caller
                // that asked for text must not be handed a base64 blob in its
                // place, and dropping it silently would hide the difference.
                Some(text) if is_text => vec![TurnEvent::Text(text.to_string())],
                Some(_) => {
                    tracing::debug!("ignoring a non-text agent_message_chunk");
                    Vec::new()
                }
                None => Vec::new(),
            }
        }
        Some("usage_update") => {
            let used = update.get("used").and_then(Value::as_u64);
            let size = update.get("size").and_then(Value::as_u64);
            match (used, size) {
                (Some(used), Some(size)) => {
                    vec![TurnEvent::ContextUsage(ContextUsage { used, size })]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// The events the `session/prompt` reply carries.
pub fn finish_events(result: &Value) -> Vec<TurnEvent> {
    // A reply with no `stopReason` still finished the turn; treating it as an
    // error would fail turns that goose considers done, and `end_turn` is the
    // documented answer for a turn that ran out of things to do.
    let stop_reason = result
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or(STOP_END_TURN)
        .to_string();
    let usage = result.get("usage").map(parse_usage).unwrap_or_default();
    vec![TurnEvent::Finished { stop_reason, usage }]
}

fn parse_usage(usage: &Value) -> Usage {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        total: field("totalTokens"),
        input: field("inputTokens"),
        output: field("outputTokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames S3 recorded, verbatim — the same fixture the transport tests
    /// replay, read here as a turn rather than as a routing problem.
    const TURN: &str = include_str!("../../tests/fixtures/acp-turn.jsonl");

    fn frames() -> Vec<Value> {
        TURN.lines()
            .map(|line| serde_json::from_str(line).expect("fixture is JSONL"))
            .collect()
    }

    #[test]
    fn the_recorded_turn_becomes_one_text_delta_two_readings_and_a_finish() {
        let mut events = Vec::new();
        for frame in frames() {
            // The replies (which carry an `id`) are the prompt's result; the rest
            // are that session's updates.
            if frame.get("id").is_some() && frame["id"] == 2 {
                events.extend(finish_events(&frame["result"]));
            } else {
                events.extend(update_events(&frame, "sess_0001"));
            }
        }

        assert_eq!(
            events,
            vec![
                TurnEvent::ContextUsage(ContextUsage {
                    used: 0,
                    size: 200_000
                }),
                TurnEvent::Text("ok".to_string()),
                TurnEvent::ContextUsage(ContextUsage {
                    used: 33_999,
                    size: 200_000
                }),
                TurnEvent::Finished {
                    stop_reason: "end_turn".to_string(),
                    usage: Usage {
                        total: 33_999,
                        input: 33_997,
                        output: 2
                    },
                },
            ],
            "the recording must yield exactly these events and nothing else"
        );
    }

    #[test]
    fn an_update_for_another_session_is_not_this_turns() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_other",
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "someone else's answer" } }
            }
        });
        assert!(update_events(&frame, "sess_0001").is_empty());
    }

    #[test]
    fn a_non_text_chunk_is_dropped_rather_than_faked_as_text() {
        let frame = serde_json::json!({
            "method": "session/update",
            "params": {
                "sessionId": "sess_0001",
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "image", "data": "aGk=" } }
            }
        });
        assert!(update_events(&frame, "sess_0001").is_empty());
    }

    #[test]
    fn the_update_kinds_a_ui_needs_are_not_turn_events() {
        for kind in ["session_info_update", "available_commands_update"] {
            let frame = serde_json::json!({
                "method": "session/update",
                "params": { "sessionId": "sess_0001",
                            "update": { "sessionUpdate": kind } }
            });
            assert!(update_events(&frame, "sess_0001").is_empty(), "{kind}");
        }
    }

    #[test]
    fn a_reply_with_no_stop_reason_is_a_finished_turn_not_an_error() {
        let events = finish_events(&serde_json::json!({}));
        assert_eq!(
            events,
            vec![TurnEvent::Finished {
                stop_reason: STOP_END_TURN.to_string(),
                usage: Usage::default()
            }]
        );
    }
}
