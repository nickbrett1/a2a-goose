//! The committed example config must actually load.
//!
//! `config/config.example.yaml` is the operator's starting point and the only
//! place most of the field documentation lives, so it drifts quietly: a renamed
//! field or a new required one turns it into a file that fails on first use,
//! which is a bad first five minutes and exactly the kind of thing that is
//! nobody's job. This test makes it somebody's job — the build's.
//!
//! The example is checked the way an operator would copy it: through
//! `Config::load_from`, so parsing, path expansion and the startup refusals all
//! run.

use std::path::PathBuf;

use a2a_goose::card;
use a2a_goose::config::Config;
use a2a_goose::skills::SkillSet;

fn example_config() -> PathBuf {
    // `CARGO_MANIFEST_DIR` rather than a relative path: the test's working
    // directory is not something a test should depend on.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.example.yaml")
}

#[test]
fn the_example_config_loads_and_passes_its_own_validation() {
    let config = Config::load_from(&example_config(), true).expect("the example must load");

    assert_eq!(config.card.protocol_version, "1.0");
    assert!(
        !config.server.public_url.trim().is_empty(),
        "the example must carry a publicUrl: it is the field with no safe default"
    );
    assert!(
        !config.goose.defaults.allowed_roots.is_empty(),
        "the example must carry an allowlist (constraint #4)"
    );
}

#[test]
fn the_example_config_assembles_a_card_with_ask_on_it() {
    let config = Config::load_from(&example_config(), true).expect("load");
    let skills = SkillSet::load(&config.skills).expect("skills");
    let card = card::assemble(&config, &skills);

    assert_eq!(card.skills[0].id, "ask");
    assert!(!card.supported_interfaces.is_empty());
    assert_eq!(card::hash(&card).len(), 64);
}

#[test]
fn skills_example_is_a_skill_the_loader_accepts() {
    // `config/skills.example.yaml` is what an operator copies into `skills.d/`,
    // so it has to survive the same `deny_unknown_fields` parse a real one does.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/skills.example.yaml");
    let dir = std::env::temp_dir().join(format!("a2a-goose-example-skill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::copy(&path, dir.join("example.yaml")).expect("copy example skill");

    let config = a2a_goose::config::Skills {
        d: dir.clone(),
        ..a2a_goose::config::Skills::default()
    };
    let skills = SkillSet::load(&config).expect("the example skill must load");
    assert!(
        skills.get("code-review").is_some(),
        "the example declares id `code-review`; if that changed, so must this test"
    );

    let _ = std::fs::remove_dir_all(dir);
}
