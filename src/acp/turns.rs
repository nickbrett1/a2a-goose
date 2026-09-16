//! [`Turns`] over ACP: one `goose serve` connection, one session per turn.
//!
//! This is the only place that turns [`crate::turn`]'s vocabulary into ACP
//! method calls. Three decisions are worth reading before the code:
//!
//! - **The connection is shared and long-lived; the session is not.** [S4]
//!   proved goose multiplexes concurrent sessions on one connection, so
//!   connecting is amortised and the demultiplexer in [`crate::acp::transport`]
//!   does the routing. A *session* is opened per A2A task in M1: `contextId` →
//!   `sessionId` reuse is a separate feature (`goose.sessions.reuseByContext`)
//!   and doing it here would mean owning an eviction policy, which is not this
//!   module's business. The cost of not doing it is compute, not correctness —
//!   every task still gets a real goose session with a real `cwd`.
//! - **A dead connection is dropped, not repaired in place.** If a request on a
//!   cached connection fails at the transport layer, the cache is cleared, so
//!   the next turn opens a fresh connection with its own `initialize`. A
//!   half-open connection that answers nothing would otherwise poison every
//!   subsequent turn — and `goose serve` restarting under a supervised process
//!   is a normal event, not an exceptional one.
//! - **The wall-clock bound lives here, not at the A2A edge.** A bound checked
//!   between events cannot fire on the turn that has gone quiet, which is the
//!   only kind of turn that needs one.
//!
//! [S4]: ../../spikes/S4.md

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::acp::client::{AcpClient, Session};
use crate::config::Config;
use crate::turn::{
    ContextUsage, Limit, TurnError, TurnEvent, TurnHealth, TurnRequest, Turns, Usage,
};

/// How many A2A frames may be buffered for a caller that is reading slowly.
/// The same trade as the transport's: a bounded buffer that drops the *turn*
/// (rather than the frames) once it is full, because an unbounded one is a
/// memory leak with a friendly name.
const EVENT_BUFFER: usize = 256;

/// The `stopReason` goose sends when a turn ran to completion.
pub const STOP_END_TURN: &str = "end_turn";

/// Turns over ACP.
pub struct AcpTurns {
    config: Arc<Config>,
    connection: Arc<Connection>,
    permits: Arc<Semaphore>,
    capacity: usize,
}

/// The connection shared by every turn, and the flag that lets `/status` report
/// on it without taking a lock (`/status` must never be the thing that blocks).
#[derive(Default)]
struct Connection {
    client: Mutex<Option<Arc<AcpClient>>>,
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
    let mut session = match client.new_session(&request.cwd).await {
        Ok(session) => session,
        Err(err) => {
            // Do not hand the next turn the connection that just failed.
            let mut slot = connection.client.lock().await;
            *slot = None;
            connection.live.store(false, Ordering::Relaxed);
            return Err(TurnError::Transport(err.to_string()));
        }
    };

    let outcome =
        tokio::time::timeout(request.wall_clock, pump(&mut session, &request, events)).await;

    // Best effort, and after the outcome is decided, so a session that cannot be
    // closed does not turn a completed turn into a failed one.
    session.close().await;

    match outcome {
        Ok(outcome) => outcome,
        Err(_) => Err(TurnError::Limit {
            limit: Limit::WallClock,
            allowed: request.wall_clock.as_secs(),
            observed: request.wall_clock.as_secs(),
        }),
    }
}

/// Reads a live session's updates while awaiting the prompt's reply.
async fn pump(
    session: &mut Session,
    request: &TurnRequest,
    events: &mpsc::Sender<Result<TurnEvent, TurnError>>,
) -> Result<(), TurnError> {
    let mut updates = session.take_updates().ok_or_else(|| {
        TurnError::Transport("this session's update stream was already taken".to_string())
    })?;

    // Started *after* the updates are in hand, and this ordering is the reason
    // `Session::take_updates` exists: a reply that arrives on a stream that is
    // not being read is a reply that never arrives.
    let prompt = session.prompt(&request.prompt);
    futures::pin_mut!(prompt);

    let mut stream_open = true;
    loop {
        tokio::select! {
            // Biased so a reply that is already waiting is never delayed behind
            // a burst of notifications.
            biased;
            reply = &mut prompt => {
                let reply = reply.map_err(|err| TurnError::Transport(err.to_string()))?;
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
) -> Result<(), TurnError> {
    events.send(Ok(event)).await.map_err(|_| {
        TurnError::Transport("the caller disconnected while the turn was running".to_string())
    })
}

async fn connect_or_reuse(
    config: &Config,
    connection: &Connection,
) -> Result<Arc<AcpClient>, TurnError> {
    let mut slot = connection.client.lock().await;
    if let Some(client) = slot.as_ref() {
        return Ok(Arc::clone(client));
    }
    // Held across the connect on purpose: two turns arriving on a cold cache
    // should produce one connection, not two racing `initialize`s.
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
