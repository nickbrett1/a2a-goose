//! Registration with LiteLLM: the agent's own entry in the agent directory.
//!
//! Three calls, all against the **API** and never `config.yaml` (constraint #2:
//! config agents are un-evictable, so a dynamic agent that lands there is
//! permanently un-sweepable):
//!
//! ```text
//! GET    /v1/agents                      is this host already listed?
//! POST   /v1/agents                      no  -> create
//! PUT    /v1/agents/{id}                 yes -> update in place
//! DELETE /v1/agents/{id}
//! ```
//!
//! **The listing is a view, and it can hide us.** `GET /v1/agents` is filtered
//! by the calling key's owner: measured 2026-09-18 against the live proxy, every
//! row carries `created_by`/`updated_by`, and the listing shows what that key
//! owns. (The same measurement rules out what first looked like the cause: a row
//! lists with `litellm_params: {}`, with `litellm_params` populated, and with
//! `is_public` absent, so `is_public` is not the filter — and `GET
//! /v1/agents/{id}`, which is *not* filtered, answers for a row the listing
//! omits.) After an upgrade, rows written by the older build carried no owner
//! stamp and disappeared from the listing while remaining present, callable and
//! addressable by id. Because the lookup below *is* that listing, a restarted
//! host read itself as absent, `POST`ed a name that was already taken, and had
//! no way back: its entry was there, working, and unreachable from here.
//!
//! So the id is remembered ([`crate::config::Registry::agent_id_path`], beside
//! `sessions.db`) and the lookup degrades: the listing first, then `GET
//! /v1/agents/{id}` with the id this host registered under. A proxy that changes
//! what the listing shows can no longer wedge a host that has registered once,
//! and the recovery is announced rather than silent — this is exactly the class
//! of failure that a green "registered" line in a log cannot detect.
//!
//! **Why a lookup first, and a PUT rather than a second POST.** `POST
//! /v1/agents` with a name that is already taken is a hard `400 {"detail":
//! "Agent with name ... already exists"}` (measured, [S13](../../spikes/S13.md)),
//! so a host that crashes without deregistering — OOM, a host sleep, a wedged
//! process; exactly the cases `deregister` cannot cover — would fail to
//! re-register and only *look* registered while serving a stale card. The
//! lookup also makes the collision mean the right thing: the name is this host's
//! identity, so a colliding entry is a previous instance of *us*, and adopting
//! its id is how the stale entry gets reclaimed. `PUT /v1/agents/{id}` updates
//! the stored card in place, which is strictly better than the `DELETE` then
//! `POST` this was originally specified as: no window with no entry, and no id
//! churn.
//!
//! **What is deliberately not sent.** LiteLLM 1.103.0 accepts and then silently
//! discards `max_iterations`, `max_budget_per_session` and
//! `require_trace_id_on_calls_by_agent`: the stored agent object simply does not
//! have them (measured, `spikes/S2.md`). Sending them would look like cost
//! control that is not there, which is worse than sending nothing — so the loop
//! bounds live in this process (`config.registry.limits`) and are enforced where
//! they can actually be observed.
//!
//! **What *is* sent, and why.** LiteLLM does not fetch the card: `POST
//! /v1/agents` merges the `agent_card_params` **given to it** and stamps in its
//! own default `[chat]` skill when none were sent ([S9](../../spikes/S9.md)). So
//! a registration that carries only `{name, url, protocolVersion}` is a
//! registration whose skills no caller ever sees, however reachable the card
//! URL is. We therefore register the card [`crate::card::assemble`] built from
//! the host's own recipes — `id`, `name`, `description`, `tags` per skill, and
//! nothing that carries dispatch detail (constraint #12) — so the directory
//! reflects the host rather than the proxy's default. `url` and
//! `protocolVersion` are lifted to the top level because LiteLLM reads them
//! there (they are LiteLLM's stored-card shape, not A2A v1.0 card fields); the
//! fields LiteLLM reserves for itself (`supportedInterfaces`, `securitySchemes`,
//! `security`, `provider`) it overwrites during its merge, so they are left to
//! be overwritten rather than sent.
//!
//! **Registration failing is not fatal.** A node agent whose proxy is down is
//! still a useful node agent; it serves locally and says `unregistered` on
//! `/status` (§9). What would be fatal is a boot that depends on another
//! service being up.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::AgentCard;
use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

/// How many times boot registration is attempted before giving up and reporting
/// `failed`. M1 picked 4 to get the agent listed and make a failure visible;
/// M4 raised it, because the thing being waited for is usually a *proxy that is
/// not up yet* — a LiteLLM restart outlasts four seconds of trying, and the
/// cost of the extra attempts is invisible next to the cost of a host that is
/// serving but unreachable.
const REGISTER_ATTEMPTS: u32 = 8;

/// First backoff step; doubles each attempt. Long enough to ride out a proxy
/// restart, short enough that a boot is not held up for minutes.
const REGISTER_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling on that doubling. Uncapped, attempt 8 would wait over two minutes
/// for its last try, which buys nothing a 30s wait has not already.
const REGISTER_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// What `/status` reports about the registry. Modelled as a state rather than a
/// bool so a reader can tell "not configured here" from "configured and failing"
/// — the two look identical otherwise, and they need different fixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "state"
)]
pub enum RegistryState {
    /// No master key in the environment: registration was never attempted.
    Unconfigured,
    /// Configured, about to try.
    Unregistered,
    /// In flight.
    Registering,
    Registered {
        agent_id: String,
        /// The skills LiteLLM's response carried back. S5: a registration whose
        /// upstream fetch has not happened yet answers with LiteLLM's own
        /// synthesised card, so this being `["chat"]` means "not read ours yet",
        /// not "registered and matching".
        skills: Vec<String>,
    },
    Failed {
        error: String,
    },
}

impl RegistryState {
    pub fn is_registered(&self) -> bool {
        matches!(self, RegistryState::Registered { .. })
    }
}

#[derive(Debug)]
pub enum RegistryError {
    NotConfigured,
    Request(reqwest::Error),
    Status {
        status: u16,
        body: String,
    },
    NoAgentId {
        body: String,
    },
    /// The name is taken, but this host could not learn the id of the entry
    /// holding it — so the entry cannot be rewritten or adopted, and retrying
    /// only repeats the refusal.
    ///
    /// Distinct from a plain [`RegistryError::Status`] on purpose: the proxy
    /// answering "duplicate name" is *proof the entry exists*, and reporting
    /// that as a generic 500 makes a recoverable identity problem read like a
    /// proxy fault. The honest report is "you are registered and this host
    /// cannot see or manage its own entry", which is a listing problem.
    NameTakenUnresolvable {
        name: String,
        body: String,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "no LiteLLM master key in the environment; registration is off"
            ),
            Self::Request(err) => write!(f, "could not reach LiteLLM: {err}"),
            Self::Status { status, body } => {
                write!(f, "LiteLLM answered {status}: {body}")
            }
            Self::NoAgentId { body } => write!(
                f,
                "LiteLLM accepted the registration but returned no agent_id, so it cannot be \
                 deleted later: {body}"
            ),
            Self::NameTakenUnresolvable { name, body } => write!(
                f,
                "LiteLLM refused to create `{name}` because the name is already taken, and its \
                 listing does not show the entry that holds it, so the id cannot be read: \
                 {body}. The entry exists and may be serving; this host simply cannot see it. \
                 That is a listing-filter problem, not a proxy fault — see RUNBOOK.md, \
                 \"the roster is filtered by owner\". Restoring visibility of the row (an admin \
                 PUT, from an admin key) is what clears it."
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

impl RegistryError {
    /// Does this failure mean "that name is already taken"?
    ///
    /// Keyed on the **body**, not the status, because the two disagree here:
    /// S13 recorded a `400` for a duplicate `agent_name`, and the live proxy
    /// answers `500` with Prisma's
    /// `Unique constraint failed on the fields: (agent_name)`. A status test
    /// would miss the case that actually happens, and a status of `500` is not
    /// otherwise distinguishable from a real server fault — so the string is
    /// the honest key, contained in one place.
    fn is_duplicate_name(&self) -> bool {
        match self {
            Self::Status { body, .. } => {
                body.contains("Unique constraint failed") || body.contains("already exists")
            }
            _ => false,
        }
    }
}

impl From<reqwest::Error> for RegistryError {
    fn from(err: reqwest::Error) -> Self {
        RegistryError::Request(err)
    }
}

/// The agent id LiteLLM assigned, plus what it echoed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub agent_id: String,
    pub skills: Vec<String>,
}

struct Client {
    base_url: String,
    master_key: String,
    agent_name: String,
    /// The token **this agent serves with**, to be handed to LiteLLM as a
    /// `static_headers` entry so its route can authenticate to us. `None` only
    /// when the environment has no bearer token, which the server refuses to
    /// start without — so in practice this is always `Some`.
    bearer_token: Option<String>,
    /// Whether an entry that already exists may be **rewritten**. `false` adopts
    /// the existing entry and leaves its card alone, for a host whose registry
    /// entry is managed from elsewhere.
    re_register_on_card_change: bool,
    /// Where the id LiteLLM assigned is remembered between runs. The one handle
    /// a filtered listing cannot take away; see the module comment. `None` only
    /// in tests that do not exercise the persistence.
    agent_id_path: Option<std::path::PathBuf>,
    http: reqwest::Client,
}

/// The registration handle: a client (when configured) plus the shared state the
/// control surface reads.
#[derive(Clone)]
pub struct Registry {
    client: Option<Arc<Client>>,
    state: Arc<Mutex<RegistryState>>,
}

impl Registry {
    /// Builds the handle. **Never fails**: a missing master key is a state, not
    /// an error, because it must not stop the agent serving (see the module
    /// comment).
    pub fn new(config: &Config) -> Self {
        let client = config.litellm_master_key().map(|master_key| {
            Arc::new(Client {
                base_url: config
                    .registry
                    .litellm_base_url
                    .trim_end_matches('/')
                    .to_string(),
                master_key,
                agent_name: config.registry.agent_name.clone(),
                bearer_token: config.bearer_token().ok(),
                re_register_on_card_change: config.registry.re_register_on_card_change,
                agent_id_path: Some(config.registry.agent_id_path.clone()),
                http: reqwest::Client::new(),
            })
        });

        let state = if client.is_some() {
            RegistryState::Unregistered
        } else {
            RegistryState::Unconfigured
        };

        Self {
            client,
            state: Arc::new(Mutex::new(state)),
        }
    }

    pub fn state(&self) -> RegistryState {
        self.state
            .lock()
            .expect("registry state is never held across an await")
            .clone()
    }

    fn set_state(&self, next: RegistryState) {
        *self
            .state
            .lock()
            .expect("registry state is never held across an await") = next;
    }

    /// Registers in the background, with backoff.
    ///
    /// Background rather than inline because boot must not be gated on the
    /// proxy: the launcher execs this process, and a slow or absent LiteLLM is
    /// not a reason for the supervisor to see a failure.
    pub fn spawn_registration(&self, card: &AgentCard) {
        let Some(client) = self.client.clone() else {
            tracing::info!(
                key = %"LITELLM_MASTER_KEY",
                "no LiteLLM master key in the environment; serving without registering"
            );
            return;
        };

        // The card is what we register, not a three-field stub (S9): LiteLLM
        // never fetches it, so the skills have to travel in the POST/PUT body.
        let card = card.clone();
        let registry = self.clone();
        tokio::spawn(async move {
            registry.set_state(RegistryState::Registering);

            let mut backoff = REGISTER_BACKOFF;
            for attempt in 1..=REGISTER_ATTEMPTS {
                match client.register(&card).await {
                    Ok(registration) => {
                        tracing::info!(
                            agent_id = %registration.agent_id,
                            skills = ?registration.skills,
                            "registered with LiteLLM"
                        );
                        // S5/S9: the response card is LiteLLM's merge of what
                        // we sent plus its own overlays. Since we now send our
                        // skills, `["chat"]` coming back means they did not
                        // travel — the silent-default case S9 exists to prevent
                        // — so say so rather than let `/status` look fine.
                        if registration.skills == vec!["chat".to_string()] {
                            tracing::warn!(
                                "LiteLLM answered with its default `chat` skill; our skills did \
                                 not reach the directory"
                            );
                        }
                        registry.set_state(RegistryState::Registered {
                            agent_id: registration.agent_id,
                            skills: registration.skills,
                        });
                        return;
                    }
                    Err(err) => {
                        if attempt == REGISTER_ATTEMPTS {
                            tracing::warn!(
                                attempts = attempt,
                                error = %err,
                                "registration failed; serving locally without a registry entry"
                            );
                            registry.set_state(RegistryState::Failed {
                                error: err.to_string(),
                            });
                            return;
                        }
                        tracing::debug!(attempt, error = %err, "registration attempt failed");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(REGISTER_BACKOFF_CAP);
                    }
                }
            }
        });
    }

    /// Best-effort deregistration on the way out.
    ///
    /// Clean shutdown only, and never a liveness mechanism: OOM, a host sleep or
    /// a wedged process all skip this, which is exactly why the sweeper exists
    /// (Part 2). A 404 is success — we may be racing a sweeper that already
    /// removed us (S5, guard #3).
    pub async fn deregister(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let RegistryState::Registered { agent_id, .. } = self.state() else {
            return;
        };

        match client.deregister(&agent_id).await {
            Ok(()) => tracing::info!(%agent_id, "deregistered from LiteLLM"),
            Err(err) => tracing::warn!(%agent_id, error = %err, "could not deregister"),
        }
    }
}

/// Finds this host's entry in a `GET /v1/agents` listing.
///
/// Returns `None` for "not listed" — including for a listing that is not an
/// array at all, because the caller's next move (create) is the same either way
/// and a shape disagreement here must not be able to stop a host registering.
fn find_by_name(text: &str, name: &str) -> Result<Option<String>, RegistryError> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    let Some(agents) = value.as_array() else {
        return Ok(None);
    };

    let mut found = agents.iter().filter_map(|agent| {
        let listed = agent.get("agent_name").and_then(Value::as_str)?;
        let id = agent.get("agent_id").and_then(Value::as_str)?;
        (listed == name).then(|| id.to_string())
    });

    let first = found.next();
    if let (Some(id), Some(duplicate)) = (&first, found.next()) {
        // Should not happen: one host, one identity. If it does, the entry we
        // rewrite must be predictable, so say which one was chosen.
        tracing::warn!(
            name,
            chosen = %id,
            ignored = %duplicate,
            "more than one registry entry carries this host's name"
        );
    }
    Ok(first)
}

/// Reads a `POST /v1/agents` response.
///
/// Split out from the request so the shape is testable against the *real*
/// response, which is committed as `tests/fixtures/litellm-agent-registration.json`
/// — LiteLLM's answer is a contract we depend on and cannot type, so it is
/// pinned rather than assumed.
fn parse_registration(text: &str) -> Result<Registration, RegistryError> {
    parse_registration_with(text, None)
}

/// As [`parse_registration`], but able to supply the id when the body does not.
///
/// Belt and braces: LiteLLM's `GET /v1/agents/{id}` *does* echo `agent_id`
/// (measured), but the call is addressed by id already, so a body without one is
/// still an answer — and failing there would turn a successful adoption into a
/// boot failure over a field we already know.
fn parse_registration_with(
    text: &str,
    known_id: Option<&str>,
) -> Result<Registration, RegistryError> {
    let value: Value = serde_json::from_str(text).map_err(|_| RegistryError::NoAgentId {
        body: text.to_string(),
    })?;
    let agent_id = value
        .get("agent_id")
        .and_then(Value::as_str)
        .or(known_id)
        .map(str::to_string)
        .ok_or_else(|| RegistryError::NoAgentId {
            body: text.to_string(),
        })?;

    // The skills live *inside* `agent_card_params`, not at the top level, and
    // S5's finding is why reading them matters at all: a registration whose
    // upstream card has not been fetched answers with LiteLLM's own synthesised
    // card, so `["chat"]` means "not read ours yet" rather than "registered and
    // matching". Reported rather than acted on, because the remedy is a
    // re-registration, not a retry.
    let card = value.get("agent_card_params").unwrap_or(&value);

    Ok(Registration {
        agent_id,
        skills: crate::card::skill_ids(card),
    })
}

/// What to do about the registry entry, given what is already there.
///
/// A pure decision, split out so the two branches are tested without a proxy —
/// the HTTP around it is three one-line calls to LiteLLM, and the part worth
/// getting right is *which* one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// Nothing is listed under this host's name: create it.
    Create,
    /// Our own stale entry: take its id and rewrite the card.
    Update(String),
    /// Our own stale entry, but this host may not rewrite cards: take the id and
    /// leave the stored card as it is.
    Adopt(String),
}

fn plan(existing: Option<String>, may_rewrite: bool) -> Plan {
    match existing {
        None => Plan::Create,
        Some(id) if may_rewrite => Plan::Update(id),
        Some(id) => Plan::Adopt(id),
    }
}

/// The `agent_card_params` we register: our assembled card, with the two
/// non-standard top-level fields LiteLLM reads lifted out of `supportedInterfaces`.
///
/// S9: LiteLLM **never fetches** `card.url` — `POST /v1/agents` merges the card
/// it is handed and substitutes its own `[chat]` default when `skills` is
/// missing — so the skills must be in the body. `url`/`protocolVersion` are
/// hoisted because LiteLLM stores and reads them at the top level (they are not
/// A2A v1.0 card fields). Everything LiteLLM reserves (`supportedInterfaces`,
/// `securitySchemes`, `security`, `provider`) it overwrites during its merge, so
/// those are left to be overwritten rather than carefully constructed here.
///
/// Serialising our own [`AgentCard`] rather than hand-rolling the JSON is the
/// point (constraint #17): the wire shape is the SDK's, and a version bump that
/// changes it is a `Cargo.lock` change we review, not a silent drift. The card's
/// skills carry no dispatch detail — `card.rs` projects only
/// `id`/`name`/`description`/`tags` (constraint #12).
fn card_params(card: &AgentCard) -> Value {
    let mut params = serde_json::to_value(card).expect("an AgentCard always serialises");
    if let Some(object) = params.as_object_mut() {
        if let Some(interface) = card.supported_interfaces.first() {
            object.insert("url".to_string(), Value::String(interface.url.clone()));
            object.insert(
                "protocolVersion".to_string(),
                Value::String(interface.protocol_version.clone()),
            );
        }
    }
    params
}

/// The `POST`/`PUT` body: our name, our card, and — when this host has one —
/// the bearer LiteLLM's own route must present to us.
///
/// M4, and measured before it was written: `static_headers` goes at the **top
/// level** of the request. A copy nested under `litellm_params` is accepted and
/// stored there, but the row the `/a2a/{agent_id}` route reads is the top-level
/// one — a probe registered both ways and read them back, and only the
/// top-level entry came back where the working entry on the NAS has it. Before
/// this, the token had to be installed by hand with an admin `PUT`, which
/// nothing in this repository could restore, and three surfaces depended on it.
///
/// This is the agent's *own* serving token, not a key of ours: it travels from
/// one place we already hold it to the one place that needs it, and it is not
/// logged (the `Authorization` value is never printed at any level).
fn registration_body(agent_name: &str, card: &AgentCard, bearer_token: Option<&str>) -> Value {
    let mut body = serde_json::json!({
        "agent_name": agent_name,
        "agent_card_params": card_params(card),
    });
    if let Some(token) = bearer_token {
        body["static_headers"] = serde_json::json!({
            "Authorization": format!("Bearer {token}"),
        });
    }
    body
}

impl Client {
    /// The agent id already listed under this host's name, if any.
    ///
    /// A list scan rather than a query, because LiteLLM's `GET /v1/agents` takes
    /// no usable name filter (`?query=` wants an embedding model configured;
    /// `?agent_name=` is ignored). One host has one entry, so the scan is over a
    /// handful.
    ///
    /// **The scan is not authoritative.** The listing is filtered by owner, so a
    /// row this host owns but the calling key cannot see reads as absent — see
    /// the module comment. When the scan comes up empty, the id this host
    /// remembered from its last registration is checked directly with `GET
    /// /v1/agents/{id}`, which is not filtered. That is the whole point: "not
    /// listed" and "not registered" are different, and only one of them is a
    /// reason to create.
    async fn find_agent_id(&self) -> Result<Option<String>, RegistryError> {
        let response = self
            .http
            .get(format!("{}/v1/agents", self.base_url))
            .bearer_auth(&self.master_key)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(RegistryError::Status {
                status: status.as_u16(),
                body: text,
            });
        }

        if let Some(id) = find_by_name(&text, &self.agent_name)? {
            return Ok(Some(id));
        }

        self.agent_id_by_remembered_id().await
    }

    /// The id remembered from a previous registration, if the proxy still holds
    /// a row under it and that row is still ours.
    ///
    /// Three answers, and each has a different consequence:
    /// a row that is ours — taken, so the caller updates it in place;
    /// a row that is gone (404) — `None`, so the caller creates;
    /// a row that is now *someone else's name* — `None`, so the caller creates
    /// and the proxy refuses the name, which is reported honestly rather than
    /// silently adopting a row that is not ours.
    async fn agent_id_by_remembered_id(&self) -> Result<Option<String>, RegistryError> {
        let Some(remembered) = self.remembered_agent_id() else {
            return Ok(None);
        };

        let response = self
            .http
            .get(format!("{}/v1/agents/{remembered}", self.base_url))
            .bearer_auth(&self.master_key)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if status.as_u16() == 404 {
            tracing::debug!(agent_id = %remembered, "the remembered agent id is gone; creating");
            return Ok(None);
        }
        if !status.is_success() {
            return Err(RegistryError::Status {
                status: status.as_u16(),
                body: text,
            });
        }

        let listed = serde_json::from_str::<Value>(&text).ok().and_then(|value| {
            value
                .get("agent_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if listed.as_deref() != Some(self.agent_name.as_str()) {
            tracing::warn!(
                agent_id = %remembered,
                listed = ?listed,
                ours = %self.agent_name,
                "the remembered id now names a different agent; not adopting it"
            );
            return Ok(None);
        }

        // Loud on purpose. This is the path that a proxy upgrade silently
        // creates: the host IS registered, the listing just does not say so,
        // and every symptom from here on (a duplicate-name POST, an entry that
        // cannot be rewritten) is a consequence. Called out so the cause is not
        // mistaken for it.
        tracing::warn!(
            agent_id = %remembered,
            "the LiteLLM agent listing does not show this host, but the by-id read does; \
             resolving by remembered id. The listing is filtered — see RUNBOOK.md"
        );
        Ok(Some(remembered))
    }

    /// Persists the id, so a filtered listing cannot hide this host next boot.
    ///
    /// Failure is logged and swallowed: un-writable state costs a stale-looking
    /// `/status` after a restart, which is not a reason to fail a registration
    /// that otherwise succeeded.
    fn remember_agent_id(&self, agent_id: &str) {
        let Some(path) = &self.agent_id_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), error = %err, "could not create the state directory for the remembered agent id");
                return;
            }
        }
        // Written to a sibling and renamed, so a reader never sees a half-written
        // id (the file is read at boot, while another process may be writing it).
        let temporary = path.with_extension("writing");
        if let Err(err) = std::fs::write(&temporary, format!("{agent_id}\n")) {
            tracing::warn!(path = %temporary.display(), error = %err, "could not remember the agent id");
            return;
        }
        if let Err(err) = std::fs::rename(&temporary, path) {
            tracing::warn!(path = %path.display(), error = %err, "could not remember the agent id");
        }
    }

    /// The id remembered from a previous registration, if any.
    fn remembered_agent_id(&self) -> Option<String> {
        let path = self.agent_id_path.as_ref()?;
        let id = std::fs::read_to_string(path).ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    }

    async fn register(&self, card: &AgentCard) -> Result<Registration, RegistryError> {
        // The card, not a stub: LiteLLM merges exactly what it is given and
        // never fetches `card.url` (S9), so skills omitted here are skills no
        // caller can discover. See the module comment for what is deliberately
        // absent, and `spikes/S2.md` for the cost-parameter measurement.
        let body = registration_body(&self.agent_name, card, self.bearer_token.as_deref());

        let existing = self.find_agent_id().await?;
        let registration = match plan(existing, self.re_register_on_card_change) {
            Plan::Create => match self.send(reqwest::Method::POST, "/v1/agents", &body).await {
                Ok(text) => parse_registration(&text),
                // The name is taken although the listing did not show it: the
                // entry appeared between the scan and the POST, or the listing
                // is filtered by the key we registered with. Either way we now
                // know an id exists for our name, so the reclaim is another
                // lookup and an in-place rewrite — the same `Plan::Update` the
                // happy path would have taken, reached one request later.
                Err(err) if err.is_duplicate_name() => {
                    let agent_id = self.find_agent_id().await?.ok_or(
                        RegistryError::NameTakenUnresolvable {
                            name: self.agent_name.clone(),
                            body: err.to_string(),
                        },
                    )?;
                    tracing::warn!(
                        %agent_id,
                        "the name was already taken on POST; reclaiming the existing entry"
                    );
                    let text = self
                        .send(
                            reqwest::Method::PUT,
                            &format!("/v1/agents/{agent_id}"),
                            &body,
                        )
                        .await?;
                    parse_registration(&text)
                }
                Err(err) => Err(err),
            },
            Plan::Update(agent_id) => {
                tracing::info!(
                    %agent_id,
                    "already listed under this name; updating the entry in place"
                );
                let text = self
                    .send(
                        reqwest::Method::PUT,
                        &format!("/v1/agents/{agent_id}"),
                        &body,
                    )
                    .await?;
                parse_registration(&text)
            }
            Plan::Adopt(agent_id) => {
                tracing::info!(
                    %agent_id,
                    "already listed under this name; adopting it without rewriting the card \
                     (reRegisterOnCardChange is off)"
                );
                // Nothing came back to parse, so read the card we left alone:
                // `/status` should report what the directory actually holds.
                let text = self.get_agent(&agent_id).await?;
                parse_registration_with(&text, Some(&agent_id))
            }
        }?;

        // Kept so that a listing which later stops showing this host cannot make
        // it look unregistered — the failure this file was taught about the hard
        // way. Cheap, and the only thing here that survives the process.
        self.remember_agent_id(&registration.agent_id);
        Ok(registration)
    }

    /// One authenticated request, returning the body or a status error.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &Value,
    ) -> Result<String, RegistryError> {
        let response = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.master_key)
            .json(body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(RegistryError::Status {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(text)
    }

    async fn get_agent(&self, agent_id: &str) -> Result<String, RegistryError> {
        let response = self
            .http
            .get(format!("{}/v1/agents/{agent_id}", self.base_url))
            .bearer_auth(&self.master_key)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(RegistryError::Status {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(text)
    }

    async fn deregister(&self, agent_id: &str) -> Result<(), RegistryError> {
        let response = self
            .http
            .delete(format!("{}/v1/agents/{agent_id}", self.base_url))
            .bearer_auth(&self.master_key)
            .send()
            .await?;

        let status = response.status();
        if status.is_success() || status.as_u16() == 404 {
            // 404 is success: already gone, possibly because a sweeper got there
            // first. Treating it as an error would make a converging system look
            // broken (S5).
            return Ok(());
        }

        let body = response.text().await.unwrap_or_default();
        Err(RegistryError::Status {
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config() -> Config {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
        config
    }

    /// A card shaped like the one `card::assemble` builds: one `ask` skill on an
    /// interface URL. The registration body is derived from this, so the tests
    /// exercise the real shape rather than a hand-made stub.
    fn test_card(url: &str) -> AgentCard {
        let mut interface = a2a::AgentInterface::new(url, a2a::TRANSPORT_PROTOCOL_JSONRPC);
        // `AgentInterface::new` pins the SDK's default version; pin ours.
        interface.protocol_version = "1.0".to_string();
        AgentCard {
            name: "mac-studio-goose".to_string(),
            description: "goose on the Mac Studio, via A2A".to_string(),
            version: "1.2.3".to_string(),
            supported_interfaces: vec![interface],
            capabilities: a2a::AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: None,
                extended_agent_card: None,
            },
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![a2a::AgentSkill {
                id: "ask".to_string(),
                name: "Ask".to_string(),
                description: "Ask goose directly".to_string(),
                tags: vec!["chat".to_string()],
                examples: None,
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            }],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }

    #[test]
    fn the_registration_body_hands_litellm_the_bearer_its_route_must_present() {
        // M4. Our own `/a2a` refuses to serve without a bearer, so LiteLLM's
        // route has to send one — and the only place it can read it from is the
        // agent row. Top-level, because that is the entry the route reads
        // (measured against the live proxy: the same value nested under
        // `litellm_params` is stored and read back from there instead).
        let body = registration_body(
            "mac-studio-goose",
            &test_card("http://host:10001"),
            Some("secret-token"),
        );
        assert_eq!(
            body["static_headers"]["Authorization"],
            "Bearer secret-token"
        );
        // And the shape the rest of the body already had is untouched.
        assert_eq!(body["agent_name"], "mac-studio-goose");
        assert_eq!(
            crate::card::skill_ids(&body["agent_card_params"]),
            vec!["ask".to_string()]
        );

        // No token in the environment: no header, rather than an empty one that
        // would make the route fail with something less obvious than "absent".
        let body = registration_body("mac-studio-goose", &test_card("http://host:10001"), None);
        assert!(body.get("static_headers").is_none());
    }

    #[test]
    fn a_taken_name_is_recognised_from_the_body_not_the_status() {
        // The live proxy answers 500 with Prisma's constraint text where S13
        // recorded a 400, and a bare 500 is not otherwise distinguishable from a
        // real fault — so the string is what is tested.
        let live = RegistryError::Status {
            status: 500,
            body: r#"{"detail":"Unique constraint failed on the fields: (agent_name)"}"#
                .to_string(),
        };
        assert!(live.is_duplicate_name());

        let documented = RegistryError::Status {
            status: 400,
            body: r#"{"detail":"Agent with name mac-studio-goose already exists"}"#.to_string(),
        };
        assert!(documented.is_duplicate_name(), "S13's documented shape too");

        // A 500 that is not about the name must not be mistaken for a reclaim.
        let real_fault = RegistryError::Status {
            status: 500,
            body: r#"{"detail":"connection to database timed out"}"#.to_string(),
        };
        assert!(!real_fault.is_duplicate_name());
    }

    /// A stub whose listing **hides** our entry while its POST refuses the
    /// name: the race M4's reclaim exists for — an entry that was created by a
    /// previous process, or by a key whose listing is filtered, so the lookup
    /// cannot see what the constraint already knows about.
    async fn stub_litellm_with_a_hidden_entry() -> (String, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let revealed = Arc::new(Mutex::new(false));
        let app = axum::Router::new()
            .fallback(hidden_stub)
            .with_state(Hidden {
                calls: calls.clone(),
                revealed: revealed.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (format!("http://{addr}"), calls)
    }

    #[derive(Clone)]
    struct Hidden {
        calls: Arc<Mutex<Vec<String>>>,
        revealed: Arc<Mutex<bool>>,
    }

    async fn hidden_stub(
        method: axum::http::Method,
        uri: axum::http::Uri,
        axum::extract::State(stub): axum::extract::State<Hidden>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        use axum::http::StatusCode;

        let path = uri.path().to_string();
        stub.calls
            .lock()
            .expect("call log")
            .push(format!("{method} {path}"));

        let registration = serde_json::json!({
            "agent_id": "id-1",
            "agent_name": "mac-studio-goose",
            "agent_card_params": {"name": "mac-studio-goose", "skills": []},
        });

        match (method.as_str(), path.as_str()) {
            ("GET", "/v1/agents") => {
                let revealed = *stub.revealed.lock().expect("revealed");
                let listing = if revealed {
                    serde_json::json!([{"agent_id": "id-1",
                                        "agent_name": "mac-studio-goose"}])
                } else {
                    serde_json::json!([])
                };
                (StatusCode::OK, axum::Json(listing))
            }
            ("POST", "/v1/agents") => {
                // The entry becomes visible as a side effect, exactly as it
                // would if another process had just created it.
                *stub.revealed.lock().expect("revealed") = true;
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({
                        "detail": "Unique constraint failed on the fields: (agent_name)"
                    })),
                )
            }
            ("PUT", "/v1/agents/id-1") => (StatusCode::OK, axum::Json(registration)),
            _ => (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))),
        }
    }

    #[tokio::test]
    async fn a_name_taken_although_it_was_not_listed_is_reclaimed_rather_than_failed() {
        let (base, calls) = stub_litellm_with_a_hidden_entry().await;
        let registration = client_against(base, true)
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("the second lookup finds the id the POST refused to give");

        assert_eq!(registration.agent_id, "id-1");
        assert_eq!(
            *calls.lock().expect("call log"),
            vec![
                "GET /v1/agents",
                "POST /v1/agents",
                "GET /v1/agents",
                "PUT /v1/agents/id-1"
            ],
            "a scan, a refused create, then the same scan and rewrite the happy path would have done"
        );
    }

    /// A stub whose listing **never** shows our entry, whatever happens — the
    /// live failure: `GET /v1/agents` filters by owner, so a row the proxy holds
    /// and serves is absent from the listing for good, not just for a moment.
    /// `by_id` is the row behind the remembered id, or `None` for the 404 that
    /// means "genuinely gone". `name_taken` is whether a create is refused.
    #[derive(Clone)]
    struct Filtered {
        calls: Arc<Mutex<Vec<String>>>,
        by_id: Option<&'static str>,
        name_taken: bool,
    }

    async fn filtered_stub(
        method: axum::http::Method,
        uri: axum::http::Uri,
        axum::extract::State(stub): axum::extract::State<Filtered>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        use axum::http::StatusCode;

        let path = uri.path().to_string();
        stub.calls
            .lock()
            .expect("call log")
            .push(format!("{method} {path}"));

        let row = |name: &str| {
            serde_json::json!({
                "agent_id": "id-1",
                "agent_name": name,
                "agent_card_params": {"name": name, "skills": []},
            })
        };

        match (method.as_str(), path.as_str()) {
            ("GET", "/v1/agents") => (StatusCode::OK, axum::Json(serde_json::json!([]))),
            ("GET", "/v1/agents/id-1") => match stub.by_id {
                Some(name) => (StatusCode::OK, axum::Json(row(name))),
                None => (
                    StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({"detail": "Agent with ID id-1 not found"})),
                ),
            },
            ("PUT", "/v1/agents/id-1") => (StatusCode::OK, axum::Json(row("mac-studio-goose"))),
            ("POST", "/v1/agents") if stub.name_taken => (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "detail": "Agent with name mac-studio-goose already exists"
                })),
            ),
            ("POST", "/v1/agents") => (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "agent_id": "id-2",
                    "agent_name": "mac-studio-goose",
                    "agent_card_params": {"name": "mac-studio-goose", "skills": []},
                })),
            ),
            _ => (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))),
        }
    }

    async fn stub_filtered(
        by_id: Option<&'static str>,
        name_taken: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .fallback(filtered_stub)
            .with_state(Filtered {
                calls: calls.clone(),
                by_id,
                name_taken,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (format!("http://{addr}"), calls)
    }

    /// A path in a per-test temporary directory, with the file already holding
    /// `contents` (the id a previous run remembered).
    fn remembered_id_path(test: &str, contents: Option<&str>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("a2a-goose-registry-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("registry-agent-id");
        match contents {
            Some(text) => std::fs::write(&path, text).expect("seed the remembered id"),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        path
    }

    #[tokio::test]
    async fn a_listing_that_hides_us_is_resolved_by_the_id_we_remembered() {
        // The live failure, 2026-09-18: the proxy holds the row, the listing
        // does not show it, and the by-id call does. Without this fallback the
        // host reads itself as absent, POSTs a duplicate name, and reports
        // itself unregistered while callers reach it perfectly well.
        let (base, calls) = stub_filtered(Some("mac-studio-goose"), true).await;
        let path = remembered_id_path("hides-us", Some("id-1\n"));

        let registration = client_with_state(base, true, Some(path))
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("the remembered id resolves the entry the listing hides");

        assert_eq!(registration.agent_id, "id-1");
        assert_eq!(
            *calls.lock().expect("call log"),
            vec![
                "GET /v1/agents",
                "GET /v1/agents/id-1",
                "PUT /v1/agents/id-1"
            ],
            "a filtered listing, the by-id lookup that replaces it, then the rewrite"
        );
    }

    #[tokio::test]
    async fn a_remembered_id_the_proxy_no_longer_holds_does_not_stop_a_create() {
        // A 404 is the honest "gone": the row was swept, or the proxy was
        // rebuilt. Falling back to create — rather than treating a stale local
        // file as truth — is what keeps this from becoming its own wedge.
        let (base, calls) = stub_filtered(None, false).await;
        let path = remembered_id_path("id-gone", Some("id-1\n"));

        let registration = client_with_state(base, true, Some(path.clone()))
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("a gone id means create");

        assert_eq!(registration.agent_id, "id-2");
        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["GET /v1/agents", "GET /v1/agents/id-1", "POST /v1/agents"]
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("remembered").trim(),
            "id-2",
            "the new id replaces the stale one, so the next boot resolves"
        );
    }

    #[tokio::test]
    async fn a_remembered_id_that_now_names_someone_else_is_never_adopted() {
        // Ids are the proxy's to reassign; a remembered one is a hint, not an
        // identity. Adopting a row under another agent's name would rewrite
        // *their* card with ours — worse than failing.
        let (base, calls) = stub_filtered(Some("someone-else"), true).await;
        let path = remembered_id_path("someone-else", Some("id-1\n"));

        let err = client_with_state(base, true, Some(path))
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect_err("a row that is not ours cannot be adopted");

        assert!(
            matches!(err, RegistryError::NameTakenUnresolvable { .. }),
            "the honest report is `name taken and unresolvable`, not a proxy fault: {err:?}"
        );
        assert!(
            err.to_string().contains("listing does not show"),
            "the message names the cause, so the fix is not looked for in the proxy: {err}"
        );
        assert!(
            !calls
                .lock()
                .expect("call log")
                .iter()
                .any(|call| call.starts_with("PUT")),
            "and nothing of ours was written over it"
        );
    }

    #[tokio::test]
    async fn a_registration_remembers_the_id_it_was_given() {
        // The other half of the fix: a host that has registered once can always
        // resolve itself, whatever the listing decides to show tomorrow.
        let (base, _calls) = stub_litellm(false).await;
        let path = remembered_id_path("remembers", None);

        client_with_state(base, true, Some(path.clone()))
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("register");

        assert_eq!(
            std::fs::read_to_string(&path).expect("remembered").trim(),
            "id-1"
        );
    }

    #[tokio::test]
    async fn a_registration_with_nowhere_to_remember_still_succeeds() {
        // Persistence is an improvement, not a precondition: an unwritable or
        // unconfigured state path must not fail a registration that worked.
        let (base, _calls) = stub_litellm(false).await;
        let registration = client_against(base, true)
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("register");
        assert_eq!(registration.agent_id, "id-1");
    }

    #[test]
    fn the_registration_body_carries_our_skills_not_a_stub() {
        // S9: LiteLLM never fetches `card.url`, so a body without skills means
        // the directory shows its default `[chat]` forever, however reachable
        // the URL is. The card's skills must travel in `agent_card_params`.
        let params = card_params(&test_card("http://192.168.1.33:10001"));
        assert_eq!(
            crate::card::skill_ids(&params),
            vec!["ask".to_string()],
            "agent_card_params must carry the host's skills"
        );
        // And LiteLLM's two non-standard top-level fields are lifted out of the
        // interface, because that is where LiteLLM reads them.
        assert_eq!(params["url"], "http://192.168.1.33:10001");
        assert_eq!(params["protocolVersion"], "1.0");
        assert_eq!(params["name"], "mac-studio-goose");
    }

    #[test]
    fn no_master_key_is_unconfigured_not_a_failure() {
        let mut config = config();
        // A name nothing in the environment can be using. Spelled out rather
        // than given a random-looking suffix: the suffix made the literal read
        // as a credential to a secret scanner (`master_key_env = "…9f3a"`), and
        // it bought nothing — if anything ever did set this variable the test
        // fails loudly rather than passing quietly, which is the same guarantee
        // a random suffix would have given.
        config.registry.master_key_env = "A2A_GOOSE_NO_SUCH_ENV_VAR".to_string();
        let registry = Registry::new(&config);
        assert_eq!(registry.state(), RegistryState::Unconfigured);
        // And spawning is a no-op rather than a panic.
        registry.spawn_registration(&test_card("http://x:1"));
        assert_eq!(registry.state(), RegistryState::Unconfigured);
    }

    #[test]
    fn a_present_master_key_starts_unregistered() {
        let mut config = config();
        config.registry.master_key_env = "A2A_GOOSE_TEST_MASTER_KEY_9f3a".to_string();
        // SAFETY: the variable is set and removed inside this test body, and the
        // name is unique to it.
        unsafe { std::env::set_var("A2A_GOOSE_TEST_MASTER_KEY_9f3a", "sk-test") };
        let registry = Registry::new(&config);
        unsafe { std::env::remove_var("A2A_GOOSE_TEST_MASTER_KEY_9f3a") };
        assert_eq!(registry.state(), RegistryState::Unregistered);
    }

    #[test]
    fn deregistering_an_unregistered_agent_is_a_no_op() {
        let mut config = config();
        config.registry.master_key_env = "A2A_GOOSE_TEST_MASTER_KEY_9f3b".to_string();
        unsafe { std::env::set_var("A2A_GOOSE_TEST_MASTER_KEY_9f3b", "sk-test") };
        let registry = Registry::new(&config);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(registry.deregister());
        unsafe { std::env::remove_var("A2A_GOOSE_TEST_MASTER_KEY_9f3b") };
        assert_eq!(registry.state(), RegistryState::Unregistered);
    }

    /// LiteLLM 1.103.0's actual `POST /v1/agents` response, with the id and name
    /// replaced. The shape is the point: `agent_card_params` is LiteLLM's
    /// *synthesised* card, not the three fields we sent, and its skills are
    /// LiteLLM's defaults rather than ours.
    const REGISTRATION_RESPONSE: &str =
        include_str!("../tests/fixtures/litellm-agent-registration.json");

    #[test]
    fn a_real_registration_response_parses_into_an_id_and_litellms_own_skills() {
        let registration = parse_registration(REGISTRATION_RESPONSE).expect("parse");
        assert_eq!(
            registration.agent_id,
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            registration.skills,
            vec!["chat".to_string()],
            "the response card is LiteLLM's synthesised default, so this is what \
             `unregistered-pending-fetch` looks like - and why /status reports it"
        );
    }

    #[test]
    fn a_response_without_an_agent_id_is_refused_because_it_could_never_be_deleted() {
        let err = parse_registration(r#"{"agent_name":"x","agent_card_params":{}}"#).unwrap_err();
        assert!(matches!(err, RegistryError::NoAgentId { .. }), "{err}");
        assert!(err.to_string().contains("agent_id"), "{err}");
    }

    #[test]
    fn a_response_that_is_not_json_is_refused() {
        let err = parse_registration("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(matches!(err, RegistryError::NoAgentId { .. }), "{err}");
    }

    #[test]
    fn top_level_skills_are_read_when_there_is_no_card_envelope() {
        // Belt and braces: the SDK's own docs disagree with themselves about
        // which shape is returned, so both are accepted rather than one being
        // silently read as "no skills".
        let registration =
            parse_registration(r#"{"agent_id":"a","skills":[{"id":"ask"}]}"#).expect("parse");
        assert_eq!(registration.skills, vec!["ask".to_string()]);
    }

    #[test]
    fn the_state_serialises_as_a_tagged_object_for_status() {
        let registered = RegistryState::Registered {
            agent_id: "abc".to_string(),
            skills: vec!["ask".to_string()],
        };
        let value = serde_json::to_value(&registered).expect("serialise");
        assert_eq!(value["state"], "registered");
        assert_eq!(value["agentId"], "abc");
        assert_eq!(value["skills"][0], "ask");
        assert!(registered.is_registered());

        assert_eq!(
            serde_json::to_value(RegistryState::Unconfigured).expect("serialise")["state"],
            "unconfigured"
        );
    }

    #[test]
    fn nothing_listed_under_our_name_means_create() {
        assert_eq!(plan(None, true), Plan::Create);
        assert_eq!(plan(None, false), Plan::Create);
    }

    #[test]
    fn our_own_stale_entry_is_rewritten_in_place_rather_than_reposted() {
        // The whole point of the lookup: a host that crashed without
        // deregistering must converge, and `POST` again is a 400 (S13).
        assert_eq!(
            plan(Some("id-1".to_string()), true),
            Plan::Update("id-1".to_string())
        );
    }

    #[test]
    fn a_host_that_may_not_rewrite_reclaims_the_entry_without_touching_its_card() {
        assert_eq!(
            plan(Some("id-1".to_string()), false),
            Plan::Adopt("id-1".to_string())
        );
    }

    /// LiteLLM's `GET /v1/agents` element shape, trimmed to the two fields this
    /// reads: a list of `{agent_id, agent_name}`.
    const LISTING: &str = r#"[
        {"agent_id":"other","agent_name":"someone-else"},
        {"agent_id":"id-1","agent_name":"mac-studio-goose"}
    ]"#;

    #[test]
    fn the_listing_is_searched_by_name_not_assumed_to_be_ours() {
        assert_eq!(
            find_by_name(LISTING, "mac-studio-goose").expect("scan"),
            Some("id-1".to_string())
        );
        assert_eq!(
            find_by_name(LISTING, "a-host-we-are-not").expect("scan"),
            None,
            "another host's entry must never be adopted as ours"
        );
    }

    #[test]
    fn a_listing_that_is_not_a_list_reads_as_not_listed() {
        // A proxy answering `{}` or an HTML error page must not stop a host
        // registering - the next move (create) is the same either way.
        assert_eq!(find_by_name("{}", "mac-studio-goose").expect("scan"), None);
        assert_eq!(find_by_name("<html>", "x").expect("scan"), None);
        assert_eq!(
            find_by_name(r#"[{"agent_name":"mac-studio-goose"}]"#, "mac-studio-goose")
                .expect("scan"),
            None,
            "an entry with no id cannot be adopted"
        );
    }

    /// A stand-in for LiteLLM's registry endpoints, recording what was called.
    ///
    /// Enough of the real surface to be worth testing against: the listing shape,
    /// the `PUT` that updates in place, and — the reason this exists — the hard
    /// `400` a second `POST` gets on a duplicate name. The behaviour under test
    /// is *which* call the client makes, and that is invisible in a unit test of
    /// a pure function.
    #[derive(Clone)]
    struct Stub {
        exists: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }

    async fn stub_registry(
        method: axum::http::Method,
        uri: axum::http::Uri,
        axum::extract::State(stub): axum::extract::State<Stub>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        use axum::http::StatusCode;

        let path = uri.path().to_string();
        stub.calls
            .lock()
            .expect("call log")
            .push(format!("{method} {path}"));

        let registration = serde_json::json!({
            "agent_id": "id-1",
            "agent_name": "mac-studio-goose",
            "agent_card_params": {
                "name": "mac-studio-goose",
                "url": "http://mac-studio.tail86fd19.ts.net:10099",
                "protocolVersion": "1.0",
                "skills": [{"id": "chat", "name": "Chat", "tags": ["chat"],
                            "description": "Conversational interaction with the agent."}],
            },
        });

        match (method.as_str(), path.as_str()) {
            ("GET", "/v1/agents") => (
                StatusCode::OK,
                axum::Json(if stub.exists {
                    serde_json::json!([{"agent_id": "id-1",
                                        "agent_name": "mac-studio-goose"}])
                } else {
                    serde_json::json!([])
                }),
            ),
            ("POST", "/v1/agents") if stub.exists => (
                // Verbatim from the live proxy, and the reason for the lookup.
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "detail": "Agent with name mac-studio-goose already exists"
                })),
            ),
            ("POST", "/v1/agents") => (StatusCode::OK, axum::Json(registration)),
            ("PUT", "/v1/agents/id-1") | ("GET", "/v1/agents/id-1") => {
                (StatusCode::OK, axum::Json(registration))
            }
            _ => (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))),
        }
    }

    async fn stub_litellm(exists: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .fallback(stub_registry)
            .with_state(Stub {
                exists,
                calls: calls.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (format!("http://{addr}"), calls)
    }

    fn client_against(base_url: String, may_rewrite: bool) -> Client {
        client_with_state(base_url, may_rewrite, None)
    }

    /// A client that also has somewhere to remember its agent id. `None` is the
    /// no-persistence case, which is what most tests want; the tests that
    /// exercise the fallback pass a real path in a temporary directory.
    fn client_with_state(
        base_url: String,
        may_rewrite: bool,
        agent_id_path: Option<PathBuf>,
    ) -> Client {
        Client {
            base_url,
            master_key: "sk-test".to_string(),
            agent_name: "mac-studio-goose".to_string(),
            bearer_token: Some("agent-serving-token".to_string()),
            re_register_on_card_change: may_rewrite,
            agent_id_path,
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn a_first_boot_creates_the_entry() {
        let (base, calls) = stub_litellm(false).await;
        let registration = client_against(base, true)
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("register");

        assert_eq!(registration.agent_id, "id-1");
        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["GET /v1/agents", "POST /v1/agents"],
            "nothing listed, so create - and the lookup happens first"
        );
    }

    #[tokio::test]
    async fn a_restart_after_an_unclean_shutdown_updates_and_never_reposts() {
        // The failure this prevents: `POST` returns the 400 the stub would give
        // on a second create, the host logs "registration failed", and it serves
        // a card nobody can discover while looking registered.
        let (base, calls) = stub_litellm(true).await;
        let registration = client_against(base, true)
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("register");

        assert_eq!(
            registration.agent_id, "id-1",
            "our own stale entry, reclaimed"
        );
        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["GET /v1/agents", "PUT /v1/agents/id-1"]
        );
    }

    #[tokio::test]
    async fn a_host_that_may_not_rewrite_cards_adopts_the_entry() {
        let (base, calls) = stub_litellm(true).await;
        let registration = client_against(base, false)
            .register(&test_card("http://mac-studio.tail86fd19.ts.net:10099"))
            .await
            .expect("register");

        assert_eq!(registration.agent_id, "id-1");
        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["GET /v1/agents", "GET /v1/agents/id-1"],
            "adopted, not rewritten and not reposted"
        );
        // And `/status` reports what the directory actually holds, which is
        // LiteLLM's synthesised card rather than ours.
        assert_eq!(registration.skills, vec!["chat".to_string()]);
    }
}
