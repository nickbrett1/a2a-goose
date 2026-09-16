//! The HTTP surface: the card LiteLLM fetches, the A2A JSON-RPC endpoint, and
//! the two control routes.
//!
//! | Route | Purpose |
//! |---|---|
//! | `/.well-known/agent-card.json` | The card LiteLLM fetches at registration |
//! | `POST /` | A2A JSON-RPC (router from `a2a-server`) |
//! | `GET /healthz` | Liveness only — no dependency checks |
//! | `GET /status` | Deep: registry, skills, card hash, limits |
//!
//! `/healthz` and `/status` are deliberately different questions. `/healthz`
//! answers *"should the supervisor restart me?"*, so it stays 200 when something
//! **else** is down: a dead LiteLLM, or a `goose serve` that is restarting, is
//! not a reason for launchd to bounce this process. `/status` answers *"is
//! anything wrong?"* for a human, and for a hang probe — which is why it is not
//! behind the bearer token. It carries no secret: no key, no token, no recipe
//! content, and goose's path is a fact the tailnet already implies.
//!
//! **Bearer auth is a layer, not a handler.** Constraint #3 requires a token on
//! every `POST /`, and §5.4 wants a **401** when it is wrong. The SDK's
//! `RequestAuthorizer` hook would give neither: its error goes back as a
//! JSON-RPC error inside a **200**, because that is what the transport does with
//! handler errors. A `tower` layer can return whatever status it likes, so the
//! token check lives there.

use std::sync::Arc;
use std::time::Instant;

use a2a::AgentCard;
use a2a_server::agent_card::{StaticAgentCard, agent_card_router};
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::task_store::InMemoryTaskStore;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::config::Config;
use crate::executor::{ADVERTISED_METHODS, GooseExecutor};
use crate::goose::{Goose, MIN_GOOSE_VERSION};
use crate::registry::Registry;
use crate::skills::{Dispatch, SkillSet};
use crate::turn::{TurnHealth, Turns};

/// Everything a handler needs. Shared, and never mutated: the mutable state is
/// inside `registry`, inside the SDK's task store, and behind `turns`.
pub struct Agent {
    pub config: Config,
    pub skills: Arc<SkillSet>,
    pub card: AgentCard,
    pub card_hash: String,
    pub goose: Goose,
    pub registry: Registry,
    /// How a turn is actually run. Held here rather than built inside
    /// [`router`] so that `/status` can report on the connection the turns use,
    /// and so an integration test can substitute a fake and pin the A2A wire
    /// without a `goose serve` running.
    pub turns: Arc<dyn Turns>,
    pub started: Instant,
}

/// Builds the router. The token is passed separately from [`Agent`] so it cannot
/// be reached from a handler that only wants the configuration.
pub fn router(agent: Arc<Agent>, bearer_token: Arc<str>) -> Router {
    // The card endpoint is public discovery: LiteLLM fetches it before it has
    // any credential of ours, and the sweeper probes it to decide whether a
    // registry entry is still real.
    let card = agent_card_router(Arc::new(StaticAgentCard::new(agent.card.clone())));

    let handler = Arc::new(DefaultRequestHandler::new(
        GooseExecutor::new(
            agent.skills.clone(),
            Arc::new(agent.config.clone()),
            agent.turns.clone(),
        ),
        InMemoryTaskStore::new(),
    ));
    let a2a =
        jsonrpc_router(handler).layer(middleware::from_fn_with_state(bearer_token, require_bearer));

    let control = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .with_state(agent);

    card.merge(a2a).merge(control)
}

/// Serves until the process is asked to stop.
pub async fn serve(
    listener: tokio::net::TcpListener,
    agent: Arc<Agent>,
    bearer_token: Arc<str>,
) -> std::io::Result<()> {
    axum::serve(listener, router(agent, bearer_token)).await
}

/// Liveness only, deliberately (§6.6).
async fn healthz() -> &'static str {
    "ok"
}

/// Deep status. See the module comment for why this is not authenticated.
async fn status(State(agent): State<Arc<Agent>>) -> Json<Value> {
    Json(status_payload(&agent))
}

pub fn status_payload(agent: &Agent) -> Value {
    let skills: Vec<Value> = agent
        .skills
        .iter()
        .map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                // The *kind* of dispatch, never its content (constraint #12).
                "dispatch": match &skill.dispatch {
                    Dispatch::Ask => "ask",
                    Dispatch::Recipe(_) => "recipe",
                    Dispatch::Instruction(_) => "instruction",
                },
                "default": skill.id == agent.skills.default_id(),
            })
        })
        .collect();

    json!({
        "status": "ok",
        "uptimeSecs": agent.started.elapsed().as_secs(),
        "goose": {
            "path": agent.goose.path.display().to_string(),
            "version": agent.goose.version.to_string(),
            "minVersion": MIN_GOOSE_VERSION.to_string(),
        },
        "card": {
            "name": agent.card.name,
            "version": agent.card.version,
            "protocolVersion": agent
                .card
                .supported_interfaces
                .first()
                .map(|interface| interface.protocol_version.clone())
                .unwrap_or_default(),
            "url": agent
                .card
                .supported_interfaces
                .first()
                .map(|interface| interface.url.clone())
                .unwrap_or_default(),
            // The whole point of hashing: this is what §6.2 compares to decide
            // whether the registry needs to converge.
            "hash": agent.card_hash,
        },
        "skills": skills,
        "methods": ADVERTISED_METHODS,
        "limits": agent.config.registry.limits,
        "attribution": agent.config.registry.attribution,
        "acp": {
            // `connected` and `idle` are both healthy: the connection is made on
            // the first turn, so a host that has served nothing yet is not a
            // host with a problem. `unconfigured` is the one that is a problem,
            // and it is reported rather than omitted so a reader can tell it
            // from a healthy-but-quiet host.
            "state": match agent.turns.health() {
                TurnHealth::Connected => "connected",
                TurnHealth::Idle => "idle",
                TurnHealth::Unavailable => "unconfigured",
            },
            "inFlight": agent.turns.in_flight(),
            "url": agent.config.goose.acp.url,
        },
        "registry": agent.registry.state(),
        "sessions": {
            "count": agent.turns.in_flight(),
            // Sessions held for a context, so that reuse is visible rather than
            // inferred: a host serving a multi-turn conversation reports a
            // steady 1 while `count` returns to 0 between turns, and a host
            // whose sessions are all being closed at the end of every turn
            // reports 0 here and 0 there — which is the difference an operator
            // needs to see when the symptom is "my agent forgets".
            "retained": agent.turns.retained(),
        },
    })
}

/// Rejects anything without the bearer token.
///
/// The comparison is constant-time. `a == b` on a `String` short-circuits on the
/// first differing byte, which leaks the token's prefix to anyone who can
/// measure; a token check is exactly the place where that matters.
async fn require_bearer(
    State(expected): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// Equality with no data-dependent early exit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent() -> Agent {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
        let skills = SkillSet::load(&config.skills).expect("skills");
        let card = crate::card::assemble(&config, &skills);
        let card_hash = crate::card::hash(&card);
        Agent {
            config,
            skills: Arc::new(skills),
            card,
            card_hash,
            goose: Goose {
                path: "/usr/local/bin/goose".into(),
                version: crate::goose::Version::new(1, 50, 0),
            },
            registry: Registry::new(&Config::default()),
            // `/status` must not need a `goose serve` to answer, so a fake is
            // the right thing for a status test: it is the *unavailable* case.
            turns: Arc::new(crate::turn::NoTurns),
            started: Instant::now(),
        }
    }

    #[test]
    fn status_names_the_goose_it_verified_and_the_card_it_serves() {
        let payload = status_payload(&agent());
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["goose"]["version"], "1.50.0");
        assert_eq!(payload["goose"]["path"], "/usr/local/bin/goose");
        assert_eq!(payload["card"]["name"], "a2a-goose");
        assert_eq!(payload["card"]["protocolVersion"], "1.0");
        assert_eq!(
            payload["card"]["url"],
            "http://mac-studio.tail86fd19.ts.net:10001"
        );
        assert_eq!(payload["card"]["hash"].as_str().map(str::len), Some(64));
        assert_eq!(payload["registry"]["state"], "unconfigured");
        assert_eq!(payload["acp"]["state"], "unconfigured");
    }

    #[test]
    fn status_lists_the_skills_and_marks_the_default() {
        let payload = status_payload(&agent());
        let skills = payload["skills"].as_array().expect("skills array");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["id"], "ask");
        assert_eq!(skills[0]["dispatch"], "ask");
        assert_eq!(skills[0]["default"], true);
    }

    #[test]
    fn status_reports_the_loop_bounds_and_never_a_budget() {
        let payload = status_payload(&agent());
        assert_eq!(payload["limits"]["maxIterationsPerTask"], 12);
        assert_eq!(payload["limits"]["maxConcurrentSessions"], 4);
        let rendered = payload.to_string();
        for forbidden in ["budget", "maxBudget", "dollar", "usd"] {
            assert!(
                !rendered.contains(forbidden),
                "the status payload must not imply a dollar control: {rendered}"
            );
        }
    }

    #[test]
    fn status_says_how_many_sessions_are_being_held() {
        // `NoTurns` holds none, which is the honest answer for a process with no
        // ACP runner: not "unknown", and not a number that implies reuse is
        // happening.
        let payload = status_payload(&agent());
        assert_eq!(payload["sessions"]["count"], 0);
        assert_eq!(payload["sessions"]["retained"], 0);
    }

    #[test]
    fn the_bearer_check_is_exact_and_constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn healthz_is_liveness_only() {
        assert_eq!(healthz().await, "ok");
    }
}
