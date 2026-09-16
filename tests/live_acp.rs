//! The demultiplexer against a **real** `goose serve`.
//!
//! Ignored by default, because it needs a goose the CI runners do not have —
//! the same shape as `tests/skeleton.rs`'s sibling spikes. Run it against a
//! host's goose with:
//!
//! ```console
//! A2A_GOOSE_LIVE_ACP=http://127.0.0.1:3284 cargo test --locked --test live_acp \
//!     -- --ignored --nocapture
//! ```
//!
//! It exists because the replay test in `src/acp/transport.rs` proves the
//! *routing* is right and cannot prove the transport is: the ordering rule S3
//! warns about (subscribe before the session-scoped request) only fails against
//! a server that actually does the racing. That is what this checks.

use std::time::Duration;

use a2a_goose::acp::Transport;
use a2a_goose::acp::transport::Scope;

fn goose_url() -> Option<String> {
    std::env::var("A2A_GOOSE_LIVE_ACP").ok()
}

/// The cwd a live turn runs in. `/tmp` because it exists everywhere and goose
/// validates `cwd` itself (S4), so a missing one would look like a protocol bug.
const CWD: &str = "/tmp";

#[tokio::test]
#[ignore = "needs a live goose serve; set A2A_GOOSE_LIVE_ACP"]
async fn one_connection_demultiplexes_a_real_session() {
    let Some(url) = goose_url() else {
        panic!("set A2A_GOOSE_LIVE_ACP to a running goose serve, e.g. http://127.0.0.1:3284");
    };

    let transport = Transport::connect(&url, None, Duration::from_secs(20))
        .await
        .expect("initialize");
    assert!(
        !transport.connection_id().is_empty(),
        "initialize must hand back an {}-like id or nothing later can be identified",
        a2a_goose::acp::CONNECTION_ID_HEADER
    );

    // The reply to this arrives on the connection-level stream.
    let created = transport
        .request(
            &Scope::Connection,
            "session/new",
            serde_json::json!({ "cwd": CWD, "mcpServers": [] }),
            Duration::from_secs(20),
        )
        .await
        .expect("session/new");

    let session_id = created["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert!(
        created["modes"]["availableModes"].is_array(),
        "goose advertises its modes; a bare id would mean the reply was truncated: {created}"
    );

    // Subscribed *before* the prompt, which is the ordering the replay test
    // cannot exercise.
    let mut updates = transport.subscribe(&session_id).await.expect("subscribe");

    let reply = transport
        .request(
            &Scope::Session(session_id.clone()),
            "session/prompt",
            serde_json::json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "reply with the single word: ok" }],
            }),
            Duration::from_secs(180),
        )
        .await
        .expect("session/prompt");

    assert!(
        reply["stopReason"].is_string(),
        "a prompt reply carries a stopReason: {reply}"
    );
    assert!(
        reply["usage"]["totalTokens"].is_number(),
        "and a usage block, which is how this project reports tokens without goose's DB: {reply}"
    );

    // Drain whatever arrived. The turn is over, so this is a snapshot, but it
    // proves updates reached the session-scoped channel at all — and the whole
    // point of the demultiplexer is that they reach *this* session's channel and
    // no other.
    let mut seen = 0;
    while let Ok(update) = updates.try_recv() {
        assert_eq!(
            update["params"]["sessionId"].as_str(),
            Some(session_id.as_str()),
            "an update for another session reached this one: {update}"
        );
        seen += 1;
    }
    assert!(seen > 0, "the session stream delivered no updates at all");

    transport
        .request(
            &Scope::Session(session_id.clone()),
            "session/close",
            serde_json::json!({ "sessionId": session_id }),
            Duration::from_secs(20),
        )
        .await
        .expect("session/close");

    assert_eq!(transport.in_flight(), 0, "no request is left waiting");
}
