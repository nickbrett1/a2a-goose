//! Answering a hub `request` over the tunnel.
//!
//! The hub sends `request {id, method, params}` and waits for
//! `response {id, ok, body?}`. In M2a the agent answers two methods:
//!
//! - `status.get` → [`crate::server::status_payload`]. This is the one the hub
//!   *polls* (every `status_poll_ms`) to fill the fleet view's in-flight count,
//!   so it is not optional: without it the hub can never report an agent as
//!   stuck, only connected.
//! - `sessions.list` → [`crate::server::sessions_payload`], the same retained-ACP
//!   list `GET /sessions` serves, so the hub and the agent agree on one
//!   definition of "a session".
//!
//! Everything else — `history.*`, `logs.tail`, anything from a newer hub — is
//! refused with an explicit error rather than an empty body. A refusal the hub
//! turns into a `502` is honest; an empty `200` would tell an operator there is
//! nothing to see.
//!
//! The dispatch is behind [`QueryAnswerer`] rather than a direct `&Agent` so the
//! tunnel is testable without a `goose serve`: the tests in `tests/tunnel_e2e.rs`
//! stand up a real WebSocket and a stub answerer, with no ACP anywhere.

use serde_json::Value;

use crate::tunnel::protocol::{RequestFrame, ResponseFrame};

/// The outcome of answering a hub method.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The body for `response.body`.
    Body(Value),
    /// A method this agent knows but cannot answer right now, with the reason.
    Refused(String),
    /// A method this agent does not implement.
    Unsupported,
}

/// Answers hub `request` methods. Implemented by [`crate::server::Agent`]; a
/// stub implements it in tests.
pub trait QueryAnswerer: Send + Sync + 'static {
    /// Never panics and never blocks: the caller is the tunnel's read loop, and a
    /// method that hung it would stall every other frame on the connection.
    fn answer(&self, method: &str, params: &Value) -> Answer;
}

/// Turn a hub `request` into the `response` frame that answers it.
pub fn respond(answerer: &dyn QueryAnswerer, request: &RequestFrame) -> ResponseFrame {
    match answerer.answer(&request.method, &request.params) {
        Answer::Body(body) => ResponseFrame::ok(request.id.clone(), body),
        Answer::Refused(error) => ResponseFrame::refused(request.id.clone(), error),
        Answer::Unsupported => ResponseFrame::refused(
            request.id.clone(),
            format!("unsupported_method: {}", request.method),
        ),
    }
}

/// The `response` to a hub `command`.
///
/// Commands are M4 (reboot). Answering with a refusal, rather than staying
/// silent, keeps the hub's pending future from hanging until its
/// `request_timeout_ms`: the hub learns "this agent does not do that" at once.
/// We advertise no command capability, so a well-behaved hub should never send
/// one; a refusal is the correct answer if one arrives anyway.
pub fn refuse_command(id: &str, action: &str) -> ResponseFrame {
    ResponseFrame::refused(id, format!("unsupported_action: {action}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stub with one canned method, to pin the dispatch without an `Agent`.
    struct Stub;

    impl QueryAnswerer for Stub {
        fn answer(&self, method: &str, params: &Value) -> Answer {
            match method {
                "status.get" => Answer::Body(json!({ "acp": { "inFlight": 0 } })),
                "history.sessions" => Answer::Refused("history is not wired in M2a".to_string()),
                _ => {
                    let _ = params;
                    Answer::Unsupported
                }
            }
        }
    }

    fn request(method: &str) -> RequestFrame {
        RequestFrame {
            id: "r-1".to_string(),
            method: method.to_string(),
            params: json!({}),
        }
    }

    #[test]
    fn a_known_method_answers_with_its_body() {
        let response = respond(&Stub, &request("status.get"));
        assert!(response.ok);
        assert_eq!(response.id, "r-1");
        assert_eq!(response.body.unwrap()["acp"]["inFlight"], 0);
        assert!(response.error.is_none());
    }

    #[test]
    fn a_refusal_carries_the_reason_and_not_a_body() {
        let response = respond(&Stub, &request("history.sessions"));
        assert!(!response.ok);
        assert_eq!(
            response.error.as_deref(),
            Some("history is not wired in M2a")
        );
        assert!(response.body.is_none());
    }

    #[test]
    fn an_unknown_method_names_itself_and_never_hangs_the_hub() {
        let response = respond(&Stub, &request("from_the_future"));
        assert!(!response.ok);
        assert_eq!(
            response.error.as_deref(),
            Some("unsupported_method: from_the_future")
        );
    }

    #[test]
    fn a_command_is_refused_by_name() {
        let response = refuse_command("c-1", "reboot");
        assert!(!response.ok);
        assert_eq!(response.id, "c-1");
        assert_eq!(
            response.error.as_deref(),
            Some("unsupported_action: reboot")
        );
    }
}
