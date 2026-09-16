//! The skill catalogue: `ask` plus whatever is mined or declared.
//!
//! A **skill** is what a caller selects with `metadata.skillId`. It chooses what
//! to *say* to goose, never which session to say it in — a session is a
//! conversation with a `cwd`, and reusing a `contextId` under a different skill
//! is legal and must not fork the session (§6.3).
//!
//! Three sources, merged in a fixed order, with a fixed precedence:
//!
//! 1. `ask` — implicit, always present, never removable and never redefinable
//!    (constraint #10). It makes an omitted `skillId` safe: a bare goose session
//!    with no instruction attached.
//! 2. **Recipes** — mined from the host's own goose recipes by
//!    [`crate::recipes`]. A recipe that is not in `skills.recipes.enabled` is not
//!    advertised.
//! 3. **`skills.d/`** — one hand-written file per skill, for anything with no
//!    recipe behind it.
//!
//! then `skills.overrides` renames or re-describes by id. Precedence per id is
//! recipe → `skills.d/` → `overrides`; ids must be unique across the first two,
//! and a collision is a startup failure rather than a silent override, because
//! the id is the routing contract callers freeze.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{Override, Skills};
use crate::recipes::{self, Mined, RecipeError};

/// The only built-in skill. Present in every card, in every configuration.
pub const ASK_ID: &str = "ask";

/// What selecting a skill does when a turn actually runs.
///
/// This is *not* on the card (constraint #12). `card.rs` projects `id`, `name`,
/// `description` and `tags`; everything here stays on disk or in memory and is
/// read only by the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// A bare goose session: the caller's own text is the prompt.
    Ask,
    /// Hand the turn to goose's recipe runner. The recipe's `instructions`,
    /// `extensions` and `parameters` are read by goose, from the file.
    Recipe(PathBuf),
    /// Say this to goose. Used by `skills.d/` entries that have no recipe.
    Instruction(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub dispatch: Dispatch,
}

/// A hand-written skill file. `deny_unknown_fields` on purpose: a typo'd key in
/// a skill file is a skill that does not do what its author thinks, which is
/// exactly the class of bug a startup failure should catch.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeclaredFile {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    instruction: Option<String>,
}

/// Where a skill came from, for error messages that name a file.
type Sourced = (Skill, String);

#[derive(Debug)]
pub enum SkillError {
    Recipes(RecipeError),
    ReadDir {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    EmptySet,
    AskRedefined {
        source: String,
    },
    DuplicateId {
        id: String,
        first: String,
        second: String,
    },
    UnknownDefaultSkill {
        default: String,
        known: Vec<String>,
    },
    UnknownOverride {
        id: String,
    },
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recipes(err) => write!(f, "{err}"),
            Self::ReadDir { path, source } => {
                write!(f, "cannot list {}: {source}", path.display())
            }
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, source } => write!(f, "cannot parse {}: {source}", path.display()),
            Self::EmptySet => write!(
                f,
                "no skills resolved. `ask` is implicit and cannot be removed, so this is a bug \
                 rather than a configuration mistake - refusing to serve an empty card"
            ),
            Self::AskRedefined { source } => write!(
                f,
                "{source} declares the skill id {ASK_ID:?}, which is the built-in and cannot be \
                 redefined or removed (constraint #10)"
            ),
            Self::DuplicateId { id, first, second } => write!(
                f,
                "skill id {id:?} is declared twice: {first} and {second}. Ids are the routing \
                 contract callers depend on, so a collision is a startup failure rather than a \
                 silent override"
            ),
            Self::UnknownDefaultSkill { default, known } => write!(
                f,
                "skills.default is {default:?}, which is not a known skill id (known: {})",
                known.join(", ")
            ),
            Self::UnknownOverride { id } => write!(
                f,
                "skills.overrides names {id:?}, which no recipe or skills.d/ file declares"
            ),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<RecipeError> for SkillError {
    fn from(err: RecipeError) -> Self {
        SkillError::Recipes(err)
    }
}

/// The merged catalogue. A `BTreeMap` so iteration — and therefore the card — is
/// ordered by id, which is what makes the card hash a statement about content
/// rather than about directory order.
#[derive(Debug, Clone)]
pub struct SkillSet {
    default_id: String,
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// Loads everything: recipes, `skills.d/`, then overrides.
    pub fn load(config: &Skills) -> Result<Self, SkillError> {
        let mined = recipes::mine(&config.recipes)?;
        let declared = read_declared(&config.d)?;

        let mut sourced: Vec<Sourced> = Vec::with_capacity(1 + mined.len() + declared.len());
        sourced.push((ask_skill(), "the built-in".to_string()));
        sourced.extend(mined.into_iter().map(skill_from_recipe));
        sourced.extend(declared);

        Self::assemble(sourced, &config.default, &config.overrides)
    }

    /// Merges already-sourced skills, applies overrides, and enforces the two
    /// invariants that make an omitted or unknown `skillId` safe.
    pub(crate) fn assemble(
        sourced: Vec<Sourced>,
        default: &str,
        overrides: &BTreeMap<String, Override>,
    ) -> Result<Self, SkillError> {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
        let mut origin: BTreeMap<String, String> = BTreeMap::new();

        for (skill, source) in sourced {
            if skill.id == ASK_ID && !origin.is_empty() {
                return Err(SkillError::AskRedefined { source });
            }
            if let Some(first) = origin.get(&skill.id) {
                return Err(SkillError::DuplicateId {
                    id: skill.id.clone(),
                    first: first.clone(),
                    second: source,
                });
            }
            origin.insert(skill.id.clone(), source);
            skills.insert(skill.id.clone(), skill);
        }

        if skills.is_empty() {
            return Err(SkillError::EmptySet);
        }

        for (id, over) in overrides {
            let Some(skill) = skills.get_mut(id) else {
                return Err(SkillError::UnknownOverride { id: id.clone() });
            };
            if let Some(name) = &over.name {
                skill.name = name.clone();
            }
            if let Some(description) = &over.description {
                skill.description = description.clone();
            }
            if let Some(tags) = &over.tags {
                skill.tags = tags.clone();
            }
        }

        if !skills.contains_key(default) {
            return Err(SkillError::UnknownDefaultSkill {
                default: default.to_string(),
                known: skills.keys().cloned().collect(),
            });
        }

        Ok(Self {
            default_id: default.to_string(),
            skills,
        })
    }

    pub fn default_id(&self) -> &str {
        &self.default_id
    }

    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.get(id)
    }

    /// The skill a caller gets when they omit `metadata.skillId`. Total, because
    /// `assemble` refuses to build a set in which the default is unknown.
    pub fn default_skill(&self) -> &Skill {
        self.skills
            .get(&self.default_id)
            .expect("assemble guarantees the default id resolves")
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Deterministic: ordered by id, because a `BTreeMap` is.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    pub fn ids(&self) -> Vec<&str> {
        self.skills.keys().map(String::as_str).collect()
    }
}

fn ask_skill() -> Skill {
    Skill {
        id: ASK_ID.to_string(),
        name: "Ask".to_string(),
        description: "Ask goose directly, with no recipe or instruction attached".to_string(),
        tags: vec!["chat".to_string()],
        dispatch: Dispatch::Ask,
    }
}

fn skill_from_recipe(mined: Mined) -> Sourced {
    // `parameters` and `extensions` are deliberately not carried across: this is
    // a projection, and the recipe file itself is what goose reads when the
    // skill is dispatched.
    let source = format!("recipe {}", mined.path.display());
    let tags = derived_tags(&mined.id);
    let skill = Skill {
        id: mined.id.clone(),
        name: mined.title.clone().unwrap_or_else(|| mined.id.clone()),
        description: mined.description.clone().unwrap_or_default(),
        tags,
        dispatch: Dispatch::Recipe(mined.path),
    };
    (skill, source)
}

/// Recipes carry no tags (S10 lists `version`, `title`, `description`,
/// `parameters`, `prompt`, `instructions`), so a group is derived from the id:
/// `scaffold-project` → `scaffold`. A cheap grouping, not a taxonomy — it exists
/// so a card reader has something to filter by, and `skills.overrides` exists
/// for when the guess is wrong.
fn derived_tags(id: &str) -> Vec<String> {
    let mut tags = vec!["recipe".to_string()];
    if let Some(first) = id.split(['-', '_', '.']).next() {
        if !first.is_empty() && first != id {
            tags.push(first.to_string());
        }
    }
    if tags.len() == 1 {
        tags.push(id.to_string());
    }
    tags
}

/// Reads `skills.d/`. A missing directory is not an error — most hosts will not
/// have one — but a file that will not parse is.
fn read_declared(dir: &Path) -> Result<Vec<Sourced>, SkillError> {
    if !dir.exists() {
        tracing::debug!(path = %dir.display(), "no skills.d directory; only ask plus recipes");
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(dir).map_err(|source| SkillError::ReadDir {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_skill_file(path))
        .collect();
    paths.sort();

    let mut declared = Vec::with_capacity(paths.len());
    for path in paths {
        let text = fs::read_to_string(&path).map_err(|source| SkillError::Read {
            path: path.clone(),
            source,
        })?;
        let file: DeclaredFile =
            serde_yaml::from_str(&text).map_err(|source| SkillError::Parse {
                path: path.clone(),
                source,
            })?;
        declared.push((
            Skill {
                id: file.id,
                name: file.name,
                description: file.description,
                tags: file.tags,
                dispatch: match file.instruction {
                    Some(text) => Dispatch::Instruction(text),
                    None => Dispatch::Ask,
                },
            },
            format!("skills.d/{}", path.display()),
        ));
    }
    Ok(declared)
}

fn is_skill_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "a2a-goose-skills-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, body).expect("write skill");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn skills_dir(dir: &TempDir, enabled: Vec<&str>) -> Skills {
        Skills {
            default: ASK_ID.to_string(),
            recipes: crate::config::Recipes {
                search_paths: Vec::new(),
                enabled: enabled.into_iter().map(str::to_string).collect(),
            },
            d: dir.0.clone(),
            overrides: BTreeMap::new(),
        }
    }

    fn skill(id: &str, source: &str) -> Sourced {
        (
            Skill {
                id: id.to_string(),
                name: id.to_string(),
                description: String::new(),
                tags: Vec::new(),
                dispatch: Dispatch::Ask,
            },
            source.to_string(),
        )
    }

    #[test]
    fn ask_is_present_with_no_configuration_at_all() {
        let dir = TempDir::new("ask");
        let set = SkillSet::load(&skills_dir(&dir, vec![])).expect("load");
        assert_eq!(set.len(), 1);
        assert_eq!(set.ids(), vec![ASK_ID]);
        assert_eq!(set.default_id(), ASK_ID);
        assert_eq!(set.default_skill().dispatch, Dispatch::Ask);
    }

    #[test]
    fn an_empty_set_is_a_bug_not_a_card() {
        let err = SkillSet::assemble(Vec::new(), ASK_ID, &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, SkillError::EmptySet), "{err}");
    }

    #[test]
    fn ask_cannot_be_redefined_by_a_declared_skill() {
        let dir = TempDir::new("ask-redefined");
        dir.write(
            "rogue.yaml",
            "id: ask\nname: Rogue\ndescription: no\ninstruction: say something else\n",
        );
        let err = SkillSet::load(&skills_dir(&dir, vec![])).unwrap_err();
        assert!(matches!(err, SkillError::AskRedefined { .. }), "{err}");
        assert!(err.to_string().contains("constraint #10"), "{err}");
    }

    #[test]
    fn a_declared_skill_appears_with_its_instruction_kept_off_the_card() {
        let dir = TempDir::new("declared");
        dir.write(
            "code-review.yaml",
            "id: code-review\nname: Code review\ndescription: Review a diff\ntags: [review]\n\
             instruction: |\n  Review the diff.\n",
        );
        let set = SkillSet::load(&skills_dir(&dir, vec![])).expect("load");
        assert_eq!(set.ids(), vec!["ask", "code-review"]);
        let skill = set.get("code-review").expect("declared skill");
        assert_eq!(skill.name, "Code review");
        assert_eq!(skill.tags, vec!["review"]);
        assert_eq!(
            skill.dispatch,
            Dispatch::Instruction("Review the diff.\n".to_string())
        );
    }

    #[test]
    fn a_declared_skill_with_no_instruction_is_a_bare_session() {
        let dir = TempDir::new("no-instruction");
        dir.write(
            "plain.yaml",
            "id: plain\nname: Plain\ndescription: Just talks\n",
        );
        let set = SkillSet::load(&skills_dir(&dir, vec![])).expect("load");
        assert_eq!(set.get("plain").expect("plain").dispatch, Dispatch::Ask);
    }

    #[test]
    fn a_duplicate_id_across_sources_fails_startup() {
        let dir = TempDir::new("duplicate");
        dir.write("a.yaml", "id: same\nname: A\ndescription: a\n");
        dir.write("b.yaml", "id: same\nname: B\ndescription: b\n");
        let err = SkillSet::load(&skills_dir(&dir, vec![])).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateId { .. }), "{err}");
        assert!(err.to_string().contains("\"same\""), "{err}");
    }

    #[test]
    fn an_unknown_key_in_a_skill_file_is_a_parse_error() {
        let dir = TempDir::new("typo");
        let path = dir.write(
            "typo.yaml",
            "id: t\nname: T\ndescription: d\ninstructions: oops\n",
        );
        let err = SkillSet::load(&skills_dir(&dir, vec![])).unwrap_err();
        assert!(matches!(err, SkillError::Parse { .. }), "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_default_that_names_nothing_is_refused_and_lists_what_exists() {
        let err = SkillSet::assemble(vec![skill("ask", "built-in")], "nope", &BTreeMap::new())
            .unwrap_err();
        let SkillError::UnknownDefaultSkill { known, .. } = &err else {
            panic!("expected UnknownDefaultSkill, got {err}");
        };
        assert_eq!(known, &vec!["ask".to_string()]);
    }

    #[test]
    fn an_override_for_an_unknown_id_is_refused() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "ghost".to_string(),
            Override {
                name: Some("Ghost".to_string()),
                ..Override::default()
            },
        );
        let err =
            SkillSet::assemble(vec![skill("ask", "built-in")], ASK_ID, &overrides).unwrap_err();
        assert!(matches!(err, SkillError::UnknownOverride { .. }), "{err}");
    }

    #[test]
    fn an_override_renames_display_fields_only() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "code-review".to_string(),
            Override {
                name: Some("Review a diff".to_string()),
                description: Some("House style".to_string()),
                tags: Some(vec!["quality".to_string()]),
            },
        );
        let mut sourced = vec![
            skill("ask", "built-in"),
            skill("code-review", "skills.d/x.yaml"),
        ];
        sourced[1].0.dispatch = Dispatch::Instruction("original".to_string());

        let set = SkillSet::assemble(sourced, ASK_ID, &overrides).expect("assemble");
        let skill = set.get("code-review").expect("skill");
        assert_eq!(skill.name, "Review a diff");
        assert_eq!(skill.description, "House style");
        assert_eq!(skill.tags, vec!["quality"]);
        // The id is the contract, and the dispatch is not display: neither is
        // touched by an override.
        assert_eq!(skill.id, "code-review");
        assert_eq!(
            skill.dispatch,
            Dispatch::Instruction("original".to_string())
        );
    }

    #[test]
    fn a_missing_skills_d_directory_is_not_an_error() {
        let mut config = skills_dir(&TempDir::new("unused"), vec![]);
        config.d = PathBuf::from("/definitely/not/a/skills/dir");
        let set = SkillSet::load(&config).expect("load");
        assert_eq!(set.ids(), vec![ASK_ID]);
    }

    #[test]
    fn recipes_and_declared_files_merge_in_id_order() {
        let recipes_dir = TempDir::new("merge-recipes");
        recipes_dir.write("zebra.yaml", "title: Zebra\n");
        let skills = TempDir::new("merge-skills");
        skills.write("apple.yaml", "id: apple\nname: Apple\ndescription: a\n");

        let mut config = skills_dir(&skills, vec!["zebra"]);
        config.recipes.search_paths = vec![recipes_dir.0.clone()];

        let set = SkillSet::load(&config).expect("load");
        assert_eq!(set.ids(), vec!["apple", "ask", "zebra"]);
        assert!(matches!(
            set.get("zebra").expect("zebra").dispatch,
            Dispatch::Recipe(_)
        ));
    }
}
