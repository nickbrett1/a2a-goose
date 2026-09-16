//! The ACP hop: one HTTP connection to `goose serve`, demultiplexed.
//!
//! [S3](../../spikes/S3.md) corrected the plan here, and every rule below is
//! measured rather than assumed:
//!
//! - **Requests are POSTs to `/acp`; replies are not the POST body.** A 202 with
//!   an empty body is *success* — the JSON-RPC reply arrives on an SSE stream.
//! - **Every POST after `initialize` needs `Acp-Connection-Id`.** The id comes
//!   back as a response *header* on `initialize`, so that call must happen first
//!   and its header be held for the lifetime of the connection.
//! - **Two stream scopes.** `session/new`'s reply lands on the *connection*-level
//!   stream; a session's notifications land on a *session*-level stream selected
//!   by `Acp-Session-Id`, which must be **opened before** the session-scoped
//!   request is sent or the reply races past a stream that does not exist yet.
//! - **One connection, many sessions.** [S4](../../spikes/S4.md) proved goose
//!   multiplexes concurrent sessions on one connection, so the agent needs a
//!   demultiplexer, not a process per context.
//!
//! The demultiplexer is deliberately *not* scope-strict about replies: a reply is
//! routed by its JSON-RPC `id` to whoever is waiting, whichever stream carried it.
//! Being strict would mean betting on which stream goose uses for a
//! session-scoped reply, and the bet has no upside — the id is unique across the
//! connection either way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Where goose serves the ACP transport on a `goose serve` process.
pub const ACP_PATH: &str = "/acp";

/// goose's ACP credential, sent on every request when this process holds one.
///
/// The value never comes from config — `goose.acp.secretEnv` names the variable
/// that holds it, and [`crate::acp::client::secret_key`] is the only thing that
/// reads it.
pub const SECRET_HEADER: &str = "X-Secret-Key";

/// Turns a configured `goose.acp.url` into the one endpoint every request dials.
///
/// The field is written both ways in the wild: as the endpoint
/// (`http://127.0.0.1:3284/acp` — what the example config and [S1] show) and as
/// the origin (`http://127.0.0.1:3284`). Appending the path blindly turned the
/// first spelling into `/acp/acp`, so every turn was answered `404` and the only
/// thing that worked was the spelling the shipped config did *not* use
/// (mac-studio, 2026-09-16). Normalising here means both dial the same endpoint:
/// a trailing slash is not a different server, and a URL that already carries
/// the path is not asking for it twice.
///
/// A path *prefix* survives, so a reverse proxy in front of goose
/// (`https://host/goose/acp`) is spelled and dialled the same way.
///
/// [S1]: ../../spikes/S1.md
pub fn acp_endpoint(url: &str) -> String {
    let base = url.trim_end_matches('/');
    let base = base.strip_suffix(ACP_PATH).unwrap_or(base);
    format!("{base}{ACP_PATH}")
}

/// Carries the connection id: a response header on `initialize`, a request header
/// on everything afterwards.
pub const CONNECTION_ID_HEADER: &str = "Acp-Connection-Id";

/// Selects the session-level SSE stream.
pub const SESSION_ID_HEADER: &str = "Acp-Session-Id";

/// How many session updates may be buffered before the demultiplexer starts
/// dropping them.
///
/// Dropping is the deliberate choice over blocking, and it is a correctness
/// choice rather than a tuning one: the reader that would block is the same task
/// that must deliver the `session/prompt` reply, so a full channel that the
/// consumer is not draining would deadlock the turn it is trying to finish. A
/// turn that produces more than this many frames between prompt and reply is a
/// bug or a runaway, and losing the tail of it loudly beats hanging.
const UPDATE_BUFFER: usize = 4096;

/// Which stream a request's activity belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `initialize`, `session/new` — replies on the connection-level stream.
    Connection,
    /// `session/prompt`, `session/close` — plus that session's notifications.
    Session(String),
}

#[derive(Debug)]
pub enum AcpError {
    /// Could not reach `goose serve` at all.
    Request(reqwest::Error),
    /// goose answered a request with a non-success status.
    Status { status: u16, body: String },
    /// The POST was accepted but the reply never arrived on a stream.
    Timeout { method: String, secs: u64 },
    /// A JSON-RPC error reply.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// `initialize` did not hand back a connection id, so no later call can work.
    NoConnectionId,
    /// The transport has been shut down.
    Closed,
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(err) => write!(f, "could not reach goose's ACP transport: {err}"),
            Self::Status { status, body } => write!(f, "goose ACP answered {status}: {body}"),
            Self::Timeout { method, secs } => write!(
                f,
                "goose accepted {method} but sent no reply within {secs}s"
            ),
            Self::Rpc {
                code,
                message,
                data,
            } => match data {
                Some(data) => write!(f, "goose refused the request ({code}): {message} ({data})"),
                None => write!(f, "goose refused the request ({code}): {message}"),
            },
            Self::NoConnectionId => write!(
                f,
                "goose's initialize reply carried no {CONNECTION_ID_HEADER} header, so no \
                 further call on this connection can be identified"
            ),
            Self::Closed => write!(f, "the ACP transport has been shut down"),
        }
    }
}

impl AcpError {
    /// Whether this error means the *connection* is gone, as opposed to one
    /// request having gone wrong on a connection that is still there.
    ///
    /// The distinction decides whether the agent forgets the connection and
    /// every session on it (see [`crate::acp::pool`]). Both ways of getting it
    /// wrong have a cost, and they are not symmetric: calling a live connection
    /// dead costs a reconnect and one fresh session per context, which is always
    /// correct and merely wasteful; calling a dead one live hands a half-open
    /// connection to every later turn, and every one of them fails against a
    /// server that will never answer.
    pub fn is_connection_loss(&self) -> bool {
        match self {
            // No HTTP at all: could not reach goose, or the stream under us was
            // shut down.
            Self::Request(_) | Self::Closed | Self::NoConnectionId => true,
            // goose answered and refused, so the connection carried a reply and
            // is therefore alive.
            Self::Status { .. } | Self::Rpc { .. } => false,
            // Accepted but silent. The connection is open — a hung turn is a
            // fact about the turn, and dropping the connection would not have
            // made this one succeed while discarding every other context's
            // session on it.
            Self::Timeout { .. } => false,
        }
    }
}

impl std::error::Error for AcpError {}

impl From<reqwest::Error> for AcpError {
    fn from(err: reqwest::Error) -> Self {
        AcpError::Request(err)
    }
}

/// Where a frame goes once it has been read off a stream.
///
/// Split from the reader so the routing rules — the part with the interesting
/// edge cases — are tested by replaying the committed frames rather than by
/// standing up a `goose serve` in CI.
#[derive(Default)]
pub struct Dispatcher {
    waiting: Mutex<HashMap<i64, oneshot::Sender<Result<Value, AcpError>>>>,
    sessions: Mutex<HashMap<String, mpsc::Sender<Value>>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers interest in a reply. The reader takes the sender out again, so
    /// exactly one frame can resolve it.
    fn expect(&self, id: i64) -> oneshot::Receiver<Result<Value, AcpError>> {
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .expect("dispatcher maps are never held across an await")
            .insert(id, tx);
        rx
    }

    /// Stops waiting on an id, for a request that timed out or was abandoned.
    fn forget(&self, id: i64) {
        self.waiting
            .lock()
            .expect("dispatcher maps are never held across an await")
            .remove(&id);
    }

    fn subscribe(&self, session_id: &str) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(UPDATE_BUFFER);
        self.sessions
            .lock()
            .expect("dispatcher maps are never held across an await")
            .insert(session_id.to_string(), tx);
        rx
    }

    fn unsubscribe(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("dispatcher maps are never held across an await")
            .remove(session_id);
    }

    fn pending(&self) -> usize {
        self.waiting
            .lock()
            .expect("dispatcher maps are never held across an await")
            .len()
    }

    /// Routes one frame. Returns the session id a notification was delivered to,
    /// which is what the tests assert on.
    pub fn dispatch(&self, frame: &Value) -> Option<String> {
        if let Some(id) = frame.get("id").and_then(Value::as_i64) {
            let sender = self
                .waiting
                .lock()
                .expect("dispatcher maps are never held across an await")
                .remove(&id);
            let outcome = match frame.get("error") {
                Some(error) => Err(AcpError::Rpc {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message")
                        .to_string(),
                    data: error.get("data").cloned(),
                }),
                None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
            };
            if let Some(sender) = sender {
                // A send failure means the waiter went away, which is a timeout
                // or a cancelled turn, not a transport problem.
                let _ = sender.send(outcome);
            }
            return None;
        }

        // No id: a notification. Only `session/update` is routed, and only to a
        // session someone is listening for; goose's other notifications (if it
        // grows any) are not this project's business.
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            return None;
        }
        let session_id = frame
            .pointer("/params/sessionId")
            .and_then(Value::as_str)?
            .to_string();

        if let Some(sender) = self
            .sessions
            .lock()
            .expect("dispatcher maps are never held across an await")
            .get(&session_id)
            && sender.try_send(frame.clone()).is_err()
        {
            tracing::warn!(
                %session_id,
                "dropped a session update: the consumer is not draining, and blocking here \
                 would stall the reply this turn is waiting for"
            );
        }
        Some(session_id)
    }
}

/// Pulls complete SSE events out of a buffer, leaving any partial trailing event
/// in place.
///
/// Hand-rolled because the ACP framing this project needs is `data:` lines and
/// nothing else: no `event:` names, no `id:` resumption, no retry. It is
/// deliberately small and directly tested, and it tolerates CRLF because the
/// difference between a stream that works and one that silently yields nothing
/// is not worth discovering in production.
pub fn take_events(buffer: &mut String) -> Vec<String> {
    // A stream may use CRLF; normalise before searching, or a frame delimited by
    // "\r\n\r\n" is never found and the stream silently yields nothing.
    if buffer.contains("\r\n") {
        *buffer = buffer.replace("\r\n", "\n");
    }

    let mut events = Vec::new();
    while let Some(end) = buffer.find("\n\n") {
        let block: String = buffer.drain(..end + 2).collect();
        let payload = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| data.strip_prefix(' ').unwrap_or(data))
            .collect::<Vec<_>>()
            .join("\n");
        if !payload.is_empty() {
            events.push(payload);
        }
    }
    events
}

/// Attaches goose's credential when this process holds one.
///
/// One place rather than three, because every request on the connection needs it
/// — `initialize` included — and a missing header on exactly one of them is a
/// 401 that only shows up on the path that forgot.
fn with_secret(request: reqwest::RequestBuilder, secret: Option<&str>) -> reqwest::RequestBuilder {
    match secret {
        Some(secret) => request.header(SECRET_HEADER, secret),
        None => request,
    }
}

/// The ACP connection: one POST endpoint, one connection-level stream, and a
/// per-session stream opened on demand.
pub struct Transport {
    endpoint: String,
    secret: Option<Arc<str>>,
    connection_id: Arc<str>,
    http: reqwest::Client,
    dispatcher: Arc<Dispatcher>,
    next_id: Arc<AtomicI64>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

impl Transport {
    /// Opens the connection: `initialize`, then the connection-level stream.
    ///
    /// The stream is opened *here* rather than lazily because `session/new`'s
    /// reply comes back on it, so a session can never be created on a connection
    /// whose stream is not already being read.
    ///
    /// `url` is `goose.acp.url` and is normalised by [`acp_endpoint`]. `secret`
    /// is the value named by `goose.acp.secretEnv`, or `None` when this process
    /// does not have it — which is not an error here, because whether goose
    /// wants a key is goose's business, and the 401 it answers says so plainly.
    pub async fn connect(
        url: &str,
        secret: Option<&str>,
        initialize_timeout: Duration,
    ) -> Result<Self, AcpError> {
        let http = reqwest::Client::new();
        let endpoint = acp_endpoint(url);

        let response = with_secret(http.post(&endpoint), secret)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": { "protocolVersion": 1, "clientCapabilities": {} },
            }))
            .timeout(initialize_timeout)
            .send()
            .await?;

        let status = response.status();
        let connection_id = response
            .headers()
            .get(CONNECTION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await?;
        if !status.is_success() {
            return Err(AcpError::Status {
                status: status.as_u16(),
                body: text,
            });
        }
        // `initialize` is the one synchronous call: it answers in its body, and
        // the header it carries is the whole point of making it first.
        let connection_id = connection_id.ok_or(AcpError::NoConnectionId)?;

        let transport = Self {
            endpoint,
            secret: secret.map(Arc::from),
            connection_id: Arc::from(connection_id.as_str()),
            http,
            dispatcher: Arc::new(Dispatcher::new()),
            next_id: Arc::new(AtomicI64::new(1)),
            readers: Mutex::new(Vec::new()),
        };
        transport.open_stream(Scope::Connection).await?;
        Ok(transport)
    }

    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Starts reading a stream. Safe to call twice for the same scope; the
    /// caller owns avoiding that.
    async fn open_stream(&self, scope: Scope) -> Result<(), AcpError> {
        let mut request = with_secret(self.http.get(&self.endpoint), self.secret.as_deref())
            .header(CONNECTION_ID_HEADER, self.connection_id.as_ref())
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Scope::Session(session_id) = &scope {
            request = request.header(SESSION_ID_HEADER, session_id);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AcpError::Status {
                status: status.as_u16(),
                body,
            });
        }

        let dispatcher = Arc::clone(&self.dispatcher);
        let handle = tokio::spawn(async move {
            pump(response.bytes_stream(), dispatcher).await;
        });
        self.readers
            .lock()
            .expect("the reader list is never held across an await")
            .push(handle);
        Ok(())
    }

    /// Issues a request and awaits its reply from whichever stream carries it.
    pub async fn request(
        &self,
        scope: &Scope,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let reply = self.dispatcher.expect(id);

        let mut request = with_secret(self.http.post(&self.endpoint), self.secret.as_deref())
            .header(CONNECTION_ID_HEADER, self.connection_id.as_ref())
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }));
        if let Scope::Session(session_id) = scope {
            request = request.header(SESSION_ID_HEADER, session_id);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            self.dispatcher.forget(id);
            return Err(AcpError::Status {
                status: status.as_u16(),
                body,
            });
        }
        // The body is deliberately not read: a 202 with an empty body is how
        // goose says "accepted, watch the stream", and the reply is never here.

        match tokio::time::timeout(timeout, reply).await {
            Ok(Ok(outcome)) => outcome,
            // The sender was dropped, which now only happens on shutdown.
            Ok(Err(_)) => {
                self.dispatcher.forget(id);
                Err(AcpError::Closed)
            }
            Err(_) => {
                // Someone has to stop waiting, or a timed-out turn would be
                // resolved by a late reply meant for nobody.
                self.dispatcher.forget(id);
                Err(AcpError::Timeout {
                    method: method.to_string(),
                    secs: timeout.as_secs(),
                })
            }
        }
    }

    /// Opens a session-scoped stream and returns the updates it will carry.
    ///
    /// Must be called *before* the first session-scoped request: the reply races
    /// past a stream that is not open yet, and it never comes back (S3).
    pub async fn subscribe(&self, session_id: &str) -> Result<mpsc::Receiver<Value>, AcpError> {
        let updates = self.dispatcher.subscribe(session_id);
        self.open_stream(Scope::Session(session_id.to_string()))
            .await?;
        Ok(updates)
    }

    pub fn unsubscribe(&self, session_id: &str) {
        self.dispatcher.unsubscribe(session_id);
    }

    /// How many requests are still waiting for a reply. `/status`'s leak check.
    pub fn in_flight(&self) -> usize {
        self.dispatcher.pending()
    }

    /// Stops reading. The tasks own nothing the caller needs, so aborting is
    /// enough; `goose serve` forgets the connection when the last stream closes.
    pub fn shutdown(&self) {
        for handle in self
            .readers
            .lock()
            .expect("the reader list is never held across an await")
            .drain(..)
        {
            handle.abort();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reads a byte stream to its end, dispatching every complete event.
async fn pump<S, C, E>(stream: S, dispatcher: Arc<Dispatcher>)
where
    S: futures::Stream<Item = Result<C, E>>,
    C: AsRef<[u8]>,
    E: std::fmt::Display,
{
    futures::pin_mut!(stream);
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                buffer.push_str(&String::from_utf8_lossy(bytes.as_ref()));
                for event in take_events(&mut buffer) {
                    match serde_json::from_str::<Value>(&event) {
                        Ok(frame) => {
                            dispatcher.dispatch(&frame);
                        }
                        // A frame that is not JSON is a goose-side bug or a
                        // keep-alive; either way it is not a reply, and
                        // inventing an error for it would fail turns that are
                        // perfectly fine.
                        Err(err) => tracing::debug!(%err, "skipping a non-JSON ACP frame"),
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "ACP stream ended with an error");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames S3 recorded, verbatim, replayable.
    const TURN: &str = include_str!("../../tests/fixtures/acp-turn.jsonl");

    fn frames() -> Vec<Value> {
        TURN.lines()
            .map(|line| serde_json::from_str(line).expect("fixture is JSONL"))
            .collect()
    }

    fn events(text: &str) -> Vec<Value> {
        let mut buffer = text.to_string();
        take_events(&mut buffer)
            .into_iter()
            .map(|event| serde_json::from_str(&event).expect("event is JSON"))
            .collect()
    }

    #[test]
    fn events_are_split_on_the_blank_line_and_carry_their_data_lines() {
        assert_eq!(events("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n").len(), 2);
        assert_eq!(events("data: {\"a\":1}\n\n")[0]["a"], 1);
        // Event names and comments are not this project's business, but they
        // must not be mistaken for payload.
        assert_eq!(
            events("event: message\n: keep-alive\ndata: {}\n\n").len(),
            1
        );
        // Multi-line data is joined with newlines, per the SSE spec.
        assert_eq!(events("data: {\"a\":\ndata: 1}\n\n")[0]["a"], 1);
        // CRLF streams are found too.
        assert_eq!(events("data: {\"a\":1}\r\n\r\n").len(), 1);
    }

    #[test]
    fn a_partial_event_stays_in_the_buffer_rather_than_being_parsed_early() {
        let mut buffer = String::from("data: {\"a\":");
        assert!(take_events(&mut buffer).is_empty());
        // The rest of the frame arrives; the id must now resolve.
        buffer.push_str("1}\n\n");
        assert_eq!(take_events(&mut buffer).len(), 1);
        assert!(buffer.is_empty(), "a consumed event leaves nothing behind");
    }

    #[tokio::test]
    async fn the_recorded_turn_routes_every_frame_to_the_right_place() {
        // This is the fixture S3 committed, replayed: the id-1 reply is
        // `session/new`, the seven `session/update`s belong to `sess_0001`, and
        // the id-2 reply is the prompt's `end_turn`.
        let dispatcher = Dispatcher::new();
        let new_session = dispatcher.expect(1);
        let prompt = dispatcher.expect(2);
        let mut updates = dispatcher.subscribe("sess_0001");

        for frame in frames() {
            dispatcher.dispatch(&frame);
        }

        let created = new_session.await.expect("id 1 resolves").expect("ok");
        assert_eq!(created["sessionId"], "sess_0001");
        assert_eq!(created["modes"]["currentModeId"], "auto");

        let ended = prompt.await.expect("id 2 resolves").expect("ok");
        assert_eq!(ended["stopReason"], "end_turn");
        assert_eq!(ended["usage"]["totalTokens"], 33999);

        assert_eq!(dispatcher.pending(), 0, "no waiter is left behind");

        let mut received = Vec::new();
        while let Ok(update) = updates.try_recv() {
            received.push(update);
        }
        assert_eq!(
            received.len(),
            7,
            "every session/update is routed and nothing else is"
        );
        let kinds: Vec<&str> = received
            .iter()
            .map(|update| {
                update["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "usage_update",
                "available_commands_update",
                "session_info_update",
                "agent_message_chunk",
                "session_info_update",
                "usage_update",
                "session_info_update",
            ]
        );
        // The answer text is in there, which is the whole reason to route them.
        assert_eq!(
            received[3]["params"]["update"]["content"]["text"], "ok",
            "agent_message_chunk carries the turn's output"
        );
    }

    #[test]
    fn a_configured_url_is_dialled_the_same_however_it_is_spelled() {
        // The two spellings that actually exist: the example config and the
        // spike notes write the endpoint, the mac-studio host wrote the origin.
        // Blindly appending the path turned the first into `/acp/acp` and 404ed
        // every turn, so this is the bug in one assertion.
        assert_eq!(
            acp_endpoint("http://127.0.0.1:3284/acp"),
            "http://127.0.0.1:3284/acp"
        );
        assert_eq!(
            acp_endpoint("http://127.0.0.1:3284"),
            "http://127.0.0.1:3284/acp"
        );
        // A trailing slash is not a different server.
        assert_eq!(
            acp_endpoint("http://127.0.0.1:3284/"),
            "http://127.0.0.1:3284/acp"
        );
        assert_eq!(
            acp_endpoint("http://127.0.0.1:3284/acp/"),
            "http://127.0.0.1:3284/acp"
        );
        // A path prefix survives, so a reverse proxy in front of goose is
        // spelled once and dialled as written.
        assert_eq!(
            acp_endpoint("https://host.example/goose/acp"),
            "https://host.example/goose/acp"
        );
        assert_eq!(
            acp_endpoint("https://host.example/goose"),
            "https://host.example/goose/acp"
        );
    }

    #[test]
    fn only_an_error_that_means_no_http_counts_as_losing_the_connection() {
        // An answer of any kind proves the connection is alive, however
        // unwelcome the answer is. Anything else means the agent is holding a
        // connection that will not carry another turn, and it must forget it.
        assert!(
            !AcpError::Status {
                status: 500,
                body: "boom".to_string()
            }
            .is_connection_loss()
        );
        assert!(
            !AcpError::Rpc {
                code: -32602,
                message: "Invalid params".to_string(),
                data: None
            }
            .is_connection_loss()
        );
        assert!(
            !AcpError::Timeout {
                method: "session/prompt".to_string(),
                secs: 900
            }
            .is_connection_loss()
        );
        assert!(AcpError::Closed.is_connection_loss());
        assert!(AcpError::NoConnectionId.is_connection_loss());
    }

    #[test]
    fn an_error_reply_becomes_an_error_rather_than_a_null_result() {
        let dispatcher = Dispatcher::new();
        let mut waiting = dispatcher.expect(7);
        dispatcher.dispatch(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": { "code": -32602, "message": "Invalid params",
                       "data": { "error": "invalid directory path" } }
        }));

        match waiting.try_recv().expect("resolved") {
            Err(AcpError::Rpc {
                code, ref message, ..
            }) => {
                assert_eq!(code, -32602);
                assert!(message.contains("Invalid params"));
            }
            other => panic!("expected an RPC error, got {other:?}"),
        }
        assert_eq!(dispatcher.pending(), 0);
    }

    #[test]
    fn an_update_for_a_session_nobody_is_listening_for_is_dropped_quietly() {
        let dispatcher = Dispatcher::new();
        let routed = dispatcher.dispatch(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": "sess_other",
                        "update": { "sessionUpdate": "usage_update" } }
        }));
        assert_eq!(routed, Some("sess_other".to_string()));
        // Nothing panicked and nothing was buffered for a later subscriber.
        assert!(dispatcher.subscribe("sess_other").try_recv().is_err());
    }

    #[test]
    fn a_reply_for_an_id_nobody_is_waiting_for_is_not_a_transport_failure() {
        // The case this covers: a turn timed out, its waiter was forgotten, and
        // the reply arrived afterwards. It must not panic or be mistaken for
        // another request's answer.
        let dispatcher = Dispatcher::new();
        assert_eq!(
            dispatcher.dispatch(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
            None
        );
        assert_eq!(dispatcher.pending(), 0);
    }

    #[test]
    fn another_method_is_not_routed_as_a_session_update() {
        let dispatcher = Dispatcher::new();
        let _updates = dispatcher.subscribe("sess_0001");
        assert_eq!(
            dispatcher.dispatch(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "something/else",
                "params": { "sessionId": "sess_0001" }
            })),
            None
        );
        let _ = _updates;
    }

    #[tokio::test]
    async fn a_frame_split_across_two_reads_is_still_dispatched_once() {
        // A real SSE stream does not respect JSON boundaries. The reader must
        // reassemble, and must not dispatch the same frame twice.
        let dispatcher = Arc::new(Dispatcher::new());
        let waiting = dispatcher.expect(1);

        let chunks: Vec<Result<String, std::io::Error>> = vec![
            Ok("data: {\"jsonrpc\":\"2.0\",\"id\":1,".to_string()),
            Ok("\"result\":{\"sessionId\":\"sess_0001\"}}\n".to_string()),
            Ok("\n".to_string()),
        ];
        pump(futures::stream::iter(chunks), Arc::clone(&dispatcher)).await;

        let result = waiting.await.expect("reassembled").expect("ok");
        assert_eq!(result["sessionId"], "sess_0001");
        assert_eq!(dispatcher.pending(), 0);
    }
}
