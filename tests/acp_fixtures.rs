//! The committed ACP fixtures are a *replay contract* (spike S3).
//!
//! `tests/fixtures/acp-turn.jsonl` is one real turn against goose 1.50.0,
//! sanitised: nine JSON-RPC frames recorded from the connection-level and
//! session-level SSE streams. These tests pin the parts of that transcript the
//! node agent will read, so a goose upgrade that changes a frame breaks a test
//! here rather than a call in production.
//!
//! They deliberately assert on *shape*, not on token counts or ids: those are
//! per-run and were redacted anyway.

use serde_json::Value;

/// Every frame in the fixture, in recorded order.
fn frames() -> Vec<Value> {
    let raw = include_str!("fixtures/acp-turn.jsonl");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("every fixture line is one JSON-RPC frame"))
        .collect()
}

fn is_update(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("session/update")
}

/// The discriminator the ACP client demultiplexes on.
fn update_kind(frame: &Value) -> &str {
    frame["params"]["update"]["sessionUpdate"]
        .as_str()
        .expect("session/update carries params.update.sessionUpdate")
}

/// A `session/new` reply, returned on the *connection* SSE stream, not as the
/// POST body (spike S3).
#[test]
fn session_new_reply_has_a_session_id_and_modes() {
    let new = frames()
        .into_iter()
        .find(|f| {
            f.get("result")
                .is_some_and(|r| r.get("sessionId").is_some())
        })
        .expect("the fixture contains a session/new reply");

    assert_eq!(new["jsonrpc"], "2.0");
    assert!(new["id"].is_number(), "the reply carries the request id");
    assert!(
        new["result"]["sessionId"].is_string(),
        "sessionId is what the contextId map stores"
    );
    // Modes are what let a caller ask for auto-approval; the agent has to
    // record `currentModeId` so it can reason about what goose will do.
    assert!(new["result"]["modes"]["currentModeId"].is_string());
    assert!(new["result"]["modes"]["availableModes"].is_array());
}

/// The final prompt reply: `stopReason` plus a `usage` block, which is where
/// token counts come from without reading goose's SQLite (spike S3).
#[test]
fn prompt_reply_reports_stop_reason_and_usage() {
    let reply = frames()
        .into_iter()
        .find(|f| {
            f.get("result")
                .is_some_and(|r| r.get("stopReason").is_some())
        })
        .expect("the fixture contains a session/prompt reply");

    assert_eq!(reply["result"]["stopReason"], "end_turn");

    let usage = &reply["result"]["usage"];
    for key in ["totalTokens", "inputTokens", "outputTokens"] {
        assert!(
            usage[key].is_u64(),
            "usage.{key} is a number the agent can meter"
        );
    }
    assert_eq!(
        usage["totalTokens"].as_u64(),
        Some(usage["inputTokens"].as_u64().unwrap() + usage["outputTokens"].as_u64().unwrap()),
        "totalTokens is the sum of its parts, so metering one field is enough"
    );
}

/// Every notification is `session/update` carrying `sessionId` (for routing to
/// the right per-session stream) and a `sessionUpdate` kind.
#[test]
fn every_notification_is_a_routed_session_update() {
    let updates: Vec<Value> = frames().into_iter().filter(is_update).collect();
    assert!(!updates.is_empty(), "the fixture contains notifications");

    for u in &updates {
        assert_eq!(u["jsonrpc"], "2.0");
        assert!(
            u.get("id").is_none(),
            "notifications carry no id: they cannot be correlated to a request"
        );
        assert!(
            u["params"]["sessionId"].is_string(),
            "sessionId is how the agent picks the stream to route this to"
        );
        assert!(!update_kind(u).is_empty());
    }
}

/// The comment stream. `agent_message_chunk` is the only frame the A2A caller
/// actually sees, so its content shape is load-bearing.
#[test]
fn agent_message_chunks_carry_text_content() {
    let chunks: Vec<Value> = frames()
        .into_iter()
        .filter(|f| is_update(f) && update_kind(f) == "agent_message_chunk")
        .collect();
    assert!(
        !chunks.is_empty(),
        "the fixture contains at least one agent message chunk"
    );

    for c in &chunks {
        let content = &c["params"]["update"]["content"];
        assert_eq!(content["type"], "text");
        assert!(
            content["text"].is_string(),
            "text content is what becomes the A2A artifact text"
        );
    }
}

/// `usage_update` gives the agent a running view of the context window, which
/// is the cheap way to notice a runaway turn.
#[test]
fn usage_updates_carry_used_and_size() {
    let usage: Vec<Value> = frames()
        .into_iter()
        .filter(|f| is_update(f) && update_kind(f) == "usage_update")
        .collect();
    assert!(!usage.is_empty());

    for u in &usage {
        let upd = &u["params"]["update"];
        assert!(upd["used"].is_u64(), "used is the current token count");
        assert!(upd["size"].is_u64(), "size is the context window");
    }
}

/// The whole fixture: exactly the nine frames S3 documents, so a change to the
/// transcript is a deliberate edit and not an accident.
#[test]
fn the_transcript_is_the_one_spike_s3_recorded() {
    let all = frames();
    assert_eq!(all.len(), 9, "nine frames, as recorded in spikes/S3.md");

    let kinds: Vec<String> = all
        .iter()
        .filter(|f| is_update(f))
        .map(|f| update_kind(f).to_string())
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
        ],
        "the notification sequence is the contract tests/replay will assert on"
    );
}
