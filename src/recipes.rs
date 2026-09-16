//! Recipe mining: the host's own goose recipes *are* the skill library.
//!
//! Skills are data, not code (constraint #10 aside: `ask` is the only built-in),
//! and the machines this runs on already have curated recipes. So the agent
//! enumerates the directories it is told to, projects each recipe onto a narrow
//! local struct, and puts the safe fields on the card.
//!
//! **Projection is the security boundary, not a formatting step** (constraint
//! #12). `RecipeFile` below reads exactly three keys and ignores the rest, so
//! `instructions`, `extensions`, `parameters` and anything else the recipe
//! carries cannot reach the card by accident — they are read by goose itself,
//! from the file, when a skill backed by that recipe is dispatched. The struct
//! is the whole reason this is not "parse into `serde_yaml::Value` and pick
//! fields out later": a `Value` would make leaking execution config a one-line
//! mistake instead of an impossible one.
//!
//! Spike [S10](../../spikes/S10.md) settled the shape at goose 1.50.0, and
//! corrected the plan: a recipe has **no `name` field**. Its identity is its
//! `title` (human) and its filename (machine), so the skill id is the filename
//! stem.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Recipes;

/// One recipe, projected onto the fields a skill card may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mined {
    /// The filename stem. Stable, filesystem-unique within a directory, and what
    /// a user would type on the command line: `scaffold-project.yaml` →
    /// `scaffold-project`.
    pub id: String,
    /// The recipe's `title`, for display. The id is the contract.
    pub title: Option<String>,
    pub description: Option<String>,
    /// Where it lives, so a dispatch can hand goose the file and so a failure
    /// can name it.
    pub path: PathBuf,
    /// The recipe takes parameters (S10). Not callable from A2A in phase 1 —
    /// there is no way to supply them — so it is *not* silently advertised.
    pub parameterised: bool,
    /// The recipe pulls in `extensions`. Recorded only so mining can say so out
    /// loud; the content never leaves the file.
    pub extensions: bool,
}

/// What a recipe file is allowed to contribute. Everything else in the file is
/// deliberately unread.
#[derive(Debug, Deserialize)]
struct RecipeFile {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// Presence only, never inspected. `serde_json::Value` rather than
    /// `serde_yaml::Value` because the shape of a goose parameter list is goose's
    /// business, and this struct must not depend on it.
    #[serde(default)]
    parameters: Option<serde_json::Value>,
    #[serde(default)]
    extensions: Option<serde_json::Value>,
}

#[derive(Debug)]
pub enum RecipeError {
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
    /// An id the operator explicitly enabled cannot be served. Loud, because the
    /// alternative is a routing hole: a skill that looks enabled and 400s.
    ParameterisedEnabled {
        id: String,
        path: PathBuf,
    },
    UnknownEnabled {
        id: String,
    },
}

impl fmt::Display for RecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDir { path, source } => {
                write!(f, "cannot list recipes in {}: {source}", path.display())
            }
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, source } => write!(f, "cannot parse {}: {source}", path.display()),
            Self::ParameterisedEnabled { id, path } => write!(
                f,
                "recipe {id:?} ({}) is listed in skills.recipes.enabled, but it takes \
                 parameters and phase 1 has no way to supply them. Either remove it from \
                 `enabled`, or give it a hand-written skills.d/ entry",
                path.display()
            ),
            Self::UnknownEnabled { id } => write!(
                f,
                "skills.recipes.enabled names {id:?}, which no recipe on the configured \
                 search paths declares - that is a typo, or a path is missing"
            ),
        }
    }
}

impl std::error::Error for RecipeError {}

const RECIPE_SUFFIXES: [&str; 2] = ["yaml", "yml"];

/// Mines the configured search paths, then applies the `enabled` allowlist.
///
/// A search path that does not exist is skipped, not an error: a fresh host with
/// no recipes yet is a normal state, and the card is still valid with `ask`. A
/// *file* that will not parse is an error — a silently dropped skill is a
/// routing hole nobody notices until a caller 400s.
pub fn mine(recipes: &Recipes) -> Result<Vec<Mined>, RecipeError> {
    let mut mined: Vec<Mined> = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();

    for dir in &recipes.search_paths {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %dir.display(), "recipe search path does not exist; skipping");
                continue;
            }
            Err(source) => {
                return Err(RecipeError::ReadDir {
                    path: dir.clone(),
                    source,
                });
            }
        };

        // Sorted, so the card is byte-identical across boots and a card hash is
        // a statement about content rather than about directory order.
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_recipe_file(path))
            .collect();
        paths.sort();

        for path in paths {
            let id = match path.file_stem().and_then(|stem| stem.to_str()) {
                Some(stem) if !stem.is_empty() => stem.to_string(),
                _ => {
                    tracing::warn!(
                        path = %path.display(),
                        "recipe filename is not valid UTF-8; skipping"
                    );
                    continue;
                }
            };

            // First-wins across search paths, matching goose's own lookup order
            // (S10). A collision inside one directory is impossible; across
            // directories it is a configuration smell, not a failure.
            if let Some(existing) = seen.get(&id) {
                tracing::warn!(
                    id = %id,
                    kept = %existing.display(),
                    ignored = %path.display(),
                    "duplicate recipe id across search paths; first wins"
                );
                continue;
            }

            let file = read_recipe(&path)?;
            let parameterised = is_present(file.parameters.as_ref());
            let extensions = is_present(file.extensions.as_ref());
            if extensions {
                // Worth a line: a recipe can pull in extensions, so mining is
                // not a sandbox, and `enabled` is doing real work (S10).
                tracing::info!(
                    id = %id,
                    path = %path.display(),
                    "recipe declares extensions; they stay on disk and are read by goose"
                );
            }

            seen.insert(id.clone(), path.clone());
            mined.push(Mined {
                id,
                title: file.title,
                description: file.description,
                path,
                parameterised,
                extensions,
            });
        }
    }

    apply_enabled(mined, recipes)
}

fn apply_enabled(mined: Vec<Mined>, recipes: &Recipes) -> Result<Vec<Mined>, RecipeError> {
    if recipes.enabled.is_empty() {
        // Not "advertise everything": an empty allowlist means advertise nothing
        // mined. The card still carries `ask`.
        return Ok(Vec::new());
    }

    for id in &recipes.enabled {
        if !mined.iter().any(|recipe| &recipe.id == id) {
            return Err(RecipeError::UnknownEnabled { id: id.clone() });
        }
    }

    let mut enabled = Vec::new();
    for recipe in mined {
        if !recipes.enabled.contains(&recipe.id) {
            continue;
        }
        if recipe.parameterised {
            return Err(RecipeError::ParameterisedEnabled {
                id: recipe.id,
                path: recipe.path,
            });
        }
        enabled.push(recipe);
    }
    Ok(enabled)
}

fn read_recipe(path: &Path) -> Result<RecipeFile, RecipeError> {
    let text = fs::read_to_string(path).map_err(|source| RecipeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_yaml::from_str(&text).map_err(|source| RecipeError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn is_recipe_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(ext) if RECIPE_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str())
    )
}

/// Present and not empty. A recipe that carries `parameters: []` takes none.
fn is_present(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(map)) => !map.is_empty(),
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "a2a-goose-recipes-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, body).expect("write recipe");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn recipes(paths: Vec<PathBuf>, enabled: Vec<&str>) -> Recipes {
        Recipes {
            search_paths: paths,
            enabled: enabled.into_iter().map(str::to_string).collect(),
        }
    }

    #[test]
    fn the_id_is_the_filename_stem_and_the_name_is_the_title() {
        let dir = TempDir::new("projection");
        dir.write(
            "scaffold-project.yaml",
            "version: 1\ntitle: Scaffold a project\ndescription: Lay out a new repo\n\
             prompt: do it\ninstructions: secret sauce\n",
        );

        let mined = mine(&recipes(vec![dir.0.clone()], vec!["scaffold-project"])).expect("mine");
        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].id, "scaffold-project");
        assert_eq!(mined[0].title.as_deref(), Some("Scaffold a project"));
        assert_eq!(mined[0].description.as_deref(), Some("Lay out a new repo"));
        assert!(!mined[0].parameterised);
    }

    #[test]
    fn execution_config_never_reaches_the_projection() {
        let dir = TempDir::new("no-leak");
        dir.write(
            "leaky.yaml",
            "title: Leaky\ndescription: d\nprompt: p\ninstructions: |\n  do the secret thing\n\
             extensions:\n  - type: builtin\n    name: computercontroller\nsecrets:\n  TOKEN: xyz\n",
        );

        let mined = mine(&recipes(vec![dir.0.clone()], vec!["leaky"])).expect("mine");
        // The type is the guarantee: there is nowhere on `Mined` for the
        // instruction text or a secret to go. The rendered struct is the
        // assertion that the leak has no address.
        let rendered = format!("{:?}", mined[0]);
        for leaked in ["do the secret thing", "xyz", "computercontroller"] {
            assert!(
                !rendered.contains(leaked),
                "leaked {leaked:?} in {rendered}"
            );
        }
    }

    #[test]
    fn a_parameterised_recipe_is_not_advertised_by_default() {
        let dir = TempDir::new("params");
        dir.write(
            "takes-args.yaml",
            "title: Takes args\nparameters:\n  - key: repo\n    input_type: string\n\
             requirement: required\n",
        );

        let mined = mine(&recipes(vec![dir.0.clone()], vec![])).expect("mine");
        assert!(mined.is_empty(), "empty allowlist advertises nothing");
    }

    #[test]
    fn explicitly_enabling_a_parameterised_recipe_is_a_loud_failure() {
        let dir = TempDir::new("params-enabled");
        let path = dir.write(
            "takes-args.yaml",
            "title: Takes args\nparameters:\n  - key: repo\n",
        );

        let err = mine(&recipes(vec![dir.0.clone()], vec!["takes-args"])).unwrap_err();
        assert!(
            matches!(err, RecipeError::ParameterisedEnabled { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("parameters"), "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn an_empty_parameter_list_takes_no_parameters() {
        let dir = TempDir::new("empty-params");
        dir.write("plain.yaml", "title: Plain\nparameters: []\n");
        let mined = mine(&recipes(vec![dir.0.clone()], vec!["plain"])).expect("mine");
        assert_eq!(mined.len(), 1);
        assert!(!mined[0].parameterised);
    }

    #[test]
    fn an_enabled_id_that_does_not_exist_is_a_typo_worth_failing_on() {
        let dir = TempDir::new("unknown-enabled");
        dir.write("real.yaml", "title: Real\n");
        let err = mine(&recipes(vec![dir.0.clone()], vec!["reel"])).unwrap_err();
        assert!(matches!(err, RecipeError::UnknownEnabled { .. }), "{err}");
        assert!(err.to_string().contains("reel"), "{err}");
    }

    #[test]
    fn a_missing_search_path_is_skipped_not_an_error() {
        let mined = mine(&recipes(
            vec![PathBuf::from("/definitely/not/a/recipe/dir")],
            vec![],
        ))
        .expect("a fresh host with no recipes is normal");
        assert!(mined.is_empty());
    }

    #[test]
    fn a_malformed_recipe_names_the_file() {
        let dir = TempDir::new("malformed");
        let path = dir.write("broken.yaml", "title: [unclosed\n");
        let err = mine(&recipes(vec![dir.0.clone()], vec!["broken"])).unwrap_err();
        assert!(matches!(err, RecipeError::Parse { .. }), "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn non_recipe_files_are_ignored() {
        let dir = TempDir::new("junk");
        dir.write("notes.txt", "not a recipe at all\n");
        dir.write("real.yml", "title: Real\n");
        let mined = mine(&recipes(vec![dir.0.clone()], vec!["real"])).expect("mine");
        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].id, "real");
    }

    #[test]
    fn a_duplicate_id_across_search_paths_is_first_wins_not_a_failure() {
        let first = TempDir::new("dup-first");
        let second = TempDir::new("dup-second");
        first.write("shared.yaml", "title: From first\n");
        second.write("shared.yaml", "title: From second\n");

        let mined = mine(&recipes(
            vec![first.0.clone(), second.0.clone()],
            vec!["shared"],
        ))
        .expect("mine");
        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].title.as_deref(), Some("From first"));
    }
}
