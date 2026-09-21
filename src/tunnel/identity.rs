//! Who this agent is to the hub: the `hello` and the per-process `bootId`.
//!
//! Identity is split out from the tunnel because it is the one part that must
//! **not** change across reconnects. The `bootId` is minted once when the process
//! starts and held for its whole life; every `hello` and every `activity` frame
//! carries it. A restart mints a new one, which is what lets the hub's ordering
//! key be `(agentId, bootId, seq)` even though `seq` restarts at zero.

use crate::config::Config;
use crate::tunnel::protocol::{Hello, PROTOCOL_VERSION};

/// The hub's `kind` for a real agent. The `fake_agent` in roost uses
/// `"devcontainer"`; this is the distinct value the fleet view keys on.
pub const DEFAULT_KIND: &str = "a2a-goose";

/// What this agent advertises to the hub.
///
/// `agent_id` is the card's `name` — stable per host, and the same identity
/// LiteLLM registers. `agent_version` is the card's `version`, which is this
/// binary's version; a card that lies about its version makes the hub's diff
/// unreadable, and so does a hello that does.
///
/// `capabilities` is a free string list the hub stores and renders verbatim, so
/// it advertises **only what M2a can actually answer**: `activity` (the publish
/// bridge), `status` (`status.get`) and `sessions` (`sessions.list`). It
/// deliberately omits `history`, `logs` and `reboot` — offering a capability the
/// agent then refuses is worse than not offering it, because the hub would show
/// an operator a control that fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub agent_id: String,
    pub host: String,
    pub kind: String,
    pub agent_version: String,
    pub skills: Vec<String>,
    pub capabilities: Vec<String>,
}

impl Identity {
    /// The capabilities this build implements. See the type comment for why the
    /// list is short.
    pub const CAPABILITIES: [&'static str; 3] = ["activity", "status", "sessions"];

    /// Build the identity from the loaded configuration and the assembled card.
    ///
    /// `skill_ids` are the card's skill ids, in their stable order — the hub
    /// shows them next to the agent, and a hub that lists a skill the agent does
    /// not serve is the same lie as a capability it cannot honour.
    pub fn from_config(
        config: &Config,
        card_name: &str,
        card_version: &str,
        skill_ids: Vec<String>,
    ) -> Self {
        Self {
            agent_id: card_name.to_string(),
            host: hostname(),
            kind: config.hub.kind.clone(),
            agent_version: card_version.to_string(),
            skills: skill_ids,
            capabilities: Self::CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// The `hello` for this identity, stamped with a boot id and start time.
    ///
    /// `protocol_version` is roost's **wire** version ([`PROTOCOL_VERSION`]), not
    /// the A2A card's `"0.3"`/`"1.0"`; the two share a name and mean different
    /// things.
    pub fn hello(&self, boot_id: &str, started_at: &str) -> Hello {
        Hello {
            agent_id: self.agent_id.clone(),
            host: self.host.clone(),
            kind: self.kind.clone(),
            agent_version: self.agent_version.clone(),
            protocol_version: PROTOCOL_VERSION,
            boot_id: boot_id.to_string(),
            started_at: Some(started_at.to_string()),
            skills: self.skills.clone(),
            capabilities: self.capabilities.clone(),
        }
    }
}

/// Mint a fresh boot id: a UUID v4.
///
/// Not derived from the clock or the pid. Two processes that start in the same
/// millisecond, or a restarted process that gets the same pid, must still be two
/// boots — otherwise the hub would merge two streams and read the second boot's
/// `seq = 1` as a duplicate of the first.
pub fn mint_boot_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// This machine's hostname, as the hub's `host` column.
///
/// `HOSTNAME` is not exported by launchd or a DSM boot wrapper, so the kernel is
/// asked directly (`libc` is already a dependency for the goose child's
/// `SIGTERM`). A host with a name longer than [`HOSTNAME_MAX`] is truncated
/// rather than refused: the name is for an operator to read, and a tunnel that
/// would not open over a long name would be a worse failure than a clipped one.
pub fn hostname() -> String {
    const HOSTNAME_MAX: usize = 255;
    let mut buffer = [0u8; HOSTNAME_MAX + 1];
    // SAFETY: the buffer is ours, and `HOSTNAME_MAX + 1` is its true length.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return "unknown".to_string();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

/// The RFC 3339 wall-clock time this process started, for `hello.startedAt`.
///
/// Wall clock, not [`std::time::Instant`]: the hub renders it, so it has to be
/// something a second process can compare against its own clock.
pub fn started_at() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn identity(kind: &str) -> Identity {
        let mut config = Config::default();
        config.hub.kind = kind.to_string();
        Identity::from_config(&config, "a2a-goose-dev", "0.9.1", vec!["ask".to_string()])
    }

    #[test]
    fn boot_ids_are_fresh_and_not_the_pid_or_clock() {
        let a = mint_boot_id();
        let b = mint_boot_id();
        assert!(!a.is_empty());
        assert_ne!(a, b, "two boots must never share an id");
        // A v4 UUID is 36 characters of `8-4-4-4-12`.
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }

    #[test]
    fn the_hello_carries_the_wire_version_not_the_card_version() {
        let hello = identity("a2a-goose").hello("boot-1", "2026-09-21T00:00:00Z");
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            hello.protocol_version, 1,
            "roost's wire version, not the card's"
        );
        let sent = hello.to_value();
        assert_eq!(sent["protocolVersion"], 1);
        assert_eq!(sent["kind"], "a2a-goose");
        assert_eq!(sent["agentId"], "a2a-goose-dev");
        assert_eq!(sent["agentVersion"], "0.9.1");
        assert_eq!(sent["bootId"], "boot-1");
        assert_eq!(sent["host"], hostname());
    }

    #[test]
    fn capabilities_are_only_what_this_build_implements() {
        let identity = identity("a2a-goose");
        assert_eq!(
            identity.capabilities,
            vec!["activity", "status", "sessions"]
        );
        for absent in ["history", "logs", "reboot"] {
            assert!(
                !identity.capabilities.contains(&absent.to_string()),
                "M2a must not advertise {absent}"
            );
        }
    }

    #[test]
    fn the_hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn a_different_configured_kind_is_carried() {
        assert_eq!(identity("devcontainer").kind, "devcontainer");
    }
}
