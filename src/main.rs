//! The node agent's entry point: verify goose, then serve.
//!
//! One process per host. Not per project — `session/new` takes a working
//! directory, so one `goose serve` behind this binary handles every project on
//! the machine, and `cwd` is the namespace.

use std::net::SocketAddr;
use std::time::Instant;

use a2a_goose::goose::{Goose, MIN_GOOSE_VERSION};
use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

/// Where the A2A surface listens. Loopback by default; a Tailscale address is
/// the usual override, because the dialer is the LiteLLM *container* and its
/// `card.url` has to be an address that container can resolve.
const BIND_ENV: &str = "A2A_GOOSE_BIND";
const DEFAULT_BIND: &str = "127.0.0.1:10001";

#[derive(Clone)]
struct Agent {
    goose: Goose,
    started: Instant,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // goose is verified before anything binds a port. A node agent with no ACP
    // server behind it is not a degraded agent, it is a broken promise: it
    // would advertise skills in the LiteLLM registry and then fail every turn.
    // Hard constraint #8 - verify, never install, never start degraded.
    let goose = match Goose::verify() {
        Ok(goose) => goose,
        Err(err) => {
            tracing::error!(error = %err, "refusing to start: goose is not usable on this host");
            eprintln!("a2a-goose: {err}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        path = %goose.path.display(),
        version = %goose.version,
        required = %MIN_GOOSE_VERSION,
        "goose verified"
    );

    let bind = std::env::var(BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let addr: SocketAddr = bind
        .parse()
        .map_err(|err| anyhow::anyhow!("{BIND_ENV}={bind} is not a socket address: {err}"))?;

    let agent = Agent {
        goose,
        started: Instant::now(),
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "a2a-goose listening");
    axum::serve(listener, router(agent)).await?;
    Ok(())
}

fn router(agent: Agent) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .with_state(agent)
}

/// Liveness only, deliberately: this answers "should the supervisor restart
/// me?", so it must stay 200 when something *else* is down. A dead LiteLLM, or
/// a `goose serve` that is restarting, is not a reason for launchd to bounce
/// this process — those are 503s on the A2A calls and lines on `/status`.
async fn healthz() -> &'static str {
    "ok"
}

/// Deep status: "is anything wrong?" — for humans and the sweeper's hang probe.
async fn status(State(agent): State<Agent>) -> Json<Value> {
    Json(status_payload(&agent))
}

fn status_payload(agent: &Agent) -> Value {
    json!({
        "status": "ok",
        "goose": {
            "path": agent.goose.path.display().to_string(),
            "version": agent.goose.version.to_string(),
            "minVersion": MIN_GOOSE_VERSION.to_string(),
        },
        "uptimeSecs": agent.started.elapsed().as_secs(),
        // The ACP connection and the LiteLLM registration are wired in M2/M4.
        // They are reported as unconfigured rather than omitted so a reader can
        // tell "not built yet" from "built and unhealthy".
        "acp": { "state": "unconfigured" },
        "registry": { "state": "unconfigured" },
        "sessions": { "count": 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Agent {
        Agent {
            goose: Goose {
                path: "/usr/local/bin/goose".into(),
                version: a2a_goose::goose::Version::new(1, 50, 0),
            },
            started: Instant::now(),
        }
    }

    #[test]
    fn status_reports_the_goose_it_verified() {
        let payload = status_payload(&agent());
        assert_eq!(payload["goose"]["version"], "1.50.0");
        assert_eq!(
            payload["goose"]["minVersion"],
            MIN_GOOSE_VERSION.to_string()
        );
        assert_eq!(payload["acp"]["state"], "unconfigured");
    }

    #[tokio::test]
    async fn healthz_is_liveness_only() {
        assert_eq!(healthz().await, "ok");
    }
}
