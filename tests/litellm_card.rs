//! Can this project read LiteLLM's own agent card?
//!
//! Half of [S13](../../spikes/S13.md): "do LiteLLM's replies parse back?" The
//! other half — whether LiteLLM accepts *our* card — is measured against the
//! live proxy and recorded in the spike, because it needs one.
//!
//! The fixture is `GET /a2a/{agent_id}/.well-known/agent-card.json` from LiteLLM
//! 1.103.0, with the agent id replaced by zeros and the probe's name replaced by
//! a neutral one. It is committed because it is a **pin**, not an anecdote: it is
//! the counterparty's shape, and the day either side moves, these tests say so.
//!
//! The answer today is no, and the interesting part is *how* it is no: the two
//! card models fail in a ladder. Each test below removes exactly one
//! incompatibility and names the next, so the gap is stated precisely rather than
//! as one blanket "incompatible".

use a2a::AgentCard;
use serde_json::Value;

const LITELLM_CARD: &str = include_str!("fixtures/litellm-agent-card.json");

fn fixture() -> Value {
    serde_json::from_str(LITELLM_CARD).expect("the fixture is JSON")
}

fn without_a_field(name: &str) -> Value {
    let mut value = fixture();
    value.as_object_mut().expect("object").remove(name);
    value
}

#[test]
fn the_first_reason_is_the_security_scheme_shape() {
    // LiteLLM's `securitySchemes` is OpenAPI-flavoured:
    //   {"LiteLLMKey": {"type": "http", "scheme": "bearer"}}
    // `a2a-lf` models a scheme as a field-presence union instead:
    //   {"httpAuthSecurityScheme": {...}}
    // and its deserializer errors on anything that is not one of the union keys.
    // So the whole card is rejected on this one field — it is not "mostly
    // readable".
    let err = serde_json::from_str::<AgentCard>(LITELLM_CARD)
        .expect_err("a2a-lf cannot currently read LiteLLM's card; if this passes, S13 changed");
    assert!(
        err.to_string().contains("security scheme"),
        "expected the failure to be about the security-scheme union, got: {err}"
    );
}

#[test]
fn the_second_reason_is_a_description_this_project_requires_and_litellm_omits() {
    // `AgentCard.description` has no serde default in `a2a-lf`, so it is required;
    // LiteLLM's synthesised card does not carry one at all. Two independent
    // hard errors, which is what makes "just use LiteLLM's card" not work.
    let err = serde_json::from_value::<AgentCard>(without_a_field("securitySchemes"))
        .expect_err("still unreadable one field further in");
    assert!(
        err.to_string().contains("missing field `description`"),
        "expected the next failure to be the required description, got: {err}"
    );
}

#[test]
fn once_both_are_patched_the_overlapping_half_is_readable() {
    let mut value = without_a_field("securitySchemes");
    value.as_object_mut().expect("object").remove("security");
    value["description"] =
        Value::String("patched by the test, only to get past the two above".into());

    let card: AgentCard = serde_json::from_value(value.clone()).expect("now it parses");

    // The half that *is* compatible — which is why registration works at all.
    assert_eq!(card.name, "s13-probe-agent");
    assert_eq!(card.version, "1.0.0");
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "chat");
    assert_eq!(
        card.provider.as_ref().map(|p| p.organization.as_str()),
        Some("LiteLLM Proxy")
    );

    // And the half that is not: LiteLLM's address for the agent is the 0.3-style
    // top-level `url`, which the SDK does not have a field for. Today the two
    // agree through `supportedInterfaces` instead — LiteLLM emits a superset —
    // so a reader that only looks at interfaces still finds the agent.
    assert!(value["url"].is_string(), "LiteLLM still carries `url`");
    assert_eq!(card.supported_interfaces.len(), 1);
    assert_eq!(card.supported_interfaces[0].url, value["url"]);
    assert_eq!(card.supported_interfaces[0].protocol_version, "1.0");
    assert_eq!(card.supported_interfaces[0].protocol_binding, "JSONRPC");
}

#[test]
fn litellm_says_text_where_this_project_says_text_plain() {
    // Small, and worth pinning: LiteLLM's card advertises `text`, this project
    // advertises `text/plain` (the A2A 1.0 default). Nothing reads the field
    // today, so it is a difference to know about rather than a bug — but a caller
    // that filters on input modes would see two vocabularies for one idea.
    let mut value = without_a_field("securitySchemes");
    value.as_object_mut().expect("object").remove("security");
    value["description"] = Value::String("patched".into());
    let card: AgentCard = serde_json::from_value(value).expect("parses");

    assert_eq!(card.default_input_modes, vec!["text".to_string()]);
    assert_eq!(
        a2a_goose::card::TEXT_PLAIN,
        "text/plain",
        "this project's own card must say text/plain; if that changed, S13 needs re-reading"
    );
}
