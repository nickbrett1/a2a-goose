//! The **roost agent↔hub wire**, mirrored from the hub's own source.
//!
//! The spec is `roost/src/protocol.rs` (field names and JSON shape are literal);
//! this module is a copy of the agent's half of it, kept deliberately small:
//! the agent **sends** `hello`, `activity` and `response`, and **receives**
//! `request` and `command`. roost is a hub binary, not a published library, so a
//! git dependency would drag the whole hub — axum, the Svelte bundle — into this
//! agent's build for a hundred lines of types. The trade is that drift is
//! possible; the tests below make it a **test failure** by pinning roost's own
//! `protocol.rs` test vectors verbatim.
//!
//! Two forward-compatibility rules roost bakes in are honoured here too, because
//! version skew is the normal state (the hub fetches latest on boot):
//!
//! - A frame is dispatched on its `type` tag; an unrecognised tag becomes
//!   [`ServerFrame::Unknown`] rather than an error.
//! - The `activity` payload is **opaque JSON** — the hub is a router, not a
//!   decoder, and neither are we: we forward the agent's own §3.1 envelope
//!   verbatim and invent no new event schema.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The wire protocol version carried in [`Hello`].
///
/// This is **roost's** version, not the A2A card's `protocolVersion`
/// (`"0.3"`/`"1.0"`). They are different numbers that unfortunately share a
/// name; see `docs/roost-tunnel.md`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The first frame an agent sends after the tunnel opens.
///
/// `boot_id` is required, not optional: `seq` resets when the agent restarts, so
/// the hub's ordering key is `(agentId, bootId, seq)` and never `(agentId, seq)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub agent_id: String,
    pub host: String,
    pub kind: String,
    pub agent_version: String,
    pub protocol_version: u32,
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl Hello {
    /// The `hello` frame as it goes on the wire: the fields plus the `type` tag.
    pub fn to_value(&self) -> Value {
        tagged("hello", self)
    }
}

/// One published activity frame. The `event` is the agent's own envelope,
/// forwarded verbatim — the hub invents no new event schema, and neither do we.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFrame {
    pub boot_id: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    pub event: Value,
}

impl ActivityFrame {
    /// The inner event's `type` discriminator, if present. The hub reads the
    /// same field; nothing else in `event` is interpreted on either side.
    pub fn event_type(&self) -> Option<&str> {
        self.event.get("type").and_then(Value::as_str)
    }

    pub fn to_value(&self) -> Value {
        tagged("activity", self)
    }
}

/// The answer to a `request` or `command`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseFrame {
    pub id: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ResponseFrame {
    /// A successful answer.
    pub fn ok(id: impl Into<String>, body: Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            body: Some(body),
            error: None,
        }
    }

    /// A refusal. `ok` stays false so the hub's `request` future resolves to an
    /// `Err` rather than a body that is a lie.
    pub fn refused(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            body: None,
            error: Some(error.into()),
        }
    }

    pub fn to_value(&self) -> Value {
        tagged("response", self)
    }
}

/// A `request` the hub sends an agent: `status.get`, `sessions.list`, `history.*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFrame {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A `command` the hub sends an agent to act on the deployment (M4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandFrame {
    pub id: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// `skip_serializing_if` for a `bool` that should be omitted when false — a
/// minimal `command` stays minimal, the shape roost's `protocol.rs` documents.
fn is_false(value: &bool) -> bool {
    !*value
}

/// A hub→agent liveness probe: the hub asks "are you still there?", the agent
/// answers [`pong_value`].
///
/// This is **purely additive**, which is the whole point of the design: a hub
/// that sends `ping` to an agent build that predates this frame is met with
/// [`ServerFrame::Unknown`] and ignored, and the `pong` this agent sends to a
/// hub that predates it is a `ClientFrame::Unknown` on the hub's side and
/// likewise ignored. Neither side can be made to drop a working tunnel by a
/// heartbeat it does not understand.
///
/// `id` is optional so the frame stays minimal on the wire; an agent echoes it
/// back on the `pong` when present, which lets a future hub correlate a probe
/// with its answer. Unknown *fields* (a `sentAt`, say) are ignored, so the hub
/// may attach whatever it likes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl PingFrame {
    /// The `ping` frame as it goes on the wire: the fields plus the `type` tag.
    pub fn to_value(&self) -> Value {
        tagged("ping", self)
    }
}

/// The `pong` answering a [`PingFrame`], as it goes on the wire.
///
/// Built by hand rather than from a struct because it carries nothing but the
/// echoed `id` (when the probe had one); a hub that sent a bare
/// `{"type":"ping"}` gets a bare `{"type":"pong"}` back.
pub fn pong_value(id: Option<&str>) -> Value {
    match id {
        Some(id) => json!({ "type": "pong", "id": id }),
        None => json!({ "type": "pong" }),
    }
}

/// A frame the hub sends an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    Request(Box<RequestFrame>),
    Command(Box<CommandFrame>),
    /// A liveness probe, answered with a `pong`.
    Ping(Box<PingFrame>),
    /// A tag this agent build does not know. Ignored, never fatal.
    Unknown {
        type_tag: String,
    },
}

impl ServerFrame {
    /// Parse a frame, dispatching on its `type` tag.
    ///
    /// A *known* tag that does not parse is an error (roost's own rule: a
    /// malformed `request` must not be silently dropped); an *unknown* tag is
    /// [`ServerFrame::Unknown`] and is the normal state during version skew.
    pub fn parse(value: &Value) -> anyhow::Result<Self> {
        let type_tag = value.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match type_tag {
            "request" => ServerFrame::Request(Box::new(serde_json::from_value(value.clone())?)),
            "command" => ServerFrame::Command(Box::new(serde_json::from_value(value.clone())?)),
            "ping" => ServerFrame::Ping(Box::new(serde_json::from_value(value.clone())?)),
            _ => ServerFrame::Unknown {
                type_tag: type_tag.to_string(),
            },
        })
    }
}

/// Serialise a typed frame and add its `type` discriminator.
fn tagged(kind: &str, value: &impl Serialize) -> Value {
    let mut value = serde_json::to_value(value).expect("a frame is serialisable");
    value
        .as_object_mut()
        .expect("a frame is an object")
        .insert("type".to_string(), Value::String(kind.to_string()));
    value
}

/// A `response` frame answering a hub `request`, built by hand.
///
/// Kept next to [`ResponseFrame`] because the hub's tests build responses the
/// same way; here it is only ever the *value*, so a caller that has a body or an
/// error and nothing else does not have to name the struct.
pub fn response_value(id: &str, body: Value) -> Value {
    json!({ "type": "response", "id": id, "ok": true, "body": body })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The vectors below are roost's own, copied from `roost/src/protocol.rs`
    // `#[cfg(test)] mod tests`. They are the contract; if they stop passing, the
    // two sides have drifted.

    #[test]
    fn hello_round_trips_the_memo_shape() {
        let raw = json!({
            "type": "hello",
            "agentId": "a2a-goose-dev",
            "host": "dev-container-3",
            "kind": "devcontainer",
            "agentVersion": "0.9.1",
            "protocolVersion": 1,
            "bootId": "7f3c1a9e",
            "startedAt": "2026-09-20T02:30:00Z",
            "skills": ["ask"],
            "capabilities": ["activity", "history", "logs", "reboot"],
            "somethingNew": true
        });
        // roost parses it; so must we, and unknown fields must not be fatal.
        let hello: Hello = serde_json::from_value(raw).expect("parses");
        assert_eq!(hello.agent_id, "a2a-goose-dev");
        assert_eq!(hello.boot_id, "7f3c1a9e");
        assert_eq!(hello.skills, vec!["ask"]);
        assert_eq!(
            hello.capabilities,
            vec!["activity", "history", "logs", "reboot"]
        );
        // And the shape we *send* is the shape roost reads.
        let sent = hello.to_value();
        assert_eq!(sent["type"], "hello");
        assert_eq!(sent["agentId"], "a2a-goose-dev");
        assert_eq!(sent["protocolVersion"], 1);
        // Optional fields are omitted when absent, so a minimal hello is minimal.
        let minimal = Hello {
            started_at: None,
            skills: vec![],
            capabilities: vec![],
            ..hello
        }
        .to_value();
        assert!(minimal.get("startedAt").is_none());
        assert_eq!(minimal["skills"], json!([]));
    }

    #[test]
    fn a_hello_without_a_boot_id_is_an_error() {
        // roost: "hello without the required bootId must fail loudly, not
        // silently." The field is required on our side too.
        let raw = json!({ "type": "hello", "agentId": "x" });
        assert!(serde_json::from_value::<Hello>(raw).is_err());
    }

    #[test]
    fn activity_keeps_the_event_opaque() {
        let frame = ActivityFrame {
            boot_id: "7f3c1a9e".to_string(),
            seq: 42,
            at: Some("2026-09-20T02:31:07.512Z".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: None,
            session_id: None,
            skill: None,
            event: json!({ "type": "tool_call", "id": "call_1", "title": "Read src/lib.rs" }),
        };
        assert_eq!(frame.seq, 42);
        assert_eq!(frame.event_type(), Some("tool_call"));
        let sent = frame.to_value();
        assert_eq!(sent["type"], "activity");
        assert_eq!(sent["seq"], 42);
        assert_eq!(sent["contextId"], "ctx-1");
        // The event is forwarded whole, untouched.
        assert_eq!(sent["event"]["title"], "Read src/lib.rs");
        // And it is the shape roost's own test parses.
        let raw = json!({
            "type": "activity",
            "bootId": "7f3c1a9e",
            "seq": 42,
            "at": "2026-09-20T02:31:07.512Z",
            "contextId": "ctx-1",
            "event": { "type": "tool_call", "id": "call_1", "title": "Read src/lib.rs" }
        });
        assert_eq!(
            serde_json::to_value(&frame).unwrap()["contextId"],
            raw["contextId"]
        );
    }

    #[test]
    fn unknown_event_type_is_carried_not_rejected() {
        let frame = ActivityFrame {
            boot_id: "b".to_string(),
            seq: 1,
            at: None,
            context_id: None,
            task_id: None,
            session_id: None,
            skill: None,
            event: json!({ "type": "from_the_future", "payload": 1 }),
        };
        assert_eq!(frame.event_type(), Some("from_the_future"));
        assert_eq!(frame.to_value()["event"]["payload"], 1);
    }

    #[test]
    fn unknown_server_frame_type_is_ignored() {
        let raw = json!({ "type": "subscribe_ack", "whatever": 1 });
        assert_eq!(
            ServerFrame::parse(&raw).expect("parses"),
            ServerFrame::Unknown {
                type_tag: "subscribe_ack".to_string()
            }
        );
    }

    #[test]
    fn a_ping_parses_with_or_without_an_id_and_ignores_extra_fields() {
        // The bare probe an older/newer hub may send.
        assert_eq!(
            ServerFrame::parse(&json!({ "type": "ping" })).expect("parses"),
            ServerFrame::Ping(Box::new(PingFrame { id: None }))
        );
        // An id, echoed back on the pong, plus a field we do not know: the
        // heartbeat must stay additive, so an unknown field is ignored.
        assert_eq!(
            ServerFrame::parse(&json!({ "type": "ping", "id": "p-1", "sentAt": 123 }))
                .expect("parses"),
            ServerFrame::Ping(Box::new(PingFrame {
                id: Some("p-1".to_string())
            }))
        );
    }

    #[test]
    fn a_pong_echoes_the_id_only_when_the_ping_had_one() {
        assert_eq!(pong_value(None), json!({ "type": "pong" }));
        assert_eq!(
            pong_value(Some("p-1")),
            json!({ "type": "pong", "id": "p-1" })
        );
        // A `pong` coming the *other* way (a hub echoing us, a bug) is an
        // unknown server frame, not a fatal one: the additive rule holds for
        // the reply type too.
        assert_eq!(
            ServerFrame::parse(&json!({ "type": "pong" })).expect("parses"),
            ServerFrame::Unknown {
                type_tag: "pong".to_string()
            }
        );
    }

    #[test]
    fn a_malformed_known_server_frame_is_an_error() {
        // A `request` with no id must fail loudly; ignoring it would hang the
        // hub's pending map.
        let raw = json!({ "type": "request", "method": "status.get" });
        assert!(ServerFrame::parse(&raw).is_err());
    }

    #[test]
    fn request_parses_the_memo_shape() {
        let raw = json!({
            "type": "request",
            "id": "r-1",
            "method": "history.sessions",
            "params": { "cwd": "/workspaces/x" }
        });
        let ServerFrame::Request(request) = ServerFrame::parse(&raw).expect("parses") else {
            panic!("expected request");
        };
        assert_eq!(request.id, "r-1");
        assert_eq!(request.method, "history.sessions");
        assert_eq!(request.params["cwd"], "/workspaces/x");
    }

    #[test]
    fn a_request_with_no_params_defaults_to_null() {
        let raw = json!({ "type": "request", "id": "r-1", "method": "status.get" });
        let ServerFrame::Request(request) = ServerFrame::parse(&raw).expect("parses") else {
            panic!("expected request");
        };
        assert!(request.params.is_null());
    }

    #[test]
    fn command_parses_mode_and_force() {
        let raw = json!({
            "type": "command",
            "id": "c-1",
            "action": "reboot",
            "mode": "preflight",
            "force": true
        });
        let ServerFrame::Command(command) = ServerFrame::parse(&raw).expect("parses") else {
            panic!("expected command");
        };
        assert_eq!(command.action, "reboot");
        assert_eq!(command.mode.as_deref(), Some("preflight"));
        assert!(command.force);
    }

    #[test]
    fn responses_serialise_ok_and_refused() {
        assert_eq!(
            ResponseFrame::ok("r-1", json!({ "acp": {} })).to_value(),
            json!({ "type": "response", "id": "r-1", "ok": true, "body": { "acp": {} } })
        );
        let refused = ResponseFrame::refused("r-2", "unsupported").to_value();
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["error"], "unsupported");
        assert!(refused.get("body").is_none());
    }
}
