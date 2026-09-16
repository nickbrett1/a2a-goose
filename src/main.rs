//! The node agent's entry point: verify goose, resolve the card, serve.
//!
//! One process per host. Not per project — `session/new` takes a working
//! directory, so one `goose serve` behind this binary handles every project on
//! the machine, and `cwd` is the namespace.
//!
//! Everything that can be known wrong at startup is refused here, in the order
//! that fails fastest: goose (a host prerequisite that is verified and never
//! installed, constraint #8), then the configuration and the skill catalogue,
//! then the bearer token, then the listen address. Nothing binds a port until
//! all of them are good, because an agent that starts and then cannot serve is
//! worse than one that does not start: it advertises skills in the LiteLLM
//! registry and fails every turn.

use std::sync::Arc;
use std::time::Instant;

use a2a_goose::acp::AcpTurns;
use a2a_goose::config::Config;
use a2a_goose::goose::{Goose, MIN_GOOSE_VERSION};
use a2a_goose::registry::Registry;
use a2a_goose::server::Agent;
use a2a_goose::skills::SkillSet;
use a2a_goose::turn::Turns;
use a2a_goose::{card, server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "refusing to start");
            eprintln!("a2a-goose: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let goose = Goose::verify()?;
    tracing::info!(
        path = %goose.path.display(),
        version = %goose.version,
        required = %MIN_GOOSE_VERSION,
        "goose verified"
    );

    let config = Config::load()?;
    let skills = SkillSet::load(&config.skills)?;
    let card = card::assemble(&config, &skills);
    let card_hash = card::hash(&card);
    tracing::info!(
        name = %card.name,
        skills = ?skills.ids(),
        card_hash = %card_hash,
        "card assembled"
    );

    // Read before binding a port: an endpoint with no token is one that says
    // "authenticated" and means it (constraint #3).
    let token: Arc<str> = Arc::from(config.bearer_token()?.as_str());

    let addr: std::net::SocketAddr = config.server.bind.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, url = %config.server.public_url, "a2a-goose listening");

    // Registration is best-effort and in the background: boot must not depend on
    // the proxy being up (§9). The handle is kept so a clean shutdown can
    // deregister — and so `/status` can say what happened.
    let registry = Registry::new(&config);

    // The ACP connection is deliberately *not* made here. `goose serve` may not
    // be up yet (the supervisor starts both), and a node agent that refuses to
    // boot because the thing next to it is still starting is a node agent that
    // never converges. The first turn connects; `/status` reports which state
    // that is in. The url is logged so a misconfigured one is visible at boot
    // rather than at the first caller's expense.
    let config = Arc::new(config);
    let turns: Arc<dyn Turns> = Arc::new(AcpTurns::new(Arc::clone(&config)));
    tracing::info!(
        acp = %config.goose.acp.url,
        max_concurrent_sessions = config.registry.limits.max_concurrent_sessions,
        "turns will run over ACP"
    );
    registry.spawn_registration(
        config.server.public_url.clone(),
        config.card.protocol_version.clone(),
    );

    let agent = Arc::new(Agent {
        config: (*config).clone(),
        skills: Arc::new(skills),
        card,
        card_hash,
        goose,
        registry: registry.clone(),
        turns,
        started: Instant::now(),
    });

    axum::serve(listener, server::router(agent, token))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Clean shutdown only, and never a liveness mechanism: OOM, a host sleep or
    // a wedged process all skip this, which is exactly why the sweeper exists.
    registry.deregister().await;
    Ok(())
}

/// Resolves when the supervisor asks us to stop.
///
/// Both signals, because the two init systems in play differ: launchd sends
/// `SIGTERM`, and Ctrl-C in a terminal sends `SIGINT`. A process that only
/// handled one would be deregistering on macOS and not on DSM, or the reverse.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => tracing::warn!(error = %err, "cannot listen for SIGTERM"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT: shutting down"),
        _ = terminate => tracing::info!("SIGTERM: shutting down"),
    }
}
