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
use crate::serve::ServeStatus;
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
    /// The `goose serve` this process started and supervises, when
    /// `goose.acp.serve` is `own`. `None` means the host starts goose itself,
    /// which `/status` says out loud rather than leaving a reader to infer it
    /// from a missing field.
    pub serve: Option<Arc<ServeStatus>>,
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
            // Whether this process found the key `secretEnv` names. Not the key
            // — whether there is one. A host that is about to send every turn
            // into a 401 can see that here rather than inferring it from a log
            // it may not be reading; the header is only attached when this is
            // true.
            "secretSet": crate::acp::secret_key(&agent.config.goose.acp).is_some(),
            // Who runs the ACP server, and — when that is this process — what it
            // is doing. `mode` is the config value, so `external` here means
            // exactly what it means in the file and nothing is inferred.
            "serve": match &agent.serve {
                Some(status) => {
                    let health = status.health();
                    json!({
                        "mode": agent.config.goose.acp.serve.as_str(),
                        "managed": true,
                        "state": health.state,
                        "pid": health.pid,
                        // Spawns after the first: a goose that has been
                        // restarted is visible here rather than only in the log.
                        "restarts": health.restarts,
                    })
                }
                None => json!({
                    "mode": agent.config.goose.acp.serve.as_str(),
                    "managed": false,
                }),
            },
        },
        // The launcher is the process that started this one, on a host that has
        // one. It is a separate question from goose (`acp.serve`) and from this
        // process's own version: it says whether the *supervisor* is the one the
        // release shipped, which nothing else here can see.
        "launcher": launcher(),
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

/// The variable names the launcher exports on the way to its `exec`.
///
/// The launcher is generated (LAUNCHING.md, *What the launcher tells the
/// payload*), and these three strings are the whole contract between it and this
/// process. They are the launcher's own names, not ours: `FETCH_LAUNCH_*` says
/// where the value came from, which matters because nothing else in this process
/// knows anything about it.
pub const LAUNCHER_PATH_ENV: &str = "FETCH_LAUNCH_PATH";
pub const LAUNCHER_VERSION_ENV: &str = "FETCH_LAUNCH_VERSION";
pub const LAUNCHER_SHA256_ENV: &str = "FETCH_LAUNCH_SHA256";

/// The launcher that started this process, as it described itself on the way in.
///
/// `managed: false` means there was no launcher: a test, a developer's
/// `cargo run`, or a payload started by hand. That is a fact worth reporting
/// rather than a gap to fill with a guess — the same shape `acp.serve` uses to
/// say that goose was not started by this process.
///
/// `version` is the release the launcher last verified itself against, which is
/// the only version a launcher has: it is fetched fresh from whichever release
/// is current rather than versioned on its own. `sha256` is the one to hold
/// against a release manifest's `launcher.sha256` — they are equal exactly when
/// the host's launcher is current, so this is what makes "we shipped a launcher"
/// checkable from outside the box. Both can lag the payload by a single start,
/// because a launcher self-update takes effect on the next one.
///
/// Read from the environment on every call rather than snapshotted: the values
/// are what this process was started with, and they cannot change under it.
fn launcher() -> Value {
    launcher_from(|name| std::env::var(name).ok())
}

fn launcher_from(read: impl Fn(&str) -> Option<String>) -> Value {
    // An empty export is the launcher saying "I could not work this out" (no
    // `sha256` tool on the host, `$0` it could not resolve), which is not the
    // same as a value, and must not read as one.
    let field = |name: &str| read(name).filter(|value| !value.is_empty());
    match field(LAUNCHER_PATH_ENV) {
        Some(path) => json!({
            "managed": true,
            "path": path,
            "version": field(LAUNCHER_VERSION_ENV).unwrap_or_default(),
            "sha256": field(LAUNCHER_SHA256_ENV).unwrap_or_default(),
        }),
        None => json!({ "managed": false }),
    }
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

    use crate::config::ServeMode;

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
            serve: None,
            started: Instant::now(),
        }
    }

    /// `/status` says who starts goose, and — when that is this process — what
    /// the child it started is doing. The `external` branch reports `managed:
    /// false` rather than omitting the field, because the two are different
    /// states and a reader should not have to infer which one they are in.
    #[test]
    fn status_says_who_starts_goose_and_what_that_server_is_doing() {
        let mut managed = agent();
        managed.config.goose.acp.serve = ServeMode::Own;
        managed.serve = Some(Arc::new(ServeStatus::new()));
        let payload = status_payload(&managed);
        assert_eq!(payload["acp"]["serve"]["mode"], "own");
        assert_eq!(payload["acp"]["serve"]["managed"], true);
        assert_eq!(payload["acp"]["serve"]["state"], "starting");
        assert!(payload["acp"]["serve"]["pid"].is_null());
        assert_eq!(payload["acp"]["serve"]["restarts"], 0);

        let mut external = agent();
        external.config.goose.acp.serve = ServeMode::External;
        let payload = status_payload(&external);
        assert_eq!(payload["acp"]["serve"]["mode"], "external");
        assert_eq!(payload["acp"]["serve"]["managed"], false);
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
    fn status_reports_the_launcher_that_started_this_process() {
        // The pair an operator holds against a release manifest's
        // `launcher.sha256`: equal exactly when the host's launcher is current.
        let payload = launcher_from(|name| match name {
            LAUNCHER_PATH_ENV => {
                Some("/Users/nick/.local/share/a2a-goose/fetch-launch.sh".to_string())
            }
            LAUNCHER_VERSION_ENV => Some("0.1.18".to_string()),
            LAUNCHER_SHA256_ENV => {
                Some("9270793a4c4f6410b526872c57fd81759c2266562fa74ea8e26aed1690220da2".to_string())
            }
            _ => None,
        });

        assert_eq!(payload["managed"], true);
        assert_eq!(
            payload["path"],
            "/Users/nick/.local/share/a2a-goose/fetch-launch.sh"
        );
        assert_eq!(payload["version"], "0.1.18");
        assert_eq!(
            payload["sha256"],
            "9270793a4c4f6410b526872c57fd81759c2266562fa74ea8e26aed1690220da2"
        );
    }

    #[test]
    fn status_says_so_when_there_is_no_launcher() {
        // A test, a developer's `cargo run`, a payload started by hand: there is
        // no launcher to report, and inventing one would make the digest
        // meaningless on the hosts where it matters.
        assert_eq!(launcher_from(|_| None), json!({ "managed": false }));
        // An empty export is the launcher saying it could not work the value
        // out, which is not a value.
        assert_eq!(
            launcher_from(|name| match name {
                LAUNCHER_PATH_ENV => Some(String::new()),
                _ => Some("0.1.18".to_string()),
            }),
            json!({ "managed": false })
        );
    }

    #[test]
    fn status_keeps_a_launcher_that_could_not_describe_itself() {
        // A host with no `sha256sum` still has a launcher, and the path is how
        // an operator finds it; the empty fields are the honest answer.
        let payload = launcher_from(|name| match name {
            LAUNCHER_PATH_ENV => Some("/opt/goose/fetch-launch.sh".to_string()),
            LAUNCHER_VERSION_ENV => Some(String::new()),
            LAUNCHER_SHA256_ENV => Some(String::new()),
            _ => None,
        });

        assert_eq!(payload["managed"], true);
        assert_eq!(payload["path"], "/opt/goose/fetch-launch.sh");
        assert_eq!(payload["version"], "");
        assert_eq!(payload["sha256"], "");
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
