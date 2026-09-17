//! The agent card: what LiteLLM fetches at registration, and what callers read.
//!
//! The card is **assembled, not written**: `ask` plus the merged skill set, with
//! the agent's address taken from `server.publicUrl`. Two properties matter more
//! than the fields themselves.
//!
//! **It is built through the SDK's `AgentCard` type**, not hand-rolled JSON
//! (constraint #17). The wire shape is `a2a-lf`'s, and a version bump that
//! changes it is a `Cargo.lock` change we review, not a silent drift.
//!
//! **It serialises deterministically and is hashed as a whole.** Skills come out
//! of a `BTreeMap` in id order, and [`canonical_json`] sorts object keys
//! recursively before hashing — which is what makes the hash in
//! [`hash`] a statement about the card's *content*. `registry.rs` re-registers
//! when this hash changes, so an unstable hash would mean re-registering on
//! every boot, and an unstable hash that *looked* stable would mean a recipe
//! edit that never converged.
//!
//! Deliberately absent: `securitySchemes`. The bearer token (constraint #3) is
//! enforced by the server on every `POST /`, and §5.4 wants a 401 for a bad
//! token — a JSON-RPC-level declaration cannot produce one. Declaring it on the
//! card is therefore documentation rather than enforcement, and it is left off
//! until S13 has shown what LiteLLM's card parser accepts.

use a2a::{
    AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill,
    TRANSPORT_PROTOCOL_JSONRPC,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::config::Config;
use crate::skills::SkillSet;

/// The card's input/output modes. Text only in phase 1: the ACP hop is text in,
/// text out, and tool/file parts are summarised rather than embedded (§5.1).
pub const TEXT_PLAIN: &str = "text/plain";

/// The extension that says how long a turn may take.
///
/// Every caller this agent has is bounded by a timeout it did not choose — a
/// hub tool call, an Open WebUI valve, an editor — and none of them can see
/// this agent's `promptSecs`, which is the ceiling the agent actually enforces.
/// The A2A spec has no field for it, and `capabilities.extensions` is exactly
/// the place for a fact that an unaware client may ignore: `required: false`,
/// describe it in words, and put the numbers in `params` so a caller that *does*
/// read it can act without parsing prose.
///
/// This is advertising, not enforcement (constraint #5, §5.3): the deadline is
/// enforced by the ACP transport. Note also that it changes the card, and the
/// card is hashed — so changing a timeout re-registers the agent, which is the
/// intended behaviour of §6.2 rather than a cost to avoid.
pub const TURN_DEADLINE_URI: &str =
    "https://github.com/nickbrett1/a2a-goose/blob/main/docs/turn-deadline.md";

/// Builds the card from configuration and the merged skill catalogue.
pub fn assemble(config: &Config, skills: &SkillSet) -> AgentCard {
    let mut interface = AgentInterface::new(&config.server.public_url, TRANSPORT_PROTOCOL_JSONRPC);
    // `AgentInterface::new` pins the SDK's own version. Pinning it from config is
    // the point of `card.protocolVersion`: the card is what a caller negotiates
    // against, and an unpinned agent serves 0.3-shaped responses to callers that
    // sent no `a2a-version` header (S7).
    interface.protocol_version = config.card.protocol_version.clone();

    AgentCard {
        name: config.card.name.clone(),
        description: config.card.description.clone(),
        version: config.card.version.clone(),
        supported_interfaces: vec![interface],
        capabilities: AgentCapabilities {
            // Honest from M1: the SDK's request handler drives streaming straight
            // from the executor's event stream, so `message/stream` works as soon
            // as an executor returns more than one event.
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: Some(vec![turn_deadline(config)]),
            extended_agent_card: None,
        },
        default_input_modes: vec![TEXT_PLAIN.to_string()],
        default_output_modes: vec![TEXT_PLAIN.to_string()],
        // The projection, and only the projection: id, name, description, tags.
        // `Skill::dispatch` carries the recipe path or the instruction text and
        // is deliberately not reachable from here (constraint #12).
        skills: skills.iter().map(project).collect(),
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

/// The deadline as an extension: the numbers in `params`, the sentence in
/// `description`, and `required: false` — a client that has never heard of this
/// must be able to ignore it (`card.rs`'s rule about advertising without
/// enforcing).
fn turn_deadline(config: &Config) -> AgentExtension {
    let prompt_secs = config.goose.acp.timeouts.prompt_secs;
    let cancel_secs = config.goose.acp.timeouts.cancel_secs;
    let mut params = HashMap::new();
    params.insert("promptSecs".to_string(), Value::from(prompt_secs));
    params.insert("cancelSecs".to_string(), Value::from(cancel_secs));

    AgentExtension {
        uri: TURN_DEADLINE_URI.to_string(),
        description: Some(format!(
            "One turn on this agent may take up to {prompt_secs} seconds, and a cancel is \
             acknowledged within {cancel_secs}. Agent-to-agent calls are meant for relatively \
             short-lived work: a caller that gives up before the turn finishes does not stop it, \
             so a long task is better asked to write its result down somewhere durable and \
             collected afterwards."
        )),
        required: Some(false),
        params: Some(params),
    }
}

fn project(skill: &crate::skills::Skill) -> AgentSkill {
    AgentSkill {
        id: skill.id.clone(),
        name: skill.name.clone(),
        description: skill.description.clone(),
        tags: skill.tags.clone(),
        examples: None,
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    }
}

/// A stable fingerprint of the card, for §6.2's change detection.
///
/// The digest is hexed byte by byte rather than with `format!("{:x}", ..)`.
/// `LowerHex` on the digest output is not part of sha2's stable surface — it
/// went away in 0.11, which turned an unrelated dependency bump into a compile
/// error here (E0277: `Array<u8, ..>: LowerHex` is not satisfied). Bytes are
/// the interface that has not moved, so hexing them keeps this correct across
/// the bump instead of pinning a version to keep one format string working.
pub fn hash(card: &AgentCard) -> String {
    use std::fmt::Write as _;

    let value = serde_json::to_value(card).expect("an AgentCard always serialises");
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(&value).as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        // Writing to a String cannot fail; the result is ignored rather than
        // unwrapped so there is no panic path in a function that fingerprints.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// JSON with object keys sorted, recursively.
///
/// Not cosmetic: `AgentCard` holds two `HashMap`s (`securitySchemes`,
/// `securityRequirements`), whose iteration order is not stable across runs.
/// Hashing the serialised struct directly would make the hash randomly change
/// and re-register the agent for no reason.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // `Value`'s own Display is compact and deterministic for scalars.
        other => out.push_str(&other.to_string()),
    }
}

/// Reads the skill ids out of a *card-shaped JSON value*.
///
/// Used by `registry.rs` against LiteLLM's response rather than against our own
/// card, because those are different objects: S5 showed a registration POST for
/// an unreachable URL answers with LiteLLM's own synthesised card
/// (`skills: [{id: "chat"}]`), so "the agent is listed" is not evidence that
/// LiteLLM read ours. The assertion has to be made on the **skills**.
pub fn skill_ids(card: &Value) -> Vec<String> {
    card.get("skills")
        .and_then(Value::as_array)
        .map(|skills| {
            skills
                .iter()
                .filter_map(|skill| skill.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::config::{Config, Recipes, Skills};

    fn config() -> Config {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.card.name = "mac-studio-goose".to_string();
        config.card.description = "goose on the Mac Studio, via A2A".to_string();
        config.card.version = "1.2.3".to_string();
        config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
        config.skills = Skills {
            default: "ask".to_string(),
            recipes: Recipes::default(),
            d: PathBuf::from("/definitely/not/a/skills/dir"),
            overrides: Default::default(),
        };
        config
    }

    #[test]
    fn the_card_carries_the_address_the_dialer_uses() {
        let config = config();
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = assemble(&config, &skills);

        assert_eq!(card.name, "mac-studio-goose");
        assert_eq!(card.version, "1.2.3");
        assert_eq!(card.supported_interfaces.len(), 1);
        let interface = &card.supported_interfaces[0];
        assert_eq!(
            interface.url, "http://mac-studio.tail86fd19.ts.net:10001",
            "the card url is what LiteLLM dials, so it must be the publicUrl"
        );
        assert_eq!(interface.protocol_binding, TRANSPORT_PROTOCOL_JSONRPC);
        assert_eq!(interface.protocol_version, "1.0");
    }

    #[test]
    fn ask_alone_is_a_valid_card() {
        let config = config();
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = assemble(&config, &skills);
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "ask");
        assert_eq!(card.skills[0].name, "Ask");
        assert_eq!(card.capabilities.streaming, Some(true));
    }

    #[test]
    fn the_card_advertises_the_turn_deadline() {
        // A caller's timeout is the one thing about this agent it cannot
        // discover and did not choose (see TURN_DEADLINE_URI).
        let mut config = config();
        config.goose.acp.timeouts.prompt_secs = 1234;
        config.goose.acp.timeouts.cancel_secs = 7;
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = assemble(&config, &skills);

        let advertised = card
            .capabilities
            .extensions
            .as_ref()
            .expect("capabilities carry extensions")
            .iter()
            .find(|extension| extension.uri == TURN_DEADLINE_URI)
            .expect("the turn deadline is one of them");

        // Ignorable by a client that has never heard of it...
        assert_eq!(advertised.required, Some(false));
        // ...readable by one that has, without parsing prose...
        let params = advertised.params.as_ref().expect("params");
        assert_eq!(params["promptSecs"], Value::from(1234u64));
        assert_eq!(params["cancelSecs"], Value::from(7u64));
        // ...and said in words for a model that is only reading the card.
        let description = advertised.description.as_deref().unwrap_or_default();
        assert!(description.contains("1234"), "{description}");
    }

    #[test]
    fn a_changed_timeout_changes_the_card_hash() {
        // Timeouts come from config and the card is hashed, so a host that
        // raises its ceiling re-registers — which is what §6.2 is for.
        let skills = SkillSet::load(&config().skills).expect("skills");
        let base = hash(&assemble(&config(), &skills));

        let mut slower = config();
        slower.goose.acp.timeouts.prompt_secs += 60;
        assert_ne!(base, hash(&assemble(&slower, &skills)));
    }

    #[test]
    fn skills_are_ordered_by_id_not_by_arrival() {
        let recipes = std::env::temp_dir().join(format!("a2a-goose-card-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&recipes);
        std::fs::create_dir_all(&recipes).expect("dir");
        for (name, title) in [("zebra", "Zebra"), ("apple", "Apple")] {
            std::fs::write(
                recipes.join(format!("{name}.yaml")),
                format!("title: {title}\n"),
            )
            .expect("write recipe");
        }

        let mut config = config();
        config.skills.recipes.search_paths = vec![recipes.clone()];
        config.skills.recipes.enabled = vec!["zebra".to_string(), "apple".to_string()];
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = assemble(&config, &skills);

        assert_eq!(
            card.skills
                .iter()
                .map(|skill| skill.id.as_str())
                .collect::<Vec<_>>(),
            vec!["apple", "ask", "zebra"]
        );
        let _ = std::fs::remove_dir_all(recipes);
    }

    #[test]
    fn the_hash_is_stable_across_identical_cards() {
        let config = config();
        let skills = SkillSet::load(&config.skills).expect("skills");
        let a = hash(&assemble(&config, &skills));
        let b = hash(&assemble(&config, &skills));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha256 hex");
    }

    #[test]
    fn the_hash_changes_when_the_card_does() {
        let skills = SkillSet::load(&config().skills).expect("skills");

        let base = hash(&assemble(&config(), &skills));

        let mut renamed = config();
        renamed.card.name = "another-host-goose".to_string();
        assert_ne!(base, hash(&assemble(&renamed, &skills)));

        let mut readdressed = config();
        readdressed.server.public_url = "http://nas.tail86fd19.ts.net:10001".to_string();
        assert_ne!(base, hash(&assemble(&readdressed, &skills)));

        let mut versioned = config();
        versioned.card.version = "1.2.4".to_string();
        assert_ne!(base, hash(&assemble(&versioned, &skills)));
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let value: Value =
            serde_json::from_str(r#"{"b":{"d":1,"c":2},"a":[{"z":1,"y":2}]}"#).expect("parse");
        assert_eq!(
            canonical_json(&value),
            r#"{"a":[{"y":2,"z":1}],"b":{"c":2,"d":1}}"#
        );
    }

    #[test]
    fn canonical_json_ignores_key_insertion_order() {
        let one: Value = serde_json::from_str(r#"{"x":1,"y":2}"#).expect("parse");
        let two: Value = serde_json::from_str(r#"{"y":2,"x":1}"#).expect("parse");
        assert_eq!(canonical_json(&one), canonical_json(&two));
    }

    #[test]
    fn the_projected_skill_carries_no_dispatch_detail() {
        // A recipe-backed skill and a declared one, because those are the two
        // kinds that *have* execution config to leak.
        let recipes =
            std::env::temp_dir().join(format!("a2a-goose-card-leak-{}", std::process::id()));
        let skills_dir =
            std::env::temp_dir().join(format!("a2a-goose-card-d-{}", std::process::id()));
        for dir in [&recipes, &skills_dir] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(
            recipes.join("leaky.yaml"),
            "title: Leaky\ndescription: d\nprompt: p\ninstructions: do the secret thing\n\
             extensions:\n  - type: builtin\n    name: computercontroller\n",
        )
        .expect("write recipe");
        std::fs::write(
            skills_dir.join("declared.yaml"),
            "id: declared\nname: Declared\ndescription: d\ninstruction: say the secret thing\n",
        )
        .expect("write skill");

        let mut config = config();
        config.skills.recipes.search_paths = vec![recipes.clone()];
        config.skills.recipes.enabled = vec!["leaky".to_string()];
        config.skills.d = skills_dir.clone();

        let skills = SkillSet::load(&config.skills).expect("skills");
        let value = serde_json::to_value(assemble(&config, &skills)).expect("serialise");
        let rendered = serde_json::to_string(&value).expect("render");

        for leaked in [
            "do the secret thing",
            "say the secret thing",
            "computercontroller",
            "\"dispatch\"",
            "\"instructions\"",
        ] {
            assert!(
                !rendered.contains(leaked),
                "leaked {leaked:?} in {rendered}"
            );
        }

        // The positive half: each skill carries exactly the four card fields.
        let skills_json = value["skills"].as_array().expect("skills array");
        assert_eq!(skills_json.len(), 3);
        for skill in skills_json {
            let mut keys: Vec<&String> = skill.as_object().expect("object").keys().collect();
            keys.sort();
            assert_eq!(
                keys,
                vec!["description", "id", "name", "tags"],
                "unexpected field on a projected skill: {skill}"
            );
        }

        let _ = std::fs::remove_dir_all(recipes);
        let _ = std::fs::remove_dir_all(skills_dir);
    }
}
