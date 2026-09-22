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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::acp::{ACP_PATH, AcpAddressError, acp_address, acp_endpoint};

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
    /// The roost mission-control hub this agent dials out to (M2a). Off by
    /// default: a host with no hub configured must not dial one.
    #[serde(default)]
    pub hub: Hub,
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
    /// Where the ACP endpoint lives on the host's `goose serve`.
    ///
    /// Written either as the endpoint (`http://127.0.0.1:3284/acp`) or as the
    /// origin (`http://127.0.0.1:3284`); [`crate::acp::acp_endpoint`] normalises
    /// both to the same URL, so neither spelling is a trap.
    #[serde(default = "default_acp_url")]
    pub url: String,
    /// The *name* of the variable holding goose's `X-Secret-Key`, never the key.
    ///
    /// Read by [`crate::acp::secret_key`] and sent on every ACP request, and —
    /// when this agent owns the server — handed to the child as goose's own
    /// `GOOSE_SERVER__SECRET_KEY`. The name is ours; the value is the host's.
    #[serde(default = "default_goose_secret_env")]
    pub secret_env: String,
    /// Who starts and keeps up `goose serve`.
    ///
    /// `own` means this agent spawns it, restarts it if it dies, and refuses to
    /// start if something is *already* listening on the address above — so a
    /// host with a hand-started goose is told so at boot rather than quietly
    /// running against a server it does not control. `external` means the host
    /// starts goose itself (init unit, systemd, by hand): nothing is spawned and
    /// nothing is checked.
    ///
    /// `own` is the default because it is the one that cannot serve a card it
    /// cannot fulfil: the agent waits for goose to answer `initialize` before it
    /// binds a port, so "the agent is up, goose is not" is not a state a caller
    /// can ever see.
    #[serde(default = "default_serve_mode")]
    pub serve: ServeMode,
    /// Start an owned `goose serve` without authentication.
    ///
    /// The only way to run an owned goose with no key, because goose refuses to
    /// start without one: `GOOSE_SERVER__SECRET_KEY must be set to start `goose
    /// serve`; pass --dangerously-unauthenticated to run without ACP
    /// authentication`. Setting this *passes that flag* to the child, so the ACP
    /// endpoint accepts any caller that can reach it — the loopback bind
    /// `own` requires is then the whole boundary. It is therefore a deliberate
    /// act, not a fallback: with it unset and no key, startup refuses.
    #[serde(default)]
    pub unauthenticated: bool,
    #[serde(default)]
    pub timeouts: Timeouts,
}

impl Default for Acp {
    fn default() -> Self {
        Self {
            url: default_acp_url(),
            secret_env: default_goose_secret_env(),
            serve: default_serve_mode(),
            unauthenticated: false,
            timeouts: Timeouts::default(),
        }
    }
}

/// Who starts `goose serve` — see [`Acp::serve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServeMode {
    Own,
    External,
}

impl ServeMode {
    /// The word as it appears in config, logs and `/status`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::External => "external",
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
    /// Where this host remembers the `agent_id` LiteLLM gave it.
    ///
    /// Not bookkeeping: the id is the only handle a filtered listing cannot take
    /// away. `GET /v1/agents` is a *view* — LiteLLM 1.103.x returns only the rows
    /// the calling key owns, so a row written before that filter existed (or by
    /// another key) is absent from the listing while remaining present, callable
    /// and addressable by `GET /v1/agents/{id}`. Without the id, such a host
    /// cannot find itself, `POST`s a duplicate name, and reports itself
    /// unregistered while the proxy still holds a working entry. With it, the
    /// lookup degrades to the by-id call and converges.
    #[serde(default = "default_registry_agent_id_path")]
    pub agent_id_path: PathBuf,
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
            agent_id_path: default_registry_agent_id_path(),
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
    #[serde(default)]
    pub activity: Activity,
}

/// The activity feed `GET /events` serves (see `crate::activity`).
///
/// On by default because it is bounded, in-memory and behind the bearer token:
/// the whole point is that an operator can *see* a turn while it is running, and
/// a viewer that has to be switched on first is a viewer nobody has when the
/// thing they need to watch is misbehaving. Off is for a host that would rather
/// not have prompt-adjacent detail in the process memory at all, or one being
/// profiled.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activity {
    /// When false, nothing is recorded and `GET /events` is refused with `403`
    /// rather than silently serving an empty stream — a disabled feed and a feed
    /// with nothing to say are different answers, and only one of them is a
    /// problem.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How many past events a newly attached viewer is handed. Clamped to
    /// `1..=crate::activity::MAX_BACKLOG`.
    #[serde(default = "default_activity_backlog")]
    pub backlog: usize,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            enabled: true,
            backlog: default_activity_backlog(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Phoenix {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
}

/// The roost mission-control hub this agent tunnels out to (`docs/roost-tunnel.md`).
///
/// The hub is a **router, not a store**: this agent dials *out* over one
/// long-lived WebSocket and pushes its activity feed, so nothing here opens an
/// inbound surface. The whole block is optional and off by default, and an
/// unreachable hub is a retry and never a crash — so a `hub` mistake costs a log
/// line, not the agent's ability to serve.
///
/// Secrets follow the repo's rule (constraint #3): `credentialEnv` holds the
/// **name** of an environment variable, never the value.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hub {
    /// Whether this agent dials a hub at all. When false, no tunnel task is
    /// started and nothing about the hub is dialled.
    #[serde(default)]
    pub enabled: bool,
    /// The hub's agent WebSocket endpoint, e.g. `ws://roost:3000/agent/ws`.
    /// Must be `ws://` or `wss://`.
    #[serde(default)]
    pub url: String,
    /// The *name* of the environment variable holding this agent's hub
    /// credential. The credential is the entire auth boundary between an agent
    /// and its hub (there is no browser login), so it is a variable name here
    /// and a value only in the environment.
    #[serde(default = "default_hub_credential_env")]
    pub credential_env: String,
    /// The `kind` the hub shows in its fleet view. The real value is
    /// `a2a-goose`; roost's `fake_agent` uses `devcontainer`.
    #[serde(default = "default_hub_kind")]
    pub kind: String,
    /// How long to wait for the hub to answer the WebSocket handshake before
    /// giving up on the attempt and backing off.
    #[serde(default = "default_hub_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// The bounded idle deadline on the tunnel read: if no frame arrives from
    /// the hub for this many seconds the socket is treated as half-open (the
    /// peer gone with no FIN/RST) and the tunnel reconnects through the normal
    /// backoff. Must comfortably exceed the hub's `status_poll_ms` (roost's
    /// default is 15 s) so a healthy tunnel is never dropped on jitter; the
    /// default of 90 s is six poll intervals. See [`crate::tunnel`].
    #[serde(default = "default_hub_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            credential_env: default_hub_credential_env(),
            kind: default_hub_kind(),
            connect_timeout_secs: default_hub_connect_timeout_secs(),
            idle_timeout_secs: default_hub_idle_timeout_secs(),
        }
    }
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

fn default_serve_mode() -> ServeMode {
    ServeMode::Own
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

/// Beside `sessions.db`: this is state this process owns, not configuration an
/// operator edits.
fn default_registry_agent_id_path() -> PathBuf {
    PathBuf::from("~/.local/share/a2a-goose/registry-agent-id")
}

fn default_activity_backlog() -> usize {
    crate::activity::DEFAULT_BACKLOG
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

fn default_hub_credential_env() -> String {
    "A2A_GOOSE_HUB_TOKEN".to_string()
}

fn default_hub_kind() -> String {
    crate::tunnel::identity::DEFAULT_KIND.to_string()
}

fn default_hub_connect_timeout_secs() -> u64 {
    10
}

/// Six roost status polls (15 s each): see [`crate::tunnel::IDLE_TIMEOUT_DEFAULT`]
/// for why the idle window is this size.
fn default_hub_idle_timeout_secs() -> u64 {
    90
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
    /// `goose.acp.url` cannot be taken apart, so an owned goose has no address.
    OwnedAcpAddress {
        url: String,
        source: AcpAddressError,
    },
    /// Only `http` is owned: TLS would mean handing goose a certificate, which
    /// is a decision nobody has made, and anything else is not a scheme goose
    /// serves.
    OwnedAcpNotHttp {
        url: String,
        scheme: String,
    },
    /// goose's `--host` is a socket address, not a name: it answers
    /// `invalid socket address syntax` to `localhost` and to `::1`.
    OwnedAcpHostIsNotAnAddress {
        url: String,
        host: String,
    },
    OwnedAcpNotLoopback {
        url: String,
    },
    /// A path prefix or a query means a proxy in front of goose, and an owned
    /// goose serves `/acp` and nothing else — so the address would not be the
    /// endpoint.
    OwnedAcpNotTheEndpoint {
        url: String,
        dialled: String,
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
            Self::OwnedAcpAddress { url, source } => write!(
                f,
                "goose.acp.serve is \"own\", so goose.acp.url has to name the address this agent \
                 should start goose on, and {url:?} cannot be read as one: {source}"
            ),
            Self::OwnedAcpNotHttp { url, scheme } => write!(
                f,
                "goose.acp.serve is \"own\", which starts a plain-http goose: goose.acp.url is \
                 {scheme:?} ({url:?}). Either write it as http://127.0.0.1:<port>/acp, or set \
                 goose.acp.serve to \"external\" and start goose yourself"
            ),
            Self::OwnedAcpHostIsNotAnAddress { url, host } => write!(
                f,
                "goose.acp.serve is \"own\" and goose.acp.url names the host {host:?}, but goose's \
                 --host takes an IPv4 address and not a name (it answers \"invalid socket address \
                 syntax\" to `localhost` and to `::1`). Write {url:?} with 127.0.0.1 in place of \
                 {host:?}"
            ),
            Self::OwnedAcpNotLoopback { url } => write!(
                f,
                "goose.acp.serve is \"own\", so the server would run on *this* machine, but \
                 goose.acp.url ({url:?}) is not a loopback address. An agent cannot start a goose \
                 on another host: point the URL at 127.0.0.1 (or whatever loopback this host \
                 uses), or set goose.acp.serve to \"external\" and let that host own its goose"
            ),
            Self::OwnedAcpNotTheEndpoint { url, dialled } => write!(
                f,
                "goose.acp.serve is \"own\", which serves /acp and nothing else, but \
                 goose.acp.url ({url:?}) resolves to {dialled} — a path prefix or a query means \
                 something is in front of goose, and this agent would be starting a server at an \
                 address no request goes to. Use the bare endpoint, or set goose.acp.serve to \
                 \"external\""
            ),
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
        self.registry.agent_id_path = expand_tilde(&self.registry.agent_id_path);
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
        self.validate_owned_acp()?;
        Ok(())
    }

    /// The misconfigurations that are reported and not refused.
    ///
    /// One so far, and it is the shape of an agent nothing can dial: a loopback
    /// `bind` with a non-loopback `publicUrl`. It is not refused because it is
    /// also legitimate — something may *front* the port, in which case `publicUrl`
    /// names the front and `bind` names the loopback behind it. But the dialer
    /// for `card.url` is the LiteLLM container, in its own network namespace, so
    /// when nothing fronts the port the agent registers an address that answers
    /// nothing and every call fails with a connection error while the agent looks
    /// healthy. Measured exactly that way (spikes/S15.md §6); the two cases are
    /// indistinguishable from here, so this is a warning with the addresses in it.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if bind_is_loopback(&self.server.bind) && !is_loopback_url(&self.server.public_url) {
            warnings.push(format!(
                "server.bind is loopback ({}) but server.publicUrl is not ({}). The dialer for \
                 card.url is the LiteLLM container, so unless something *fronts* this port the \
                 agent is unreachable at the URL it registers - every call fails with a \
                 connection error. Bind the address publicUrl names, not 127.0.0.1 (spikes/S15.md)",
                self.server.bind, self.server.public_url
            ));
        }
        // The hub is optional, so a hub mistake is a warning and not a refusal:
        // an agent that cannot tunnel is still an agent that can serve. But a
        // `hub.enabled` that cannot possibly connect is worth saying out loud at
        // boot rather than discovering from a log line every 30 seconds.
        if self.hub.enabled && !is_websocket_url(&self.hub.url) {
            warnings.push(format!(
                "hub.enabled is set but hub.url ({:?}) is not a ws:// or wss:// URL, so no \
                 tunnel will be opened. Set it to the hub's agent endpoint, e.g. \
                 ws://roost:3000/agent/ws",
                self.hub.url
            ));
        }
        warnings
    }

    /// The refusals that only apply when this agent starts goose itself.
    ///
    /// All four are the same mistake wearing different clothes: `goose.acp.url`
    /// is describing a server this process *cannot* start. Two addresses that
    /// disagree, or a scheme or a host goose will not bind, produce a host whose
    /// agent is healthy and whose turns all fail — which is the state owning the
    /// process exists to make impossible. So they are refusals, at startup, with
    /// the field named.
    ///
    /// Deliberately *not* here: whether a key is present. That is an environment
    /// question, not a configuration one (the same reason `bearer_token` is read
    /// where it is used), and it is asked when the child is started.
    fn validate_owned_acp(&self) -> Result<(), ConfigError> {
        if self.goose.acp.serve != ServeMode::Own {
            return Ok(());
        }
        let url = &self.goose.acp.url;

        let address = acp_address(url).map_err(|source| ConfigError::OwnedAcpAddress {
            url: url.clone(),
            source,
        })?;

        if address.scheme != "http" {
            return Err(ConfigError::OwnedAcpNotHttp {
                url: url.clone(),
                scheme: address.scheme,
            });
        }

        // goose's own `--host` argument is what decides this, and it takes an
        // IPv4 address: `goose serve --host localhost` and `--host ::1` both
        // answer `invalid socket address syntax`. Converting a name here would
        // be guessing at which of a host's addresses goose should bind.
        let Ok(loopback) = address.host.parse::<Ipv4Addr>() else {
            return Err(ConfigError::OwnedAcpHostIsNotAnAddress {
                url: url.clone(),
                host: address.host.clone(),
            });
        };
        if !loopback.is_loopback() {
            return Err(ConfigError::OwnedAcpNotLoopback { url: url.clone() });
        }

        // The child would bind `http://<host>:<port>` and serve `/acp`; if the
        // configured URL dials anything else, the two are different servers.
        let dialled = acp_endpoint(url);
        let served = format!("http://{}:{}{}", address.host, address.port, ACP_PATH);
        if dialled != served {
            return Err(ConfigError::OwnedAcpNotTheEndpoint {
                url: url.clone(),
                dialled,
            });
        }
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

    /// The hub credential, read from the environment variable `hub.credentialEnv`
    /// *names*.
    ///
    /// `None` when the variable is unset — and an agent with no credential is an
    /// agent that will not dial the hub with a bogus identity, because the
    /// tunnel refuses to open without it (`crate::tunnel::spawn`). Read here and
    /// not stored on `Config` for the same reason [`Self::bearer_token`] is: the
    /// value must never be serialised, logged, or written beside the sessions.
    pub fn hub_credential(&self) -> Option<String> {
        match std::env::var(&self.hub.credential_env) {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        }
    }
}

/// Is this URL (or bare address) pointing at the loopback interface?
///
/// `localhost`, `127.0.0.0/8` and `[::1]` all mean "this process" — and the
/// process that dials the card is the LiteLLM container, where that is a
/// different machine's idea of here.
/// Is this a loopback *listen* address (`127.0.0.1:10001`, `[::1]:10001`)?
///
/// Unparseable is `false`: `validate` has already refused anything that is not a
/// socket address, and a warning is not the place to re-raise that.
/// Is this a WebSocket URL the tunnel can dial? `ws://` or `wss://` and nothing
/// else — `http://` would be a plain request, not a tunnel, and is the mistake a
/// reader of `publicUrl` makes.
pub fn is_websocket_url(url: &str) -> bool {
    url.starts_with("ws://") || url.starts_with("wss://")
}

pub fn bind_is_loopback(bind: &str) -> bool {
    match bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr.ip().is_loopback(),
        Err(_) => false,
    }
}

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
    fn a_loopback_bind_with_an_external_public_url_is_reported_and_not_refused() {
        // The measured outage shape (spikes/S15.md §6): the box registered a
        // tailnet `publicUrl` while binding loopback, so the LiteLLM container
        // dialled an address nothing was listening on.
        let mut config = valid();
        config.server.bind = "127.0.0.1:10001".to_string();
        config
            .validate()
            .expect("legitimate when something fronts the port");

        let warnings = config.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("127.0.0.1:10001"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("mac-studio.tail86fd19.ts.net:10001"),
            "{}",
            warnings[0]
        );

        // Binding what publicUrl names is the fix, and it is silent.
        config.server.bind = "100.77.144.14:10001".to_string();
        assert!(config.warnings().is_empty());

        // So is a fronted loopback pair: both sides loopback, nothing to say.
        config.server.bind = "127.0.0.1:10001".to_string();
        config.server.public_url = "http://127.0.0.1:10001".to_string();
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
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

    /// `goose.acp.serve: own` means this process starts goose, so the URL has to
    /// describe a server it can actually start: plain http, an IPv4 loopback,
    /// and exactly the endpoint goose serves.
    #[test]
    fn an_owned_goose_refuses_a_url_it_could_not_start_a_server_on() {
        /// Does this refusal name the thing the case is about?
        type Refusal = fn(&ConfigError) -> bool;

        let cases: [(&str, Refusal, &str); 8] = [
            (
                "https://127.0.0.1:3284/acp",
                |err| matches!(err, ConfigError::OwnedAcpNotHttp { .. }),
                "tls needs a certificate nobody has chosen",
            ),
            (
                "http://localhost:3284/acp",
                |err| matches!(err, ConfigError::OwnedAcpHostIsNotAnAddress { .. }),
                "goose --host rejects a name",
            ),
            (
                "http://::1:3284/acp",
                |err| matches!(err, ConfigError::OwnedAcpHostIsNotAnAddress { .. }),
                "goose --host rejects a bare IPv6 literal",
            ),
            (
                "http://192.168.1.5:3284/acp",
                |err| matches!(err, ConfigError::OwnedAcpNotLoopback { .. }),
                "an agent cannot start a goose on another host",
            ),
            (
                "http://127.0.0.1:3284/goose/acp",
                |err| matches!(err, ConfigError::OwnedAcpNotTheEndpoint { .. }),
                "a path prefix means something is in front of goose",
            ),
            (
                "http://127.0.0.1:3284/acp?x=1",
                |err| matches!(err, ConfigError::OwnedAcpNotTheEndpoint { .. }),
                "a query is not the endpoint goose serves",
            ),
            (
                "http://127.0.0.1/acp",
                |err| matches!(err, ConfigError::OwnedAcpAddress { .. }),
                "a URL with no port is not an address to bind",
            ),
            (
                "127.0.0.1:3284",
                |err| matches!(err, ConfigError::OwnedAcpAddress { .. }),
                "no scheme, nothing to read",
            ),
        ];

        for (url, expect, why) in cases {
            let mut config = valid();
            config.goose.acp.url = url.to_string();
            let err = config
                .validate()
                .expect_err(&format!("{url} should be refused: {why}"));
            assert!(
                expect(&err),
                "{url}: expected a refusal about the URL, got {err}"
            );
            assert!(
                err.to_string().contains(url),
                "the refusal must name the URL it is about: {err}"
            );
        }
    }

    #[test]
    fn every_spelling_of_one_owned_endpoint_is_accepted() {
        // The same four spellings the transport test dials: one server, so one
        // address, so no refusal.
        for url in [
            "http://127.0.0.1:3284",
            "http://127.0.0.1:3284/",
            "http://127.0.0.1:3284/acp",
            "http://127.0.0.1:3284/acp/",
        ] {
            let mut config = valid();
            config.goose.acp.url = url.to_string();
            config
                .validate()
                .unwrap_or_else(|err| panic!("{url} names this host's own goose: {err}"));
        }
    }

    #[test]
    fn an_external_goose_may_be_anywhere_under_any_scheme() {
        // `external` says "the host starts goose and this process only dials it",
        // so none of the owned-address rules apply - there is no address to bind.
        for url in [
            "https://goose.example/goose/acp",
            "http://192.168.1.5:3284",
            "http://[::1]:3284/acp",
        ] {
            let mut config = valid();
            config.goose.acp.serve = ServeMode::External;
            config.goose.acp.url = url.to_string();
            config
                .validate()
                .unwrap_or_else(|err| panic!("{url} is somebody else's server to start: {err}"));
        }
    }

    #[test]
    fn own_is_the_default_because_it_is_the_one_that_cannot_lie() {
        assert_eq!(Config::default().goose.acp.serve, ServeMode::Own);
        assert!(!Config::default().goose.acp.unauthenticated);
    }

    #[test]
    fn the_serve_mode_is_written_lowercase_in_config() {
        // The value an operator types, round-tripped through the parser that
        // reads it - so `serve: Own` is a parse error rather than a silent
        // mismatch between what was written and what runs.
        let dir = std::env::temp_dir().join(format!("a2a-goose-serve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.yaml");

        std::fs::write(
            &path,
            "server:\n  publicUrl: \"http://host:10001\"\n\
             goose:\n  acp:\n    serve: \"external\"\n  defaults:\n    allowedRoots: [\"/tmp\"]\n    cwd: \"/tmp\"\n",
        )
        .expect("write");
        let config = Config::load_from(&path, true).expect("external parses");
        assert_eq!(config.goose.acp.serve, ServeMode::External);
        assert_eq!(config.goose.acp.serve.as_str(), "external");

        std::fs::write(
            &path,
            "server:\n  publicUrl: \"http://host:10001\"\n\
             goose:\n  acp:\n    serve: \"Own\"\n  defaults:\n    allowedRoots: [\"/tmp\"]\n    cwd: \"/tmp\"\n",
        )
        .expect("write");
        let err = Config::load_from(&path, true).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_misspelled_unauthenticated_is_a_parse_error_not_a_silent_false() {
        // The field is the only way to own an unauthenticated goose, so a typo
        // in it must not read as "false, carry on".
        let dir = std::env::temp_dir().join(format!("a2a-goose-unauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "server:\n  publicUrl: \"http://host:10001\"\n\
             goose:\n  acp:\n    dangerouslyUnauthenticated: true\n  defaults:\n    allowedRoots: [\"/tmp\"]\n    cwd: \"/tmp\"\n",
        )
        .expect("write");
        let err = Config::load_from(&path, true).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let _ = std::fs::remove_dir_all(dir);
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

    #[test]
    fn the_hub_is_off_by_default_and_its_defaults_are_the_safe_ones() {
        let hub = Hub::default();
        assert!(!hub.enabled, "a host with no hub must not dial one");
        assert!(hub.url.is_empty());
        assert_eq!(hub.kind, "a2a-goose");
        assert_eq!(hub.credential_env, "A2A_GOOSE_HUB_TOKEN");
        assert_eq!(hub.connect_timeout_secs, 10);
        // Six roost status polls: a healthy tunnel that misses a poll or two is
        // not mistaken for a half-open one.
        assert_eq!(hub.idle_timeout_secs, 90);
        // And it is in the resolved config, not just the type.
        assert!(!Config::default().hub.enabled);
    }

    #[test]
    fn a_hub_enabled_with_a_non_websocket_url_is_warned_not_refused() {
        let mut config = valid();
        config.hub.enabled = true;
        config.hub.url = "http://roost:3000/agent/ws".to_string();
        // Still a config that serves: the hub is optional, so this is a warning.
        config
            .validate()
            .expect("a hub mistake must not stop the agent serving");
        let hub_warning = |config: &Config| {
            config
                .warnings()
                .into_iter()
                .find(|w| w.contains("hub.url"))
        };
        assert!(hub_warning(&config).is_some(), "{:?}", config.warnings());

        // The right scheme is silent, and so is no hub at all.
        config.hub.url = "ws://roost:3000/agent/ws".to_string();
        assert!(hub_warning(&config).is_none(), "{:?}", config.warnings());
        config.hub.enabled = false;
        config.hub.url = "http://roost:3000/agent/ws".to_string();
        assert!(hub_warning(&config).is_none(), "{:?}", config.warnings());
    }

    #[test]
    fn the_hub_credential_is_read_by_name_and_never_stored() {
        let mut config = Config::default();
        let name = format!("A2A_GOOSE_TEST_HUB_TOKEN_{}", std::process::id());
        config.hub.credential_env = name.clone();
        // SAFETY: a test-only variable, set and cleared within this test.
        unsafe {
            std::env::set_var(&name, "s3cret");
        }
        assert_eq!(config.hub_credential().as_deref(), Some("s3cret"));
        // The value is not a field, so it cannot be serialised with the config.
        let yaml = serde_yaml::to_string(&config).expect("serialises");
        assert!(!yaml.contains("s3cret"), "the credential leaked into YAML");
        unsafe {
            std::env::remove_var(&name);
        }
        assert!(config.hub_credential().is_none());
    }

    #[test]
    fn a_blank_hub_credential_counts_as_unset() {
        let mut config = Config::default();
        let name = format!("A2A_GOOSE_TEST_BLANK_{}", std::process::id());
        config.hub.credential_env = name.clone();
        // SAFETY: a test-only variable, set and cleared within this test.
        unsafe {
            std::env::set_var(&name, "   ");
        }
        assert!(config.hub_credential().is_none());
        unsafe {
            std::env::remove_var(&name);
        }
    }
}
