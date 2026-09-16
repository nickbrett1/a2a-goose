//! Configuration: one file per host, three layers of precedence.
//!
//! `env > file > defaults`. The file defaults to
//! `$A2A_GOOSE_CONFIG`, then `~/.config/a2a-goose/config.yaml`; a *missing*
//! default file is not an error, but a missing `publicUrl` or `allowedRoots` is.
//! Secrets are never written here: every `*Env` field holds the **name** of an
//! environment variable, and the value is read from the process environment —
//! which on a host means `ENV_FILE`, sourced by `scripts/fetch-launch.sh`
//! before `exec` (see `LAUNCHING.md`).
//!
//! The validation in [`Config::validate`] is deliberately a *list of refusals*,
//! not a set of warnings. Every one of them is a state in which the agent would
//! start, advertise skills in the LiteLLM registry, and then fail or — worse —
//! behave in a way nobody can see (a loopback `publicUrl` the LiteLLM container
//! can never dial, a `cwd` allowlist that isn't there). Starting is not a
//! courtesy: refuse, and say why.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Overrides the config file path. Set by the deploy units and by tests.
pub const CONFIG_ENV: &str = "A2A_GOOSE_CONFIG";

/// `A2A_GOOSE_BIND` — the listen address. An env override because it is the one
/// field that plausibly differs between a laptop and a server on the same host
/// image, and because `axum::serve` needs it before the rest of the config is
/// interesting.
pub const BIND_ENV: &str = "A2A_GOOSE_BIND";

/// `A2A_GOOSE_PUBLIC_URL` — what goes on the card as the agent's address.
pub const PUBLIC_URL_ENV: &str = "A2A_GOOSE_PUBLIC_URL";

const DEFAULT_BIND: &str = "127.0.0.1:10001";

/// The protocol versions LiteLLM accepts at registration (memo
/// `litellm-agent-registry-v1`: "Only `0.3` and `1.0` are accepted; anything
/// else is HTTP 400"). Anything else is a startup failure here, so the mistake
/// surfaces on the host rather than in the registry.
pub const ACCEPTED_PROTOCOL_VERSIONS: [&str; 2] = ["0.3", "1.0"];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub card: Card,
    #[serde(default)]
    pub skills: Skills,
    #[serde(default)]
    pub goose: Goose,
    #[serde(default)]
    pub registry: Registry,
    #[serde(default)]
    pub executor: Executor,
    #[serde(default)]
    pub tracing: Tracing,
    #[serde(default)]
    pub observability: Observability,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Server {
    /// Where the A2A surface listens. Loopback by default; a Tailscale address
    /// is the usual override.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// What goes on the card as the agent's address, and therefore what LiteLLM
    /// dials. Must not be loopback — the dialer is the LiteLLM *container*, and
    /// `127.0.0.1` there is the container itself (S9).
    #[serde(default)]
    pub public_url: String,
    /// Name of the environment variable holding the bearer token. Constraint
    /// #3: A2A has no authentication of its own, so every `POST /` carries a
    /// token even though the tailnet ACL is the real boundary.
    #[serde(default = "default_bearer_token_env")]
    pub bearer_token_env: String,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            public_url: String::new(),
            bearer_token_env: default_bearer_token_env(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Card {
    #[serde(default = "default_card_name")]
    pub name: String,
    #[serde(default = "default_card_description")]
    pub description: String,
    /// Pinned, never inferred. An unpinned agent serves 0.3-shaped responses to
    /// callers that send no `a2a-version` header (S7).
    #[serde(default = "default_protocol_version")]
    pub protocol_version: String,
    /// The card's own version, which is this binary's version. A card that
    /// changes must re-register (§6.2), and a card that lies about its version
    /// makes that diff unreadable.
    #[serde(default = "default_card_version")]
    pub version: String,
}

impl Default for Card {
    fn default() -> Self {
        Self {
            name: default_card_name(),
            description: default_card_description(),
            protocol_version: default_protocol_version(),
            version: default_card_version(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Skills {
    /// The only built-in skill, always present, never removable and never
    /// redefinable (constraint #10). It makes an omitted `metadata.skillId`
    /// safe: a bare goose session with no instruction attached.
    #[serde(default = "default_skill")]
    pub default: String,
    #[serde(default)]
    pub recipes: Recipes,
    /// Where a hand-written skill file lives, for anything with no recipe
    /// behind it. One skill per file.
    #[serde(default = "default_skills_d")]
    pub d: PathBuf,
    /// Per-id overrides of the projected display fields. Ids are the stable
    /// contract; `name` and `description` may change freely.
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
}

impl Default for Skills {
    fn default() -> Self {
        Self {
            default: default_skill(),
            recipes: Recipes::default(),
            d: default_skills_d(),
            overrides: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recipes {
    /// Directories searched for goose recipe YAML. The agent adds none of its
    /// own and reads only what it is told to (S10).
    #[serde(default)]
    pub search_paths: Vec<PathBuf>,
    /// An allowlist. A recipe that is not listed here is not advertised, so an
    /// empty list is a valid, deliberate configuration: the card carries `ask`
    /// plus whatever `skills.d/` declares.
    ///
    /// This is load-bearing rather than cosmetic because a recipe can pull in
    /// `extensions` and take `parameters` (S10) — mining is not a sandbox.
    #[serde(default)]
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Override {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Goose {
    #[serde(default)]
    pub acp: Acp,
    #[serde(default)]
    pub sessions: Sessions,
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Acp {
    #[serde(default = "default_acp_url")]
    pub url: String,
    #[serde(default = "default_goose_secret_env")]
    pub secret_env: String,
    #[serde(default)]
    pub timeouts: Timeouts,
}

impl Default for Acp {
    fn default() -> Self {
        Self {
            url: default_acp_url(),
            secret_env: default_goose_secret_env(),
            timeouts: Timeouts::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Timeouts {
    #[serde(default = "default_initialize_secs")]
    pub initialize_secs: u64,
    /// A turn is an agent loop, not a request. This is the ceiling for a whole
    /// `session/prompt`, so it is minutes, not seconds.
    #[serde(default = "default_prompt_secs")]
    pub prompt_secs: u64,
    #[serde(default = "default_cancel_secs")]
    pub cancel_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            initialize_secs: default_initialize_secs(),
            prompt_secs: default_prompt_secs(),
            cancel_secs: default_cancel_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sessions {
    /// The `contextId` → `sessionId` map. Not the conversation history: that is
    /// goose's own `sessions.db`, which is the system of record (constraint #1).
    #[serde(default = "default_sessions_db")]
    pub db_path: PathBuf,
    #[serde(default = "default_true")]
    pub reuse_by_context: bool,
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            db_path: default_sessions_db(),
            reuse_by_context: true,
            idle_ttl_secs: default_idle_ttl(),
            max_sessions: default_max_sessions(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Defaults {
    /// Handed to `session/new`. Caller-supplied `metadata.cwd` overrides it, and
    /// is checked against `allowed_roots` first.
    #[serde(default)]
    pub cwd: PathBuf,
    /// The security boundary (constraint #4). A missing list is a startup
    /// failure: because the agent runs as the host user, a traversal is bounded
    /// by that user's own permissions — a weaker boundary than a container, not
    /// a stronger one.
    #[serde(default)]
    pub allowed_roots: Vec<PathBuf>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            allowed_roots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    #[serde(default = "default_litellm_base_url")]
    pub litellm_base_url: String,
    #[serde(default = "default_master_key_env")]
    pub master_key_env: String,
    #[serde(default = "default_card_name")]
    pub agent_name: String,
    /// Whether this process may **rewrite** a registry entry that already exists
    /// under this host's name. On (the default) a restart converges the entry
    /// onto the card this process is serving — `PUT /v1/agents/{id}`, in place,
    /// because a second `POST` is a 400 on a duplicate name and a
    /// `DELETE`-then-`POST` would leave a window with no entry. Off, the entry
    /// is adopted as-is, for a host whose registry entry is managed elsewhere.
    /// (M4 narrows this further, to "only when the card hash actually changed",
    /// which it can compare in memory without persisting anything.)
    #[serde(default = "default_true")]
    pub re_register_on_card_change: bool,
    /// Loop bounds, not budgets. Deliberately counts of things that are visible
    /// in the ACP stream, never dollars — see `spikes/S2.md` and
    /// [`crate::config::Attribution`].
    #[serde(default)]
    pub limits: Limits,
    /// Visibility, which is a different thing from a budget.
    #[serde(default)]
    pub attribution: Attribution,
    /// Parked, deliberately, and not a TODO. Revisit only if a goose release
    /// ships `session_id_header_override` *and* a ceiling is actually wanted.
    #[serde(default)]
    pub per_thread_budget: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            litellm_base_url: default_litellm_base_url(),
            master_key_env: default_master_key_env(),
            agent_name: default_card_name(),
            re_register_on_card_change: true,
            limits: Limits::default(),
            attribution: Attribution::default(),
            per_thread_budget: false,
        }
    }
}

/// Agent-enforced loop bounds (the S2 decision: bound the loop, not the wallet).
///
/// Measured on one trivial turn, goose's ACP stream and LiteLLM's spend log
/// agree *exactly* on tokens but price the same call 3.5x apart, because each is
/// a local price table and neither is the provider's invoice. A proxy-side
/// dollar budget would therefore be enforced against one system's guess. These
/// are counts, they need no cooperation from the proxy, and they hold even if
/// the proxy is misconfigured.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// Consecutive goose turns for one A2A task.
    #[serde(default = "default_max_iterations")]
    pub max_iterations_per_task: u32,
    /// Across the whole host, not per context.
    #[serde(default = "default_max_concurrent_sessions")]
    pub max_concurrent_sessions: usize,
    /// From `usage_update.used`, summed over the task.
    #[serde(default = "default_max_tokens_per_context")]
    pub max_tokens_per_context: u64,
    #[serde(default = "default_max_wall_clock")]
    pub max_wall_clock_seconds_per_task: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_iterations_per_task: default_max_iterations(),
            max_concurrent_sessions: default_max_concurrent_sessions(),
            max_tokens_per_context: default_max_tokens_per_context(),
            max_wall_clock_seconds_per_task: default_max_wall_clock(),
        }
    }
}

/// How this host's traffic is distinguishable in LiteLLM's spend logs.
///
/// Attribution only. Neither route carries a budget: a per-agent virtual key
/// makes LiteLLM's own aggregates split per agent, and a `User-Agent` rides
/// `LITELLM_CUSTOM_HEADERS` onto `metadata.user_agent` on every row. Both were
/// verified against `nas:4000` (see the addendum in `spikes/S2.md`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attribution {
    /// One distinct name per host, e.g. `a2a-goose/mac-studio`.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Optional. Names the environment variable holding a per-agent key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
}

impl Default for Attribution {
    fn default() -> Self {
        Self {
            user_agent: default_user_agent(),
            key_env: None,
        }
    }
}

fn default_max_iterations() -> u32 {
    12
}

fn default_max_concurrent_sessions() -> usize {
    4
}

fn default_max_tokens_per_context() -> u64 {
    400_000
}

fn default_max_wall_clock() -> u64 {
    900
}

fn default_user_agent() -> String {
    "a2a-goose".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Executor {
    /// On a client disconnect mid-turn: `cancel` or `orphan`. `cancel` is the
    /// default — a disconnected caller must not leave a turn burning tokens, and
    /// a silent orphan is the one outcome nobody can see.
    #[serde(default = "default_on_client_disconnect")]
    pub on_client_disconnect: OnClientDisconnect,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            on_client_disconnect: default_on_client_disconnect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OnClientDisconnect {
    Cancel,
    Orphan,
}

fn default_on_client_disconnect() -> OnClientDisconnect {
    OnClientDisconnect::Cancel
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tracing {
    #[serde(default = "default_trace_id_header")]
    pub trace_id_header: String,
    #[serde(default = "default_trace_id_source")]
    pub trace_id_source: String,
    #[serde(default = "default_true")]
    pub forward_to_goose: bool,
}

impl Default for Tracing {
    fn default() -> Self {
        Self {
            trace_id_header: default_trace_id_header(),
            trace_id_source: default_trace_id_source(),
            forward_to_goose: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observability {
    #[serde(default)]
    pub phoenix: Phoenix,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Phoenix {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}

fn default_bearer_token_env() -> String {
    "A2A_GOOSE_BEARER_TOKEN".to_string()
}

fn default_card_name() -> String {
    "a2a-goose".to_string()
}

fn default_card_description() -> String {
    "goose, via A2A".to_string()
}

fn default_protocol_version() -> String {
    "1.0".to_string()
}

fn default_card_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn default_skill() -> String {
    crate::skills::ASK_ID.to_string()
}

fn default_skills_d() -> PathBuf {
    PathBuf::from("~/.config/a2a-goose/skills.d")
}

fn default_acp_url() -> String {
    "http://127.0.0.1:3284/acp".to_string()
}

fn default_goose_secret_env() -> String {
    "GOOSE_SERVER__SECRET_KEY".to_string()
}

fn default_initialize_secs() -> u64 {
    10
}

fn default_prompt_secs() -> u64 {
    900
}

fn default_cancel_secs() -> u64 {
    10
}

fn default_sessions_db() -> PathBuf {
    PathBuf::from("~/.local/share/a2a-goose/sessions.db")
}

fn default_idle_ttl() -> u64 {
    3600
}

fn default_max_sessions() -> usize {
    16
}

fn default_litellm_base_url() -> String {
    "http://nas:4000".to_string()
}

fn default_master_key_env() -> String {
    "LITELLM_MASTER_KEY".to_string()
}

fn default_trace_id_header() -> String {
    "x-litellm-trace-id".to_string()
}

fn default_trace_id_source() -> String {
    "contextId".to_string()
}

fn default_true() -> bool {
    true
}

/// Every way starting can be refused. Each variant names the setting and what is
/// wrong with it — an operator reading a launchd log at 2am should not have to
/// guess.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    MissingPublicUrl,
    LoopbackPublicUrl {
        public_url: String,
    },
    UnsupportedProtocolVersion {
        found: String,
    },
    MissingAllowedRoots,
    MissingDefaultCwd,
    UnknownDefaultSkill {
        default: String,
        known: Vec<String>,
    },
    UnknownOverride {
        id: String,
    },
    BindNotASocketAddress {
        bind: String,
        source: std::net::AddrParseError,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "cannot parse {}: {source}", path.display())
            }
            Self::MissingPublicUrl => write!(
                f,
                "server.publicUrl is not set. It is what goes on the agent card and what LiteLLM \
                 dials, so there is no safe default - it has to be an address the LiteLLM \
                 container can reach (a Tailscale name or IP, not 127.0.0.1)"
            ),
            Self::LoopbackPublicUrl { public_url } => write!(
                f,
                "server.publicUrl is loopback ({public_url}). The dialer is the LiteLLM \
                 container, so 127.0.0.1 there is the container itself, not this host (S9)"
            ),
            Self::UnsupportedProtocolVersion { found } => write!(
                f,
                "card.protocolVersion is {found:?}; LiteLLM accepts only {} at registration",
                ACCEPTED_PROTOCOL_VERSIONS.join(" and ")
            ),
            Self::MissingDefaultCwd => write!(
                f,
                "goose.defaults.cwd is not set. It is the directory a turn runs in when the \
                 caller names none, and without it every such turn is refused - a host that \
                 boots into that state looks healthy and answers every call with an error"
            ),
            Self::MissingAllowedRoots => write!(
                f,
                "goose.defaults.allowedRoots is empty. A caller-supplied cwd reaches the \
                 filesystem as this process's user, so the allowlist is the only authority on \
                 what a caller may point at - it cannot be absent (constraint #4)"
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
            Self::BindNotASocketAddress { bind, source } => {
                write!(f, "server.bind={bind:?} is not a socket address: {source}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Loads the config from `$A2A_GOOSE_CONFIG`, else `~/.config/a2a-goose/config.yaml`,
    /// applies environment overrides, and validates.
    pub fn load() -> Result<Self, ConfigError> {
        let explicit = std::env::var(CONFIG_ENV).ok();
        let path = explicit
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| expand_tilde(&PathBuf::from("~/.config/a2a-goose/config.yaml")));
        Self::load_from(&path, explicit.is_some())
    }

    /// Loads from `path`. `required` distinguishes "the operator named this file"
    /// from "the default file happens not to exist": the first is a typo worth
    /// failing on, the second is a fresh host that will be validated anyway.
    pub fn load_from(path: &Path, required: bool) -> Result<Self, ConfigError> {
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            serde_yaml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?
        } else if required {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{CONFIG_ENV} names a file that does not exist"),
                ),
            });
        } else {
            Config::default()
        };

        config.apply_env();
        config.expand_paths();
        config.validate()?;
        Ok(config)
    }

    /// Env overrides for the fields that legitimately differ per host. Deliberately
    /// short: everything else lives in the file, so there is one place to read.
    fn apply_env(&mut self) {
        if let Ok(bind) = std::env::var(BIND_ENV) {
            self.server.bind = bind;
        }
        if let Ok(public_url) = std::env::var(PUBLIC_URL_ENV) {
            self.server.public_url = public_url;
        }
        if let Ok(base_url) = std::env::var("LITELLM_BASE_URL") {
            self.registry.litellm_base_url = base_url;
        }
    }

    /// `~` in a path is a config-file convenience, not something the OS expands.
    fn expand_paths(&mut self) {
        self.skills.d = expand_tilde(&self.skills.d);
        self.skills.recipes.search_paths = self
            .skills
            .recipes
            .search_paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect();
        self.goose.sessions.db_path = expand_tilde(&self.goose.sessions.db_path);
        self.goose.defaults.cwd = expand_tilde(&self.goose.defaults.cwd);
        self.goose.defaults.allowed_roots = self
            .goose
            .defaults
            .allowed_roots
            .iter()
            .map(|path| expand_tilde(path))
            .collect();
    }

    /// The refusals. See the module comment for why these are fatal and not
    /// warnings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.public_url.trim().is_empty() {
            return Err(ConfigError::MissingPublicUrl);
        }
        if is_loopback_url(&self.server.public_url) {
            return Err(ConfigError::LoopbackPublicUrl {
                public_url: self.server.public_url.clone(),
            });
        }
        if !ACCEPTED_PROTOCOL_VERSIONS.contains(&self.card.protocol_version.as_str()) {
            return Err(ConfigError::UnsupportedProtocolVersion {
                found: self.card.protocol_version.clone(),
            });
        }
        if self.goose.defaults.allowed_roots.is_empty() {
            return Err(ConfigError::MissingAllowedRoots);
        }
        // Not a filesystem check - a fresh host may not have the directory yet.
        // An *empty* default, though, is a configuration mistake with a
        // predictable outcome: every turn that does not name a `cwd` is
        // refused, so the agent answers every call with an error while looking
        // perfectly healthy.
        if self.goose.defaults.cwd.as_os_str().is_empty() {
            return Err(ConfigError::MissingDefaultCwd);
        }
        // Parsed here rather than at bind time so a typo is a startup failure
        // with the field named, not a bare `AddrParseError` from inside the
        // runtime.
        self.server
            .bind
            .parse::<std::net::SocketAddr>()
            .map_err(|source| ConfigError::BindNotASocketAddress {
                bind: self.server.bind.clone(),
                source,
            })?;
        Ok(())
    }

    /// The bearer token, read from the environment variable it is *named* by.
    ///
    /// A separate call rather than a field on `Config` because the value must
    /// never be serialised, logged or written into the map beside the sessions
    /// (constraint #3, and the reason every secret here is a `*Env` name).
    pub fn bearer_token(&self) -> Result<String, ConfigError> {
        std::env::var(&self.server.bearer_token_env).map_err(|_| ConfigError::Read {
            path: PathBuf::from(format!("env:{}", self.server.bearer_token_env)),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the bearer token is not set. A2A has no authentication of its own, so this \
                 endpoint refuses to start without one (constraint #3)",
            ),
        })
    }

    /// The LiteLLM master key, read from the environment variable it is named by.
    pub fn litellm_master_key(&self) -> Option<String> {
        std::env::var(&self.registry.master_key_env).ok()
    }
}

/// Is this URL (or bare address) pointing at the loopback interface?
///
/// `localhost`, `127.0.0.0/8` and `[::1]` all mean "this process" — and the
/// process that dials the card is the LiteLLM container, where that is a
/// different machine's idea of here.
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Strip userinfo and port. `[::1]:10001` keeps its brackets.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped
            .split_once(']')
            .map(|(host, _)| host)
            .unwrap_or(stripped)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };

    if host.eq_ignore_ascii_case("localhost") || host == "::1" || host == "0:0:0:0:0:0:0:1" {
        return true;
    }
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(v4) => v4.is_loopback(),
        Err(_) => false,
    }
}

/// Expands a leading `~` (and only a leading `~`) using `$HOME`.
///
/// Deliberately not a general path expander: `~user/` is not supported because
/// nothing here needs it, and pretending to support it would be a silent
/// wrong-answer on a host with several users.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_path_buf();
    }
    match std::env::var_os("HOME") {
        Some(home) => {
            let mut expanded = PathBuf::from(home);
            if !rest.is_empty() {
                expanded.push(rest.trim_start_matches('/'));
            }
            expanded
        }
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
        config.goose.defaults.cwd = PathBuf::from("/tmp");
        config
    }

    #[test]
    fn a_host_with_no_default_cwd_is_refused_rather_than_answering_every_call_with_an_error() {
        let mut config = valid();
        config.goose.defaults.cwd = PathBuf::new();
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::MissingDefaultCwd), "{err}");
        assert!(err.to_string().contains("goose.defaults.cwd"), "{err}");
    }

    #[test]
    fn a_bare_config_refuses_to_start_without_a_public_url() {
        let err = Config::default().validate().unwrap_err();
        assert!(matches!(err, ConfigError::MissingPublicUrl), "{err}");
        assert!(err.to_string().contains("publicUrl"), "{err}");
    }

    #[test]
    fn a_loopback_public_url_is_refused() {
        for url in [
            "http://127.0.0.1:10001",
            "http://localhost:10001",
            "http://[::1]:10001",
            "http://127.9.9.9/x",
            "127.0.0.1:10001",
        ] {
            assert!(is_loopback_url(url), "{url} should look loopback");
            let mut config = valid();
            config.server.public_url = url.to_string();
            assert!(
                matches!(
                    config.validate().unwrap_err(),
                    ConfigError::LoopbackPublicUrl { .. }
                ),
                "{url} should be refused"
            );
        }
    }

    #[test]
    fn a_tailnet_url_is_not_loopback() {
        for url in [
            "http://mac-studio.tail86fd19.ts.net:10001",
            "http://100.101.102.103:10001",
            "http://nas:10001",
        ] {
            assert!(!is_loopback_url(url), "{url} should not look loopback");
        }
    }

    #[test]
    fn the_protocol_version_is_restricted_to_what_litellm_accepts() {
        let mut config = valid();
        config.card.protocol_version = "2.0".to_string();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::UnsupportedProtocolVersion { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("0.3 and 1.0"), "{err}");

        for accepted in ACCEPTED_PROTOCOL_VERSIONS {
            config.card.protocol_version = accepted.to_string();
            config.validate().expect("accepted version");
        }
    }

    #[test]
    fn an_empty_allowlist_is_refused() {
        let mut config = valid();
        config.goose.defaults.allowed_roots.clear();
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::MissingAllowedRoots), "{err}");
        assert!(err.to_string().contains("allowedRoots"), "{err}");
    }

    #[test]
    fn a_bad_bind_address_is_refused_by_name() {
        let mut config = valid();
        config.server.bind = "not-an-address".to_string();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::BindNotASocketAddress { .. }),
            "{err}"
        );
        assert!(
            err.to_string().contains("10001") || err.to_string().contains("bind"),
            "{err}"
        );
    }

    #[test]
    fn env_overrides_win_over_the_file() {
        let dir = std::env::temp_dir().join(format!("a2a-goose-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "server:\n  publicUrl: \"http://from-file:10001\"\n  bind: \"127.0.0.1:1\"\n\
             goose:\n  defaults:\n    allowedRoots: [\"/tmp\"]\n    cwd: \"/tmp\"\n",
        )
        .expect("write config");

        // SAFETY: single-threaded test body; the variables are removed before we
        // return, and no other test reads these names.
        unsafe {
            std::env::set_var(PUBLIC_URL_ENV, "http://from-env:10001");
            std::env::set_var(BIND_ENV, "127.0.0.1:2");
        }
        let config = Config::load_from(&path, true).expect("load");
        unsafe {
            std::env::remove_var(PUBLIC_URL_ENV);
            std::env::remove_var(BIND_ENV);
        }

        assert_eq!(config.server.public_url, "http://from-env:10001");
        assert_eq!(config.server.bind, "127.0.0.1:2");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unknown_key_is_a_parse_error_not_a_silent_default() {
        let dir = std::env::temp_dir().join(format!("a2a-goose-config-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.yaml");
        std::fs::write(&path, "server:\n  publicURL: \"http://typo:10001\"\n").expect("write");
        let err = Config::load_from(&path, true).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_named_file_that_does_not_exist_is_an_error() {
        let err = Config::load_from(Path::new("/definitely/not/here.yaml"), true).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err}");
    }

    #[test]
    fn tilde_expands_only_a_leading_home() {
        // SAFETY: this test only reads HOME, and does not change it.
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(
            expand_tilde(Path::new("~/.config/goose/recipes")),
            PathBuf::from(&home).join(".config/goose/recipes")
        );
        assert_eq!(
            expand_tilde(Path::new("/abs/~/not-home")),
            PathBuf::from("/abs/~/not-home")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/x")),
            PathBuf::from("~other/x")
        );
    }
}
