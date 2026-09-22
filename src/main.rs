//! The node agent's entry point: verify goose, resolve the card, serve.
//!
//! One process per host. Not per project — `session/new` takes a working
//! directory, so one `goose serve` behind this binary handles every project on
//! the machine, and `cwd` is the namespace.
//!
//! Everything that can be known wrong at startup is refused here, in the order
//! that fails fastest: goose (a host prerequisite that is verified and never
//! installed, constraint #8), then the configuration and the skill catalogue,
//! then the bearer token, then the environment the child goose will inherit,
//! then the listen address. Nothing binds a port until all of them are good,
//! because an agent that starts and then cannot serve is worse than one that does
//! not start: it advertises skills in the LiteLLM registry and fails every turn.

use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a_goose::acp::AcpTurns;
use a2a_goose::activity::ActivityHub;
use a2a_goose::config::{Config, ServeMode};
use a2a_goose::goose::{Goose, MIN_GOOSE_VERSION};
use a2a_goose::registry::Registry;
use a2a_goose::serve::{ServeState, ServeStatus, Supervisor, check_child_env};
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
    // Non-fatal, and reported before anything else the operator will read: these
    // are states the agent serves in, not states it refuses (see `Config::warnings`).
    for warning in config.warnings() {
        tracing::warn!(warning = %warning, "misconfiguration");
    }
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

    // The ACP server this agent runs on, started *before* a port is bound and
    // not returned from until goose answers `initialize`. That ordering is the
    // point of owning the process: an agent that binds first and finds out
    // later can advertise skills and accept calls while failing every turn,
    // which is exactly what the first host did after a reboot (nothing had
    // started `goose serve`, and nothing said so).
    let supervisor = match config.goose.acp.serve {
        ServeMode::Own => {
            // The child inherits this environment whole, so a host's identity
            // line has to be one goose can parse - the two spellings it cannot
            // are both refused here rather than discovered per turn.
            check_child_env(|name| std::env::var(name).ok())?;
            Some(Supervisor::start(&config, &goose).await?)
        }
        ServeMode::External => {
            tracing::info!(
                acp = %config.goose.acp.url,
                "goose.acp.serve is external: this host starts goose itself. Nothing is spawned \
                 and nothing is checked, so an agent that comes up without it will answer every \
                 turn with an error"
            );
            None
        }
    };
    let serve_status = supervisor.as_ref().map(Supervisor::status);

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
    // The activity feed (§`activity`): one hub, shared by the ACP runner here and
    // the executor built in `server::router` from `agent.activity`. It is the
    // operator's view of a turn — requests, steps, outcomes — and it is what
    // `GET /events` streams.
    let activity = Arc::new(ActivityHub::new(
        config.observability.activity.enabled,
        config.observability.activity.backlog,
    ));

    let config = Arc::new(config);
    let turns: Arc<dyn Turns> =
        Arc::new(AcpTurns::new(Arc::clone(&config)).with_activity(Arc::clone(&activity)));
    tracing::info!(
        acp = %config.goose.acp.url,
        serve = config.goose.acp.serve.as_str(),
        // Whether goose will want a key is only knowable by asking it, but
        // whether this process *has* one is knowable now — and a host that is
        // about to run every turn into a 401 should hear about it at boot
        // rather than from the first caller.
        acp_secret_set = a2a_goose::acp::client::secret_key(&config.goose.acp).is_some(),
        acp_secret_env = %config.goose.acp.secret_env,
        max_concurrent_sessions = config.registry.limits.max_concurrent_sessions,
        "turns will run over ACP"
    );
    registry.spawn_registration(&card);

    if let Some(status) = &serve_status {
        give_up_if_goose_will_not_stay_up(Arc::clone(status));
    }

    let agent = Arc::new(Agent {
        config: (*config).clone(),
        skills: Arc::new(skills),
        card,
        card_hash,
        goose,
        registry: registry.clone(),
        history: a2a_goose::history::HistoryStore::new(a2a_goose::history::default_db_path()),
        turns,
        serve: serve_status,
        activity,
        started: Instant::now(),
    });

    // The roost tunnel (M2a): this agent dials *out* to the mission-control hub
    // and keeps one long-lived WebSocket open, pushing its activity feed and
    // answering `status.get`/`sessions.list`. Best-effort and off the serving
    // path, like registration: an unreachable hub is a retry, never a reason
    // this host does not serve. `spawn` returns `None` (and says why) when no
    // hub is configured.
    let _tunnel = a2a_goose::tunnel::spawn(&config, Arc::clone(&agent));

    // Bounded graceful drain. `shutdown_signal` closes the listener at once
    // (so `/healthz` goes to 000 while connections finish), but it does not
    // wait on them forever: an SSE `/events` subscriber - and an in-flight A2A
    // turn - is a connection that is meant to stay open, and an unbounded wait
    // on it is a host that never exits. `serve_until` bounds the drain so the
    // steps below, which stop the goose child, always run. See `serve_until`.
    server::serve_until(
        listener,
        server::router(agent, token),
        shutdown_signal(),
        server::SHUTDOWN_GRACE,
    )
    .await?;

    // Stop the process this one started, before this one goes. The watcher is
    // what stops it; this is what makes the *attempt*, so a shutdown does not
    // leave a goose holding the port (and the host's recipes) under a dead
    // agent. `SIGTERM` first, then a grace period, then `SIGKILL` - so a goose
    // that ignores `SIGTERM` is still reaped and never holds the exit open (see
    // `serve::stop_child`).
    if let Some(supervisor) = supervisor {
        supervisor.shutdown().await;
    }

    // Clean shutdown only, and never a liveness mechanism: OOM, a host sleep or
    // a wedged process all skip this, which is exactly why the sweeper exists.
    // Bounded for the same reason as everything above it: a LiteLLM that is
    // down or unreachable costs a log line, never the process's exit.
    if tokio::time::timeout(DEREGISTER_GRACE, registry.deregister())
        .await
        .is_err()
    {
        tracing::warn!(
            grace_secs = DEREGISTER_GRACE.as_secs(),
            "deregistration did not finish in time; exiting anyway"
        );
    }
    Ok(())
}

/// How long the best-effort deregistration gets before the process exits.
///
/// Deregistration is a courtesy to the registry, not a condition of exit: the
/// sweeper reclaims a stale row anyway (see `registry`). A host that will not
/// exit because the proxy is down is the same un-supervisable failure as a
/// shutdown held open by a stream.
const DEREGISTER_GRACE: Duration = Duration::from_secs(3);

/// Ends this process when the goose it started is given up on.
///
/// A goose that cannot stay up means an agent that cannot serve, and a card
/// nobody can call is worse than an agent that is visibly down - so this is a
/// deliberate `exit(1)` rather than a degraded mode that keeps advertising
/// skills. The restart is the init system's job (launchd `KeepAlive`, DSM's
/// boot wrapper), and its throttle is what paces the attempt: hard constraint
/// #9's split, this process supervising goose and the init system supervising
/// this process.
///
/// Abrupt on purpose: goose is already gone, the A2A surface has nothing to
/// deregister *with*, and a graceful path here would only be a slower way to
/// the same exit code.
fn give_up_if_goose_will_not_stay_up(status: Arc<ServeStatus>) {
    tokio::spawn(async move {
        let mut states = status.subscribe();
        while states.changed().await.is_ok() {
            if let ServeState::GaveUp { exits, window_secs } = *states.borrow() {
                tracing::error!(
                    exits,
                    window_secs,
                    "goose serve is gone for good: exiting so the init system starts this host over"
                );
                std::process::exit(1);
            }
        }
    });
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
