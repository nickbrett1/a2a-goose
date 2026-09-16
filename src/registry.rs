//! Registration with LiteLLM: the agent's own entry in the agent directory.
//!
//! Two calls, both against the **API** and never `config.yaml` (constraint #2:
//! config agents are un-evictable, so a dynamic agent that lands there is
//! permanently un-sweepable):
//!
//! ```text
//! POST   /v1/agents          {"agent_name": ..., "agent_card_params": {name, url, protocolVersion}}
//! DELETE /v1/agents/{id}
//! ```
//!
//! **What is deliberately not sent.** LiteLLM 1.103.0 accepts and then silently
//! discards `max_iterations`, `max_budget_per_session` and
//! `require_trace_id_on_calls_by_agent`: the stored agent object simply does not
//! have them (measured, `spikes/S2.md`). Sending them would look like cost
//! control that is not there, which is worse than sending nothing — so the loop
//! bounds live in this process (`config.registry.limits`) and are enforced where
//! they can actually be observed.
//!
//! **Registration failing is not fatal.** A node agent whose proxy is down is
//! still a useful node agent; it serves locally and says `unregistered` on
//! `/status` (§9). What would be fatal is a boot that depends on another
//! service being up.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

/// How many times boot registration is attempted before giving up and reporting
/// `failed`. M4 owns the ongoing retry; M1 gets the agent listed and makes the
/// failure visible.
const REGISTER_ATTEMPTS: u32 = 4;

/// First backoff step; doubles each attempt. Long enough to ride out a proxy
/// restart, short enough that a boot is not held up for minutes.
const REGISTER_BACKOFF: Duration = Duration::from_secs(1);

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
    Status { status: u16, body: String },
    NoAgentId { body: String },
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
        }
    }
}

impl std::error::Error for RegistryError {}

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
    pub fn spawn_registration(&self, public_url: String, protocol_version: String) {
        let Some(client) = self.client.clone() else {
            tracing::info!(
                key = %"LITELLM_MASTER_KEY",
                "no LiteLLM master key in the environment; serving without registering"
            );
            return;
        };

        let registry = self.clone();
        tokio::spawn(async move {
            registry.set_state(RegistryState::Registering);

            let mut backoff = REGISTER_BACKOFF;
            for attempt in 1..=REGISTER_ATTEMPTS {
                match client.register(&public_url, &protocol_version).await {
                    Ok(registration) => {
                        tracing::info!(
                            agent_id = %registration.agent_id,
                            skills = ?registration.skills,
                            "registered with LiteLLM"
                        );
                        // S5's second finding: the response card is not
                        // necessarily ours, so say so rather than implying the
                        // registry mirrors the host.
                        if registration.skills != vec!["chat".to_string()] {
                            tracing::debug!("LiteLLM's card carries our skills back");
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
                        backoff *= 2;
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

/// Reads a `POST /v1/agents` response.
///
/// Split out from the request so the shape is testable against the *real*
/// response, which is committed as `tests/fixtures/litellm-agent-registration.json`
/// — LiteLLM's answer is a contract we depend on and cannot type, so it is
/// pinned rather than assumed.
fn parse_registration(text: &str) -> Result<Registration, RegistryError> {
    let value: Value = serde_json::from_str(text).map_err(|_| RegistryError::NoAgentId {
        body: text.to_string(),
    })?;
    let agent_id = value
        .get("agent_id")
        .and_then(Value::as_str)
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

impl Client {
    async fn register(
        &self,
        public_url: &str,
        protocol_version: &str,
    ) -> Result<Registration, RegistryError> {
        // Exactly the three fields LiteLLM uses. See the module comment for what
        // is deliberately absent, and `spikes/S2.md` for the measurement.
        let body = serde_json::json!({
            "agent_name": self.agent_name,
            "agent_card_params": {
                "name": self.agent_name,
                "url": public_url,
                "protocolVersion": protocol_version,
            },
        });

        let response = self
            .http
            .post(format!("{}/v1/agents", self.base_url))
            .bearer_auth(&self.master_key)
            .json(&body)
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

        parse_registration(&text)
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

    #[test]
    fn no_master_key_is_unconfigured_not_a_failure() {
        let mut config = config();
        // A name nothing in the environment can be using.
        config.registry.master_key_env = "A2A_GOOSE_SURELY_NOT_SET_9f3a".to_string();
        let registry = Registry::new(&config);
        assert_eq!(registry.state(), RegistryState::Unconfigured);
        // And spawning is a no-op rather than a panic.
        registry.spawn_registration("http://x:1".to_string(), "1.0".to_string());
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
}
