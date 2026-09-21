//! The roost tunnel client: the agent's outbound link to the mission-control hub.
//!
//! `roost` is a **router, not a store**. The hub needs no inbound surface on a
//! host: this agent dials *out* over one long-lived WebSocket and keeps it open,
//! which gives the hub agent→hub push (the live feed) and hub→agent
//! request/response (status, history) on a single connection. The wire is
//! mirrored in [`protocol`]; identity is [`identity`]; answering a hub request is
//! [`answer`].
//!
//! Two properties are non-negotiable, from the mission-control design:
//!
//! - **Fail open.** An unreachable hub is a retry, never a crash. The tunnel is a
//!   separate task; nothing about it is on the agent's serving path, so a hub
//!   that is down, restarting, or pointed at the wrong address costs the agent
//!   nothing but a log line and a reconnect.
//! - **Version skew is normal.** The hub fetches the latest on boot, so a newer
//!   hub or a newer agent is the ordinary state. Unknown *frames* are ignored and
//!   the `activity` payload is **opaque JSON** — we forward the agent's own
//!   envelope and invent no new event schema.
//!
//! The lifecycle of one connection ([`Tunnel::run_once`]):
//!
//! 1. dial the hub (with the credential on the handshake, if one is configured),
//! 2. send `hello` **first** — roost drops a tunnel whose first frame is not one,
//! 3. subscribe to [`ActivityHub`] (backlog first, then live),
//! 4. replay the backlog, then forward live activity and answer hub requests
//!    until the socket drops.
//!
//! On a drop, [`Tunnel::run`] backs off and dials again. The backlog is replayed
//! on every reconnect; the hub drops the overlap by its `(bootId, seq)` floor,
//! which is exactly what that key is for.

pub mod answer;
pub mod identity;
pub mod protocol;

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use crate::activity::{Activity, ActivityHub};
use crate::config::Config;
use crate::server::Agent;

pub use answer::{Answer, QueryAnswerer};
pub use identity::Identity;
pub use protocol::{ActivityFrame, Hello, PROTOCOL_VERSION, ServerFrame};

/// The first reconnect delay. Small: a hub that is up but was mid-restart is
/// back within a second, and the first retry should catch it.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
/// The longest reconnect delay. A hub that is down for maintenance must not be
/// hammered — one attempt every 30 s is plenty to notice it coming back.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Capped exponential reconnect backoff.
///
/// Deterministic, with no jitter: the roost fleet is a handful of hosts, and a
/// testable sequence is worth more than a thundering-herd guard the scale does
/// not need. [`Backoff::reset`] is called as soon as a `hello` is sent, so a
/// connection that stayed up for hours does not resume at the cap after one
/// blip.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    attempts: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            attempts: 0,
        }
    }

    /// Back to the first delay. Called once a connection is established.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// The next delay, advancing the sequence.
    pub fn next_delay(&mut self) -> Duration {
        let delay = Self::delay(self.initial, self.max, self.attempts);
        self.attempts = self.attempts.saturating_add(1);
        delay
    }

    /// The delay for the `attempt`th retry, counted from zero. Pure, so the
    /// sequence can be asserted without sleeping.
    pub fn delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(32)).unwrap_or(u32::MAX);
        let scaled = initial.saturating_mul(factor.max(1));
        scaled.min(max)
    }
}

/// Everything the tunnel needs: where to dial, who it claims to be, and the two
/// streams it serves. Built by [`spawn`] from the running agent, or directly in a
/// test with a stub [`QueryAnswerer`].
pub struct Tunnel {
    identity: Identity,
    url: String,
    credential: Option<String>,
    boot_id: String,
    started_at: String,
    activity: Arc<ActivityHub>,
    answers: Arc<dyn QueryAnswerer>,
    connect_timeout: Duration,
}

impl Tunnel {
    pub fn new(
        identity: Identity,
        url: impl Into<String>,
        credential: Option<String>,
        activity: Arc<ActivityHub>,
        answers: Arc<dyn QueryAnswerer>,
    ) -> Self {
        Self {
            identity,
            url: url.into(),
            credential,
            boot_id: identity::mint_boot_id(),
            started_at: identity::started_at(),
            activity,
            answers,
            connect_timeout: Duration::from_secs(10),
        }
    }

    /// Override the connect timeout, for a test that would rather not wait.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The boot id this tunnel will carry for the life of the process. Exposed so
    /// a test can assert it is stable across reconnects.
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Run until the process ends, reconnecting forever. Never returns an error:
    /// there is no hub condition that should stop this task, and the only way out
    /// is the process exiting.
    pub async fn run(self) {
        let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
        loop {
            match self.run_once(&mut backoff).await {
                Ok(()) => tracing::info!(
                    agent = %self.identity.agent_id,
                    "roost tunnel closed by the hub; reconnecting"
                ),
                Err(error) => tracing::warn!(
                    agent = %self.identity.agent_id,
                    url = %self.url,
                    error = %format!("{error:#}"),
                    "roost tunnel dropped; will retry"
                ),
            }
            tokio::time::sleep(backoff.next_delay()).await;
        }
    }

    /// One connection, from dial to drop.
    ///
    /// `Ok(())` is a clean close (the hub said goodbye, or the activity feed
    /// ended); `Err` is a transport failure. Both lead to a retry; the difference
    /// is only in the log.
    async fn run_once(&self, backoff: &mut Backoff) -> anyhow::Result<()> {
        let request = self.handshake_request()?;
        let (socket, _response) = tokio::time::timeout(
            self.connect_timeout,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out dialling the hub"))??;
        tracing::info!(agent = %self.identity.agent_id, url = %self.url, "roost tunnel connected");

        let (mut sink, mut stream) = socket.split();

        // `hello` must be the first frame: roost drops a tunnel whose first text
        // frame is anything else.
        let hello = self.identity.hello(&self.boot_id, &self.started_at);
        sink.send(Message::Text(hello.to_value().to_string().into()))
            .await?;

        // Established: the next failure starts at the short delay again.
        backoff.reset();

        // Subscribe *after* hello, so no activity frame can precede it. The
        // backlog covers the gap between hello and subscribe, with a possible
        // duplicate at the seam that the hub drops by `seq`.
        let (backlog, mut live) = self.activity.subscribe();
        for activity in backlog {
            let frame = activity_frame(&self.boot_id, &activity);
            sink.send(Message::Text(frame.to_value().to_string().into()))
                .await?;
        }

        loop {
            tokio::select! {
                event = live.recv() => match event {
                    Ok(activity) => {
                        let frame = activity_frame(&self.boot_id, &activity);
                        sink.send(Message::Text(frame.to_value().to_string().into())).await?;
                    }
                    // A slow subscriber missed frames; the hub is a view, not a
                    // record, so the next event corrects it. Never fatal.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "roost tunnel lagged the activity feed");
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                message = stream.next() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    match message? {
                        Message::Text(text) => {
                            if let Some(response) = self.handle_server_frame(text.as_str()) {
                                sink.send(Message::Text(response.to_string().into())).await?;
                            }
                        }
                        Message::Close(_) => return Ok(()),
                        // axum's WebSocket does not ping, but any other hub
                        // might; a pong keeps the connection honest.
                        Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
                        _ => {}
                    }
                }
            }
        }
    }

    /// The WebSocket handshake request, with the credential attached.
    ///
    /// The credential is sent as `Authorization: Bearer <cred>`. roost does not
    /// check it yet — `protocol.rs`'s `Hello` has no credential field and
    /// `server.rs` authenticates nothing — so this is the agent holding up its
    /// end of a boundary the hub has still to build. See the calls in
    /// `docs/m2a-plan.md`.
    fn handshake_request(
        &self,
    ) -> anyhow::Result<tokio_tungstenite::tungstenite::http::Request<()>> {
        let mut request = self.url.as_str().into_client_request()?;
        if let Some(credential) = &self.credential {
            let value = HeaderValue::from_str(&format!("Bearer {credential}"))?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        Ok(request)
    }

    /// Dispatch one hub→agent frame. Returns the `response` frame to send, if the
    /// frame wants one.
    ///
    /// An `Unknown` frame, a non-JSON frame and a malformed known frame are all
    /// logged and skipped: none of them is a reason to drop a working tunnel.
    fn handle_server_frame(&self, text: &str) -> Option<Value> {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            tracing::warn!("roost hub sent a non-JSON frame; ignoring");
            return None;
        };
        let frame = match ServerFrame::parse(&value) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "roost hub sent a malformed frame; ignoring");
                return None;
            }
        };
        match frame {
            ServerFrame::Request(request) => {
                let response = answer::respond(self.answers.as_ref(), &request);
                Some(response.to_value())
            }
            ServerFrame::Command(command) => {
                let response = answer::refuse_command(&command.id, &command.action);
                Some(response.to_value())
            }
            ServerFrame::Unknown { type_tag } => {
                tracing::debug!(type_tag, "ignoring an unknown hub frame type");
                None
            }
        }
    }
}

/// Turn one recorded [`Activity`] into the `activity` frame the hub receives.
///
/// The envelope fields are copy-through: `seq` is the activity feed's own seq,
/// `at` its RFC 3339 stamp, the ids its identity. `event` is the **inner**
/// `ActivityEvent` — the §3.1 body with its `type` tag — serialised and carried
/// whole, so the hub forwards a shape it never decodes.
pub fn activity_frame(boot_id: &str, activity: &Activity) -> ActivityFrame {
    ActivityFrame {
        boot_id: boot_id.to_string(),
        seq: activity.seq,
        at: Some(activity.at.to_rfc3339()),
        context_id: activity.context_id.clone(),
        task_id: activity.task_id.clone(),
        session_id: activity.session_id.clone(),
        skill: activity.skill.clone(),
        event: serde_json::to_value(&activity.event).unwrap_or(Value::Null),
    }
}

impl QueryAnswerer for Agent {
    fn answer(&self, method: &str, _params: &Value) -> Answer {
        match method {
            // The hub polls this for the fleet view's in-flight count.
            "status.get" => Answer::Body(crate::server::status_payload(self)),
            // The same retained-session list `GET /sessions` serves.
            "sessions.list" => Answer::Body(crate::server::sessions_payload(self)),
            // `history.*` and `logs.tail` are M1/M3; refused by name in
            // `answer::respond` so the hub gets a typed error, not an empty body.
            _ => Answer::Unsupported,
        }
    }
}

/// Start the tunnel for a running agent, if a hub is configured.
///
/// Returns `None` — and logs why — when there is nothing to dial: the hub is
/// disabled, its url is empty or not a WebSocket url, or it is enabled with no
/// credential. None of those is fatal: an agent with no hub is exactly the agent
/// this was before M2a.
///
/// The task is deliberately not held: it lives for the process, and a clean
/// shutdown is the process exiting (there is nothing on the hub's side to close
/// and no state to flush — the hub is a view).
pub fn spawn(config: &Config, agent: Arc<Agent>) -> Option<JoinHandle<()>> {
    if !config.hub.enabled {
        tracing::debug!("roost hub disabled; no tunnel");
        return None;
    }
    let url = config.hub.url.trim();
    if url.is_empty() {
        tracing::warn!("hub.enabled is set but hub.url is empty; no tunnel");
        return None;
    }
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        tracing::warn!(url, "hub.url is not a ws:// or wss:// url; no tunnel");
        return None;
    }
    let Some(credential) = config.hub_credential() else {
        tracing::warn!(
            env = %config.hub.credential_env,
            "hub.enabled is set but its credential is not in the environment ({}); \
             refusing to dial without it",
            config.hub.credential_env
        );
        return None;
    };

    let skills = agent.skills.ids().into_iter().map(str::to_string).collect();
    let identity = Identity::from_config(config, &agent.card.name, &agent.card.version, skills);
    tracing::info!(
        agent = %identity.agent_id,
        host = %identity.host,
        url,
        "roost tunnel starting"
    );

    let tunnel = Tunnel::new(
        identity,
        url,
        Some(credential),
        Arc::clone(&agent.activity),
        Arc::clone(&agent) as Arc<dyn QueryAnswerer>,
    )
    .with_connect_timeout(Duration::from_secs(config.hub.connect_timeout_secs.max(1)));
    Some(tokio::spawn(tunnel.run()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    #[test]
    fn the_backoff_is_capped_and_deterministic() {
        let initial = Duration::from_millis(500);
        let max = Duration::from_secs(30);
        let seq: Vec<u64> = (0..8)
            .map(|attempt| Backoff::delay(initial, max, attempt).as_millis() as u64)
            .collect();
        assert_eq!(
            seq,
            vec![500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
        );
        // A pathological attempt count saturates rather than overflowing.
        assert_eq!(Backoff::delay(initial, max, u32::MAX), max);
    }

    #[test]
    fn a_successful_connection_resets_the_backoff() {
        let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    }

    #[test]
    fn an_activity_becomes_an_opaque_frame_with_the_boot_envelope() {
        let activity = Activity {
            seq: 17,
            at: Utc.with_ymd_and_hms(2026, 9, 21, 12, 0, 0).unwrap(),
            context_id: Some("ctx-1".to_string()),
            task_id: Some("task-1".to_string()),
            session_id: Some("sess-1".to_string()),
            skill: Some("ask".to_string()),
            event: crate::activity::ActivityEvent::ToolCall {
                id: "call_1".to_string(),
                title: Some("Read src/lib.rs".to_string()),
                tool_kind: Some("read".to_string()),
                status: Some("in_progress".to_string()),
            },
        };
        let frame = activity_frame("boot-7", &activity);
        assert_eq!(frame.boot_id, "boot-7");
        assert_eq!(frame.seq, 17);
        assert_eq!(frame.context_id.as_deref(), Some("ctx-1"));
        assert_eq!(frame.event_type(), Some("tool_call"));
        // The event is the inner envelope, whole and undecoded.
        assert_eq!(frame.event["title"], "Read src/lib.rs");

        let sent = frame.to_value();
        assert_eq!(sent["type"], "activity");
        assert_eq!(sent["bootId"], "boot-7");
        assert_eq!(sent["seq"], 17);
        assert_eq!(sent["at"], "2026-09-21T12:00:00+00:00");
        assert_eq!(sent["event"]["type"], "tool_call");
    }

    /// The tunnel's read loop, without a socket: a stub answerer pins the
    /// dispatch, and the real `Agent` impl is exercised in `tests/tunnel_e2e.rs`.
    struct Stub;
    impl QueryAnswerer for Stub {
        fn answer(&self, method: &str, _params: &Value) -> Answer {
            match method {
                "status.get" => Answer::Body(json!({ "acp": { "inFlight": 3 } })),
                _ => Answer::Unsupported,
            }
        }
    }

    fn tunnel() -> Tunnel {
        let identity = Identity {
            agent_id: "a".to_string(),
            host: "h".to_string(),
            kind: "a2a-goose".to_string(),
            agent_version: "0.1.0".to_string(),
            skills: vec![],
            capabilities: vec![],
        };
        Tunnel::new(
            identity,
            "ws://127.0.0.1:1/agent/ws",
            None,
            Arc::new(ActivityHub::disabled()),
            Arc::new(Stub),
        )
    }

    #[test]
    fn a_request_is_answered_and_its_body_carried() {
        let response = tunnel()
            .handle_server_frame(r#"{"type":"request","id":"r-1","method":"status.get"}"#)
            .expect("a response");
        assert_eq!(response["type"], "response");
        assert_eq!(response["id"], "r-1");
        assert_eq!(response["ok"], true);
        assert_eq!(response["body"]["acp"]["inFlight"], 3);
    }

    #[test]
    fn an_unsupported_request_and_a_command_are_both_refused() {
        let unsupported = tunnel()
            .handle_server_frame(r#"{"type":"request","id":"r-2","method":"history.sessions"}"#)
            .unwrap();
        assert_eq!(unsupported["ok"], false);
        assert_eq!(unsupported["error"], "unsupported_method: history.sessions");

        let command = tunnel()
            .handle_server_frame(r#"{"type":"command","id":"c-1","action":"reboot"}"#)
            .unwrap();
        assert_eq!(command["ok"], false);
        assert_eq!(command["error"], "unsupported_action: reboot");
    }

    #[test]
    fn unknown_and_malformed_frames_are_ignored_never_fatal() {
        assert!(
            tunnel()
                .handle_server_frame(r#"{"type":"subscribe_ack"}"#)
                .is_none()
        );
        assert!(tunnel().handle_server_frame("not json at all").is_none());
        // A `request` missing its id is malformed and skipped, not a panic.
        assert!(
            tunnel()
                .handle_server_frame(r#"{"type":"request","method":"status.get"}"#)
                .is_none()
        );
    }

    #[test]
    fn the_boot_id_is_minted_once_and_kept_for_the_life_of_the_tunnel() {
        let tunnel = tunnel();
        let boot_id = tunnel.boot_id().to_string();
        assert_eq!(boot_id.len(), 36);
        assert_eq!(tunnel.boot_id(), boot_id);
    }

    #[test]
    fn a_credential_is_sent_as_a_bearer_header() {
        let mut tunnel = tunnel();
        tunnel.credential = Some("s3cret".to_string());
        let request = tunnel.handshake_request().expect("a request");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer s3cret"
        );
        // With no credential there is no header to leak.
        tunnel.credential = None;
        let bare = tunnel.handshake_request().expect("a request");
        assert!(bare.headers().get(AUTHORIZATION).is_none());
    }
}
