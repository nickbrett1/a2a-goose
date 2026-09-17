//! The supervised `goose serve`: this agent starts the ACP server it talks to.
//!
//! Before this module existed, `goose serve` was somebody else's process. The
//! deploy story said so out loud — `deploy/` shipped a unit for *this* agent and
//! nothing for goose — and the first host found the hole: after a reboot the
//! agent came back, bound its port, served a card, accepted calls, and failed
//! every single turn, because nothing had started the ACP leg. A host that looks
//! healthy and answers every call with an error is the worst shape of failure
//! there is, and it is the state this module exists to make impossible.
//!
//! Three rules, and the rest is mechanism:
//!
//! **1. The server this agent dials is the server it started.** `goose.acp.url`
//! is read as an *address* ([`crate::acp::acp_address`]) and handed to goose as
//! `--host`/`--port`, so there is one address and not two. `goose.acp.serve:
//! external` opts out entirely and puts the old problem back, deliberately, for
//! a host whose goose is managed by something else.
//!
//! **2. Nothing is served until goose answers.** After the spawn, `initialize`
//! is retried against the configured endpoint until `acp.timeouts.initializeSecs`
//! runs out; only then does [`crate::main`] bind a port. Three things fall out of
//! that: a goose that cannot start means an agent that does not start (loudly, at
//! boot, in the log the init system keeps); a *wrong key* is caught at boot
//! rather than by the first caller, because goose answers 401 immediately and
//! that is a different kind of failure from "not listening yet" and is not
//! waited out; and after a *crash*, the replacement is not reported as up until
//! it answers, so a goose that comes back mute is not mistaken for a recovered
//! one.
//!
//! **3. An address somebody else is already using is a refusal, not a merge.**
//! If something is listening on the address this agent was told to own, it
//! refuses to start and says what it found. Two `goose serve` processes on one
//! host are two servers over one `sessions.db` and one set of recipes, and the
//! agent cannot tell which one a session landed on. A host that *has* a goose
//! already gets told so, instead of quietly running against a process it does
//! not control and will not restart.
//!
//! **Restarting** is the fourth thing, and it is deliberately small: the child
//! is waited on; when it exits, the agent logs it, waits (1s, doubling, capped at
//! 30s) and starts it again. Failures are counted in a rolling window — more
//! than 5 in 60 seconds means it cannot stay up, so the supervisor gives up and
//! [`crate::main`] exits non-zero. That is the whole escalation: this process
//! supervises goose, and the init system (launchd `KeepAlive`, DSM's boot
//! wrapper) supervises this process, which is hard constraint #9's division of
//! labour and the reason no second supervisor daemon was ever written.
//!
//! What this module does *not* do is reimplement the connection. When goose
//! restarts, the ACP connection dies with it and [`crate::acp::turns`] already
//! knows what that means — the connection and the session pool are dropped
//! together and the next turn reconnects (constraint #1: goose's own
//! `sessions.db` keeps the history; what is lost is each context's label).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::acp::client::AcpClient;
use crate::acp::transport::{AcpAddress, AcpError, acp_address};
use crate::config::{Acp as AcpConfig, Config, ServeMode};
use crate::goose::Goose;

/// The variable *goose* reads its ACP credential from.
///
/// Our side names a variable (`goose.acp.secretEnv`) and never holds a value;
/// goose's side has exactly one name it will accept. This constant is where the
/// first is mapped onto the second.
pub const GOOSE_SECRET_ENV: &str = "GOOSE_SERVER__SECRET_KEY";

/// The variable a host's identity rides to LiteLLM.
///
/// Not ours and not goose's own either: it is goose's passthrough for
/// OpenAI-compatible providers, and LiteLLM turns the `User-Agent` in it into
/// `metadata.user_agent` on every spend-log row. It is read here for one reason —
/// the value has to be *parseable by goose*, and the spellings that are not are
/// the two everyone writes first.
pub const CUSTOM_HEADERS_ENV: &str = "LITELLM_CUSTOM_HEADERS";

/// Refuses an inherited `LITELLM_CUSTOM_HEADERS` that goose cannot read.
///
/// The child goose is handed this process's environment whole, deliberately (see
/// [`spawn_child`]), so a value that merely *looks* like headers is a value that
/// costs every turn this host will ever answer. Two shapes are known-bad, both
/// measured on the NAS against goose 1.50.0 (`spikes/S12.md`):
///
/// - a **JSON object** — `{"User-Agent":"a2a-goose/nas"}` — which is the spelling
///   every template shipped: goose exits `Error invalid HTTP header name`, so
///   every turn answers `-32603 … "Provider not set"`, a message that points at
///   the provider rather than at the header;
/// - **`name=value`**, which goose accepts and drops: the host answers turns, and
///   LiteLLM's rows come back with an empty `user_agent`. The quiet version of
///   the same mistake, and the reason this is a refusal rather than a warning.
///
/// A refusal at startup, before a port is bound, for the same reason the version
/// gate is: an agent that advertises skills and then fails every turn is worse
/// than one that does not start.
pub fn check_child_env(read: impl Fn(&str) -> Option<String>) -> Result<(), ServeError> {
    let Some(value) = read(CUSTOM_HEADERS_ENV) else {
        return Ok(());
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed.starts_with('{') {
        return Err(ServeError::UnusableCustomHeaders {
            shape: "a JSON object",
        });
    }
    if !value.contains(':') && value.contains('=') {
        return Err(ServeError::UnusableCustomHeaders {
            shape: "`name=value`",
        });
    }
    Ok(())
}

/// goose's own refusal, quoted in ours so an operator reads it once, in the
/// place they were already looking.
const GOOSE_ON_MISSING_KEY: &str = "GOOSE_SERVER__SECRET_KEY must be set to start `goose serve`; \
     pass --dangerously-unauthenticated to run without ACP authentication";

/// How many of a child's output lines an error may quote.
const OUTPUT_TAIL: usize = 12;

/// How long the readiness loop waits between attempts at `initialize`.
///
/// Short, because goose is ready in well under a second — measured at 74ms from
/// spawn to a 200 on `initialize` (goose 1.50.0) — and a wasted attempt costs
/// one refused connection.
const READY_POLL: Duration = Duration::from_millis(25);

/// How long the child gets to exit after `SIGTERM` before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long [`Tap::drained`] waits for the readers after the child is gone.
///
/// Not a grace period for goose: the pipe is already closed. it bounds the one
/// case that is not - a process that inherited the pipe and outlived the child.
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);

/// How long the conflict check waits for a connection to the address.
///
/// Aimed at a *listening* server, so a refusal to connect is immediate and this
/// only bounds something firewall-shaped.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// How many times a child may fail, inside what window, before it is given up on.
///
/// Not configurable, deliberately. These numbers are not a host's tuning, they
/// are the point at which "goose cannot stay up" is a fact about the *install*
/// (a bad upgrade, a missing provider, a broken config) rather than a hiccup,
/// and the answer to that is a loud boot failure, not a longer backoff. Five in
/// a minute is wide enough that a goose dying every ten minutes for an hour is
/// restarted without comment.
const BURST: u32 = 5;
const BURST_WINDOW: Duration = Duration::from_secs(60);

/// What a supervised `goose serve` is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeState {
    Starting,
    Ready {
        pid: u32,
    },
    Restarting {
        restarts: u32,
    },
    /// Stopped on request: the child was shut down by *this* process.
    Stopped,
    /// Failed `exits` times inside `window_secs` and was given up on.
    GaveUp {
        exits: u32,
        window_secs: u64,
    },
}

impl ServeState {
    /// The word `/status` and the logs use.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready { .. } => "ready",
            Self::Restarting { .. } => "restarting",
            Self::Stopped => "stopped",
            Self::GaveUp { .. } => "gave_up",
        }
    }
}

/// The supervisor's state, readable without holding anything.
///
/// A `watch` channel rather than a `Mutex<ServeState>` because the two callers
/// want different things: `/status` wants a value *now* and must never be the
/// thing that blocks, and [`crate::main`] wants to be *woken* when the child is
/// given up on. A lock would serve the first and turn the second into a poll.
#[derive(Debug)]
pub struct ServeStatus {
    state: watch::Sender<ServeState>,
    pid: AtomicI32,
    spawns: AtomicU32,
}

/// The snapshot `/status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub state: &'static str,
    pub pid: Option<u32>,
    /// Spawns after the first, so a goose restarted twice reports 2.
    pub restarts: u32,
}

impl ServeStatus {
    /// A status with nothing behind it yet.
    ///
    /// `pub(crate)` rather than private: `/status`'s tests need a handle to
    /// report on, and building one is the only way to see the shape it produces
    /// without a real `goose serve`.
    pub(crate) fn new() -> Self {
        let (state, _) = watch::channel(ServeState::Starting);
        Self {
            state,
            pid: AtomicI32::new(0),
            spawns: AtomicU32::new(0),
        }
    }

    fn set(&self, state: ServeState) {
        // `send_replace` rather than `send`: `send` fails when there is no
        // receiver, and `/status` subscribes *after* the supervisor has already
        // reported `ready` — so a plain `send` would leave the value stuck at
        // `starting` and `/status` would lie about a healthy child.
        self.state.send_replace(state);
    }

    fn spawned(&self, pid: u32) {
        self.pid.store(pid as i32, Ordering::Relaxed);
        self.spawns.fetch_add(1, Ordering::Relaxed);
    }

    fn gone(&self) {
        self.pid.store(0, Ordering::Relaxed);
    }

    pub fn now(&self) -> ServeState {
        self.state.borrow().clone()
    }

    pub fn health(&self) -> Health {
        let pid = self.pid.load(Ordering::Relaxed);
        Health {
            state: self.now().label(),
            pid: (pid > 0).then_some(pid as u32),
            restarts: self.spawns.load(Ordering::Relaxed).saturating_sub(1),
        }
    }

    /// Woken on every state change, including the ones that are not `Ready`.
    pub fn subscribe(&self) -> watch::Receiver<ServeState> {
        self.state.subscribe()
    }
}

/// How a starting `goose serve` is asked whether it is up yet.
///
/// A seam, not indirection for its own sake: the real answer is "a JSON-RPC
/// `initialize` over HTTP", which needs a `goose serve` to answer it — so
/// without this seam, everything about restarting and giving up could only be
/// tested against a real goose on a real host. Tests provide their own answer;
/// production uses [`AcpProbe`], and the ignored live test drives that one.
pub trait Rendezvous: Send + Sync {
    fn probe<'a>(&'a self, acp: &'a AcpConfig) -> BoxFuture<'a, Result<(), AcpError>>;
}

/// The real readiness check: `initialize` on the configured endpoint.
///
/// The same call the first turn would make, with the same key — which is why a
/// 401 here is a *startup* failure. The probe's connection is shut down again:
/// it exists to answer a question, and the turns open their own.
pub struct AcpProbe;

impl Rendezvous for AcpProbe {
    fn probe<'a>(&'a self, acp: &'a AcpConfig) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            let client = AcpClient::connect(acp).await?;
            client.shutdown();
            Ok(())
        })
    }
}

/// Every way owning `goose serve` can fail to start.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(
        "goose.acp.serve is \"{mode}\", so this agent does not start goose, and it refuses to \
         supervise a server it does not own"
    )]
    NotOwned { mode: &'static str },

    #[error("goose.acp.url {url:?} cannot be read as an address to start goose on: {source}")]
    Address {
        url: String,
        source: crate::acp::AcpAddressError,
    },

    #[error(
        "goose.acp.serve is \"own\" and no key is configured, but goose will not start without \
         one. goose says: {GOOSE_ON_MISSING_KEY}. Two ways out, both deliberate: put the key in \
         the variable {secret_env} names (that is what ENV_FILE is for), or set \
         goose.acp.unauthenticated to true to start goose with no ACP authentication at all"
    )]
    NoKey { secret_env: String },

    #[error(
        "goose.acp.serve is \"own\", goose.acp.unauthenticated is true, and {secret_env} is set \
         anyway. The flag wins: goose would start with no ACP authentication while this agent \
         sent a key to a server that does not check one, which is a configuration that lies \
         about what it is protecting. Set unauthenticated to false, or stop defining \
         {secret_env}"
    )]
    KeyAndUnauthenticated { secret_env: String },

    #[error(
        "{address} is already in use: {detail}. This agent starts and owns its own `goose serve` \
         and will not adopt a server it did not start - two of them share one sessions.db and \
         one set of recipes, and a turn could not say which one it landed on. Stop it, or set \
         goose.acp.serve to \"external\" if that server is yours to manage"
    )]
    AlreadyRunning { address: String, detail: String },

    #[error(
        "{CUSTOM_HEADERS_ENV} is not something goose can read - it is {shape}. goose parses that \
         variable as `Name: value` lines (newline-separated when there is more than one header). \
         A JSON object makes goose exit `Error invalid HTTP header name`, so every turn of a host \
         started on one answers -32603 … \"Provider not set\"; a `name=value` spelling is \
         accepted and silently drops the header, so the host answers turns and LiteLLM's rows \
         carry no user_agent. Write it as, for example, 'User-Agent: a2a-goose/<host>' \
         (spikes/S12.md measured both)"
    )]
    UnusableCustomHeaders { shape: &'static str },

    #[error("could not start {path}: nothing was spawned, so nothing is running: {source}")]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error(
        "{path} exited before it answered initialize ({status}), so this agent will not serve a \
         card it cannot fulfil. Its own output, which is also in the log above: {output}"
    )]
    ExitedBeforeReady {
        path: PathBuf,
        status: String,
        output: String,
    },

    #[error(
        "goose never answered initialize on {url} within {waited_secs}s, so it was stopped and \
         this agent will not start. Last reason: {last}. Its own output, which is also in the \
         log above: {output}"
    )]
    NotReady {
        url: String,
        waited_secs: u64,
        last: String,
        output: String,
    },

    #[error(
        "goose answered initialize on {url} and refused it: {reason}. Waiting would not change \
         the answer, so this agent stops here - a key goose does not accept is a boot failure, \
         not a first caller's problem. Its own output, which is also in the log above: {output}"
    )]
    Refused {
        url: String,
        reason: String,
        output: String,
    },
}

/// Failures counted in a rolling window.
///
/// The window is what makes "5 in 60s" a statement about *now*: a goose that
/// died once an hour ago does not count against one that is dying now, and a
/// child that had trouble on Monday is not given up on because of it on Friday.
struct Failures {
    window: Duration,
    burst: u32,
    at: VecDeque<Instant>,
}

impl Failures {
    fn new(window: Duration, burst: u32) -> Self {
        Self {
            window,
            burst,
            at: VecDeque::new(),
        }
    }

    /// Records a failure and says whether it is one too many.
    fn note(&mut self, now: Instant) -> bool {
        self.at.push_back(now);
        while let Some(oldest) = self.at.front() {
            if now.saturating_duration_since(*oldest) > self.window {
                self.at.pop_front();
            } else {
                break;
            }
        }
        self.at.len() as u32 > self.burst
    }

    fn count(&self) -> u32 {
        self.at.len() as u32
    }
}

/// How hard to try before giving the child up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    pub first_delay: Duration,
    pub max_delay: Duration,
    pub burst: u32,
    pub window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            burst: BURST,
            window: BURST_WINDOW,
        }
    }
}

impl RestartPolicy {
    /// The wait before attempt `n` (1-based): doubling, then flat.
    fn delay(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(20);
        let millis = self.first_delay.as_millis() as u64 * 2u64.pow(doublings);
        Duration::from_millis(millis).min(self.max_delay)
    }
}

/// A `goose serve` this process started, and the task that keeps it running.
#[derive(Debug)]
pub struct Supervisor {
    status: Arc<ServeStatus>,
    shutdown: oneshot::Sender<()>,
    watcher: JoinHandle<()>,
}

impl Supervisor {
    /// Starts goose for `config` and returns once it is answering.
    ///
    /// The returned supervisor is already in shape: goose has answered
    /// `initialize`, a watcher is restarting it when it dies, and
    /// [`Supervisor::shutdown`] will stop it. Anything short of that is an
    /// `Err` with *nothing left running* — a failed start must not leave a child
    /// behind for the next attempt to trip over.
    pub async fn start(config: &Config, goose: &Goose) -> Result<Self, ServeError> {
        Self::start_with(config, goose, RestartPolicy::default(), Arc::new(AcpProbe)).await
    }

    /// [`Supervisor::start`], with the restart policy and the readiness check
    /// supplied — which is what makes all of this testable without a goose.
    pub async fn start_with(
        config: &Config,
        goose: &Goose,
        policy: RestartPolicy,
        prober: Arc<dyn Rendezvous>,
    ) -> Result<Self, ServeError> {
        let acp = config.goose.acp.clone();
        if acp.serve != ServeMode::Own {
            return Err(ServeError::NotOwned {
                mode: acp.serve.as_str(),
            });
        }

        let address = acp_address(&acp.url).map_err(|source| ServeError::Address {
            url: acp.url.clone(),
            source,
        })?;

        // The key is the host's, never the config's: `secret_env` names the
        // variable, and this lookup is what turns the name into the value.
        let secret = crate::acp::secret_key(&acp);
        match (secret.as_deref(), acp.unauthenticated) {
            (None, false) => {
                return Err(ServeError::NoKey {
                    secret_env: acp.secret_env.clone(),
                });
            }
            (Some(_), true) => {
                return Err(ServeError::KeyAndUnauthenticated {
                    secret_env: acp.secret_env.clone(),
                });
            }
            _ => {}
        }

        // Before spawning: an address in use is not a race to lose, it is a fact
        // to report. Spawning first would mean goose exiting with "Address
        // already in use" and the operator reading it out of a child's log.
        if let Some(detail) = occupied(&address, &acp).await {
            return Err(ServeError::AlreadyRunning {
                address: address.to_string(),
                detail,
            });
        }

        let status = Arc::new(ServeStatus::new());
        let (shutdown, mut stop) = oneshot::channel::<()>();

        let (mut child, tap) = spawn_child(
            &goose.path,
            &address,
            secret.as_deref(),
            acp.unauthenticated,
        )
        .await
        .map_err(|source| ServeError::Spawn {
            path: goose.path.clone(),
            source,
        })?;

        match wait_ready(&mut child, &acp, prober.as_ref(), &mut stop).await {
            Readiness::Ready => {}
            Readiness::Exited(exit) => {
                return Err(ServeError::ExitedBeforeReady {
                    path: goose.path.clone(),
                    status: exit.to_string(),
                    output: tap.drained().await,
                });
            }
            // A child that never answers is stopped here rather than left to the
            // watcher: the caller is about to exit, and an orphan would hold the
            // port against the next start.
            Readiness::NotReady { last, waited } => {
                stop_child(&mut child).await;
                return Err(ServeError::NotReady {
                    url: acp.url.clone(),
                    waited_secs: waited,
                    last,
                    output: tap.drained().await,
                });
            }
            Readiness::Refused { reason } => {
                stop_child(&mut child).await;
                return Err(ServeError::Refused {
                    url: acp.url.clone(),
                    reason,
                    output: tap.drained().await,
                });
            }
            Readiness::Cancelled => {
                stop_child(&mut child).await;
                return Err(ServeError::NotReady {
                    url: acp.url.clone(),
                    waited_secs: 0,
                    last: "the start was cancelled".to_string(),
                    output: tap.drained().await,
                });
            }
        }

        let pid = pid_of(&child);
        status.spawned(pid);
        status.set(ServeState::Ready { pid });
        tracing::info!(
            address = %address,
            pid,
            goose = %goose.path.display(),
            version = %goose.version,
            "started goose serve and it is answering"
        );

        let watcher = tokio::spawn(supervise(
            child,
            tap,
            Arc::clone(&status),
            acp,
            address,
            goose.path.clone(),
            secret,
            policy,
            stop,
            prober,
        ));
        Ok(Self {
            status,
            shutdown,
            watcher,
        })
    }

    pub fn status(&self) -> Arc<ServeStatus> {
        Arc::clone(&self.status)
    }

    /// Stops the child this process started, and waits for it to be gone.
    ///
    /// `SIGTERM` first: goose owns the sessions it is holding, and a signal it
    /// can act on beats one it cannot (tokio's `Child::kill` is `SIGKILL` and
    /// nothing else).
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        if self.watcher.await.is_err() {
            tracing::warn!("the goose serve watcher did not finish cleanly");
        }
    }
}

/// The watcher: one child's failure, then the next child.
///
/// An explicit two-state loop rather than nested loops, because the number that
/// decides when to give up is "failures in the window" and not every way a child
/// can fail is an exit: a spawn that fails, and a child that comes up but never
/// answers, both count, and neither involves waiting on a process that is
/// already gone.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    child: Child,
    tap: Tap,
    status: Arc<ServeStatus>,
    acp: AcpConfig,
    address: AcpAddress,
    goose: PathBuf,
    secret: Option<String>,
    policy: RestartPolicy,
    mut stop: oneshot::Receiver<()>,
    prober: Arc<dyn Rendezvous>,
) {
    enum Step {
        Run(Child),
        Spawn,
    }

    let mut step = Step::Run(child);
    let mut failures = Failures::new(policy.window, policy.burst);

    loop {
        // `mem::replace` because the child is moved out of the branch it is
        // used in, and every branch ends by saying what the next step is.
        match std::mem::replace(&mut step, Step::Spawn) {
            Step::Run(mut child) => {
                let pid = pid_of(&child);
                let exit = tokio::select! {
                    _ = &mut stop => None,
                    status = child.wait() => Some(status),
                };
                let Some(exit) = exit else {
                    stop_child(&mut child).await;
                    status.gone();
                    status.set(ServeState::Stopped);
                    tracing::info!(pid, "stopped the goose serve this agent started");
                    return;
                };
                status.gone();
                match exit {
                    Ok(exit) => {
                        let output = tap.drained().await;
                        tracing::error!(
                            pid,
                            %exit,
                            %output,
                            "the goose serve this agent started has exited"
                        )
                    }
                    Err(err) => tracing::error!(pid, %err, "lost the goose serve child"),
                }
                if note_failure(&mut failures, &policy, &status, &mut stop).await {
                    return;
                }
                step = Step::Spawn;
            }

            Step::Spawn => {
                status.set(ServeState::Restarting {
                    restarts: status.health().restarts,
                });
                let (mut child, child_tap) =
                    match spawn_child(&goose, &address, secret.as_deref(), acp.unauthenticated)
                        .await
                    {
                        Ok(spawned) => spawned,
                        Err(err) => {
                            tracing::error!(
                                %err,
                                goose = %goose.display(),
                                "could not start goose serve again"
                            );
                            if note_failure(&mut failures, &policy, &status, &mut stop).await {
                                return;
                            }
                            continue;
                        }
                    };

                let pid = pid_of(&child);
                status.set(ServeState::Starting);
                match wait_ready(&mut child, &acp, prober.as_ref(), &mut stop).await {
                    Readiness::Ready => {
                        status.spawned(pid);
                        status.set(ServeState::Ready { pid });
                        tracing::info!(pid, %address, "goose serve is answering again");
                        step = Step::Run(child);
                    }
                    Readiness::Cancelled => {
                        stop_child(&mut child).await;
                        status.set(ServeState::Stopped);
                        tracing::info!(pid, "shutdown during a restart: not starting another");
                        return;
                    }
                    outcome => {
                        // The child is stopped here whatever it did: a process
                        // that is up but mute would otherwise hold the port
                        // while the next attempt tries to bind it.
                        stop_child(&mut child).await;
                        let output = child_tap.drained().await;
                        match &outcome {
                            Readiness::Exited(exit) => tracing::error!(
                                pid,
                                %exit,
                                %output,
                                "goose serve exited before it answered initialize"
                            ),
                            Readiness::NotReady { last, .. } => tracing::error!(
                                pid,
                                %last,
                                %output,
                                "goose serve came up but did not answer initialize"
                            ),
                            Readiness::Refused { reason } => tracing::error!(
                                pid,
                                %reason,
                                %output,
                                "goose serve answered initialize and refused it"
                            ),
                            _ => {}
                        }
                        if note_failure(&mut failures, &policy, &status, &mut stop).await {
                            return;
                        }
                        step = Step::Spawn;
                    }
                }
            }
        }
    }
}

/// Counts a failure, then either gives up or waits the backoff out.
///
/// Returns whether the watcher should stop: `true` for "given up" and for
/// "shutdown arrived", which are the two ways this ends.
async fn note_failure(
    failures: &mut Failures,
    policy: &RestartPolicy,
    status: &ServeStatus,
    stop: &mut oneshot::Receiver<()>,
) -> bool {
    if failures.note(Instant::now()) {
        status.set(ServeState::GaveUp {
            exits: failures.count(),
            window_secs: policy.window.as_secs(),
        });
        tracing::error!(
            failures = failures.count(),
            window_secs = policy.window.as_secs(),
            "goose serve will not stay up, so this agent will stop rather than serve a card it \
             cannot fulfil. The init system is what starts it again"
        );
        return true;
    }

    let delay = policy.delay(failures.count());
    tracing::warn!(
        attempt = failures.count(),
        delay_ms = delay.as_millis() as u64,
        "restarting goose serve"
    );
    if wait_or_stop(delay, stop).await {
        return false;
    }
    status.set(ServeState::Stopped);
    true
}

/// Sleeps, unless shutdown arrives first. `false` means shutdown won.
async fn wait_or_stop(delay: Duration, stop: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = &mut *stop => false,
    }
}

/// The answer from a starting goose: one of four things.
enum Readiness {
    Ready,
    /// It exited on its own — a missing provider, a bad flag, a port taken
    /// between the conflict check and the spawn.
    Exited(ExitStatus),
    /// Nothing was listening for the whole deadline.
    NotReady {
        last: String,
        waited: u64,
    },
    /// Something answered, and it was not an acceptance: a status, a JSON-RPC
    /// error, an accepted request that never got a reply.
    Refused {
        reason: String,
    },
    Cancelled,
}

/// Waits for `initialize` to be answered, or for the child to exit, or for the
/// deadline.
///
/// The three-way wait is what keeps the boot honest. "Not listening yet" is
/// retried; "answered and said no" is *not* (goose's 401 arrives instantly, and
/// spending ten seconds pretending to wait would only delay the same sentence);
/// and a child that dies while we wait is reported with its own output rather
/// than as a timeout.
async fn wait_ready(
    child: &mut Child,
    acp: &AcpConfig,
    prober: &dyn Rendezvous,
    stop: &mut oneshot::Receiver<()>,
) -> Readiness {
    let started = Instant::now();
    let until = started + Duration::from_secs(acp.timeouts.initialize_secs);
    let mut last = "nothing is listening yet".to_string();

    loop {
        match probe(acp, prober).await {
            Ok(()) => return Readiness::Ready,
            Err(Probe::Refused { reason }) => return Readiness::Refused { reason },
            Err(Probe::NotUp) => {}
        }

        // `try_wait` rather than `wait`: this loop re-polls, and a `wait` future
        // recreated every 25ms is a future whose cancellation semantics would
        // have to be worth trusting.
        match child.try_wait() {
            Ok(Some(exit)) => return Readiness::Exited(exit),
            Ok(None) => {}
            Err(err) => last = format!("could not check on the child: {err}"),
        }

        let now = Instant::now();
        if now >= until {
            return Readiness::NotReady {
                last,
                waited: now.saturating_duration_since(started).as_secs(),
            };
        }

        tokio::select! {
            _ = tokio::time::sleep(READY_POLL) => {}
            _ = &mut *stop => return Readiness::Cancelled,
        }
    }
}

/// What a probe said, once the awkward part is out of the way.
///
/// The split is the reason this type exists rather than a `Result<(), AcpError>`:
/// "could not connect" is retried, everything else is reported.
enum Probe {
    NotUp,
    Refused { reason: String },
}

async fn probe(acp: &AcpConfig, prober: &dyn Rendezvous) -> Result<(), Probe> {
    match prober.probe(acp).await {
        Ok(()) => Ok(()),
        Err(err) if not_up_yet(&err) => Err(Probe::NotUp),
        Err(err) => Err(Probe::Refused {
            reason: err.to_string(),
        }),
    }
}

/// Is this error "nothing is listening yet"?
///
/// A connection failure is, and so is an HTTP-level timeout: goose binds its
/// socket before it can serve, so a request that arrives in that window can hang
/// without anything being wrong. Everything else — a status, a JSON-RPC error,
/// an `initialize` that was *accepted* and then never answered, a missing
/// connection id — is a server that is there and saying no, which no amount of
/// waiting changes.
fn not_up_yet(err: &AcpError) -> bool {
    match err {
        AcpError::Request(source) => source.is_connect() || source.is_timeout(),
        _ => false,
    }
}

/// What, if anything, is already on the address this agent was told to own.
///
/// Two steps, cheapest first: a TCP connection, which has no side effects, and
/// then — only if that succeeded — an `initialize`, so the refusal can say *what*
/// is there rather than only that something is. Connecting to a server that is
/// about to be reported as an obstacle is worth one short request.
async fn occupied(address: &AcpAddress, acp: &AcpConfig) -> Option<String> {
    let target = (address.host.as_str(), address.port);
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(target)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) | Err(_) => return None,
    }

    // A one-second budget: this is a diagnosis, and a diagnosis that hangs is
    // worse than a vague one.
    let mut short = acp.clone();
    short.timeouts.initialize_secs = 1;
    match AcpProbe.probe(&short).await {
        Ok(()) => Some(
            "something is listening there and it answered an ACP `initialize`, so it is almost \
             certainly the goose serve this host already runs"
                .to_string(),
        ),
        Err(err) => Some(format!(
            "something is listening there but did not answer an ACP `initialize` ({err})"
        )),
    }
}

/// The argument list an owned goose is started with.
///
/// `serve`, on the address the config dials, and nothing else: S1 pinned the
/// bare invocation (`--platform cli`, no `--enable-scheduler`), and this is that
/// verdict enforced in code instead of remembered from a document. A host's own
/// schedule would fire goose sessions with no `contextId` — invisible both to
/// this agent and to LiteLLM's per-thread accounting.
fn serve_args(address: &AcpAddress, unauthenticated: bool) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--host".to_string(),
        address.host.clone(),
        "--port".to_string(),
        address.port.to_string(),
    ];
    if unauthenticated {
        args.push("--dangerously-unauthenticated".to_string());
    }
    args
}

/// Starts the child, with its output echoed into this process's log.
///
/// The environment is inherited, deliberately: a host's goose has its own
/// provider keys, its own `GOOSE_*` variables and its own configuration, and
/// this process was started from the same ENV_FILE. The one variable *added* is
/// goose's own secret, mapped from whichever name `goose.acp.secretEnv` gives it.
async fn spawn_child(
    goose: &Path,
    address: &AcpAddress,
    secret: Option<&str>,
    unauthenticated: bool,
) -> std::io::Result<(Child, Tap)> {
    let mut command = Command::new(goose);
    command
        .args(serve_args(address, unauthenticated))
        // A goose that reads stdin would otherwise read ours. Its output is
        // piped so it can be quoted in a refusal and echoed into our log.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A backstop for the paths this module does not get to run: an unwind
        // drops the `Child` and takes goose with it. A `SIGKILL` of *this*
        // process still leaves the child behind, which is why what stops a
        // host's processes is the init unit rather than this doing it by hand.
        .kill_on_drop(true);
    if let Some(secret) = secret {
        command.env(GOOSE_SECRET_ENV, secret);
    }

    let mut child = command.spawn()?;
    let tap = Tap::default();
    if let Some(stdout) = child.stdout.take() {
        tap.reading(echo(stdout, "stdout", tap.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tap.reading(echo(stderr, "stderr", tap.clone()));
    }
    Ok((child, tap))
}

/// Stops the child: `SIGTERM`, then `SIGKILL` if it is still there.
async fn stop_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        if let Err(err) = signal_term(pid) {
            tracing::warn!(pid, %err, "could not signal goose serve to stop");
        }
    }
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            tracing::warn!("goose serve did not exit after SIGTERM; killing it");
            let _ = child.kill().await;
        }
    }
}

/// `SIGTERM`, which tokio's `Child` does not offer (its `kill` is `SIGKILL`).
#[cfg(unix)]
fn signal_term(pid: u32) -> std::io::Result<()> {
    // SAFETY: `kill` takes a pid and a signal and has no other precondition. The
    // pid is one this process spawned and holds a `Child` for.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if sent == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// macOS and DSM are both unix; this keeps the module honest if that changes.
#[cfg(not(unix))]
fn signal_term(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no SIGTERM on this platform",
    ))
}

fn pid_of(child: &Child) -> u32 {
    child.id().unwrap_or(0)
}

/// The child's own output, kept to the last few lines and echoed to the log.
///
/// Both halves matter. The echo is so that one log has the whole story — a
/// launchd job's log is where an operator looks, and goose's reason for refusing
/// to start is worth exactly nothing sitting in a pipe. The tail is so a
/// *startup* failure can quote it, since the refusal is what the operator is
/// reading at that moment.
#[derive(Clone, Default)]
struct Tap(
    Arc<StdMutex<VecDeque<String>>>,
    Arc<StdMutex<Vec<JoinHandle<()>>>>,
);

impl Tap {
    /// The reader tasks that feed this tap, so their handles can be awaited.
    fn reading(&self, reader: JoinHandle<()>) {
        let mut readers = match self.1.lock() {
            Ok(readers) => readers,
            Err(poisoned) => poisoned.into_inner(),
        };
        readers.push(reader);
    }

    /// The tail, after giving the readers a bounded moment to finish.
    ///
    /// The pipe closes when the child exits, so this is normally instant: both
    /// readers see EOF and end, and the lines are already here. It is bounded
    /// anyway because a *grandchild* can inherit the pipe and hold it open — a
    /// goose that spawned something of its own, or an `sh -c` that outlived it —
    /// and nothing about a startup refusal is worth waiting on indefinitely.
    ///
    /// What this exists to fix: the tail used to be read on the same tick that
    /// `try_wait` reported the exit, with no await in between, so a goose that
    /// printed its reason and died at once could be refused with *"it printed
    /// nothing"* — the one sentence the refusal exists to carry (build 54, a
    /// loaded macOS agent). Reading the tail without this can be a lie; with it,
    /// it is what the child said.
    async fn drained(&self) -> String {
        let readers: Vec<JoinHandle<()>> = match self.1.lock() {
            Ok(mut readers) => readers.drain(..).collect(),
            Err(poisoned) => poisoned.into_inner().drain(..).collect(),
        };
        let _ = tokio::time::timeout(OUTPUT_DRAIN, async move {
            for reader in readers {
                let _ = reader.await;
            }
        })
        .await;
        self.tail()
    }

    fn push(&self, line: String) {
        let mut lines = match self.0.lock() {
            Ok(lines) => lines,
            // A panic while holding this is not a reason to lose the child's log
            // along with everything else.
            Err(poisoned) => poisoned.into_inner(),
        };
        while lines.len() >= OUTPUT_TAIL {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn tail(&self) -> String {
        let lines: Vec<String> = match self.0.lock() {
            Ok(lines) => lines.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        };
        if lines.is_empty() {
            "it printed nothing".to_string()
        } else {
            lines.join(" / ")
        }
    }
}

/// Copies a child's stream into the log and the [`Tap`], and hands back the task.
///
/// The task ends when the stream does, which is when the child exits and its
/// pipe closes. The handle is kept by the [`Tap`] so that a refusal can wait for
/// the last line instead of reading whatever happened to be in the buffer
/// ([`Tap::drained`]); it is not a task anything joins on the normal path, where
/// the child outlives it.
fn echo<R>(reader: R, stream: &'static str, tap: Tap) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "goose_serve", stream, line = %line, "");
            tap.push(line);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(url: &str) -> AcpAddress {
        acp_address(url).expect("test url")
    }

    /// The two spellings goose will not read, and the two it will.
    ///
    /// Measured on the NAS (spikes/S12.md): the JSON one cost a host every turn
    /// it answered, with an error naming the *provider*; the `name=value` one
    /// cost it the attribution it was deployed for, silently. Both were in the
    /// templates.
    #[test]
    fn a_header_value_goose_cannot_parse_is_refused_before_anything_binds() {
        let env = |value: &'static str| {
            move |name: &str| (name == CUSTOM_HEADERS_ENV).then(|| value.to_string())
        };

        for bad in [
            r#"{"User-Agent":"a2a-goose/nas"}"#,
            r#"  {"User-Agent":"a2a-goose/nas"}"#,
            "User-Agent=a2a-goose/nas",
        ] {
            let err = check_child_env(env(bad)).expect_err(bad);
            assert!(
                matches!(err, ServeError::UnusableCustomHeaders { .. }),
                "{bad}: {err}"
            );
            // The message has to name the mistake and the cure, because the
            // symptom it replaces names neither.
            assert!(err.to_string().contains("Name: value"), "{err}");
            assert!(
                err.to_string().contains("User-Agent: a2a-goose/<host>"),
                "{err}"
            );
        }

        for good in [
            "User-Agent: a2a-goose/nas",
            "User-Agent:a2a-goose/nas",
            "x-a: 1\nx-b: 2",
            "",
            "   ",
        ] {
            check_child_env(env(good)).unwrap_or_else(|err| panic!("{good}: {err}"));
        }

        // No launcher, no variable: an unmanaged host is not a broken one.
        check_child_env(|_| None).expect("unset is fine");
    }

    #[test]
    fn an_owned_goose_is_started_with_the_address_the_config_dials_and_nothing_else() {
        // S1's verdict, as code: the bare invocation. No `--platform desktop`,
        // no `--enable-scheduler`.
        assert_eq!(
            serve_args(&address("http://127.0.0.1:3284/acp"), false),
            vec!["serve", "--host", "127.0.0.1", "--port", "3284"]
        );
        // The flag is the only addition, and only when it was asked for.
        assert_eq!(
            serve_args(&address("http://127.0.0.1:3284/acp"), true),
            vec![
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "3284",
                "--dangerously-unauthenticated"
            ]
        );
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_doubling() {
        let policy = RestartPolicy {
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            ..RestartPolicy::default()
        };
        assert_eq!(policy.delay(1), Duration::from_secs(1));
        assert_eq!(policy.delay(2), Duration::from_secs(2));
        assert_eq!(policy.delay(3), Duration::from_secs(4));
        assert_eq!(policy.delay(5), Duration::from_secs(16));
        assert_eq!(policy.delay(6), Duration::from_secs(30), "capped");
        assert_eq!(policy.delay(60), Duration::from_secs(30), "still capped");
        // Attempt 0 does not exist, but it must not underflow into a panic.
        assert_eq!(policy.delay(0), Duration::from_secs(1));
    }

    #[test]
    fn a_burst_is_counted_inside_a_rolling_window() {
        let window = Duration::from_secs(60);
        let mut failures = Failures::new(window, 3);
        let t0 = Instant::now();

        assert!(!failures.note(t0));
        assert!(!failures.note(t0 + Duration::from_secs(1)));
        assert!(!failures.note(t0 + Duration::from_secs(2)));
        assert_eq!(failures.count(), 3);
        // The fourth inside the window is one too many.
        assert!(failures.note(t0 + Duration::from_secs(3)));

        // A failure old enough to leave the window takes its place with it: a
        // goose that died an hour ago does not count against one dying now.
        let mut failures = Failures::new(window, 2);
        for i in 0..3 {
            failures.note(t0 + Duration::from_secs(i));
        }
        assert_eq!(failures.count(), 3);
        assert!(!failures.note(t0 + Duration::from_secs(600)));
        assert_eq!(
            failures.count(),
            1,
            "everything outside the window aged out"
        );
    }

    #[test]
    fn a_status_error_is_not_worth_waiting_out() {
        // A 401 means goose is up and refusing our key. Retrying it would turn a
        // configuration mistake into a startup timeout, ten seconds later.
        assert!(!not_up_yet(&AcpError::Status {
            status: 401,
            body: String::new()
        }));
        assert!(!not_up_yet(&AcpError::Rpc {
            code: -32602,
            message: "invalid".to_string(),
            data: None
        }));
        assert!(!not_up_yet(&AcpError::NoConnectionId));
        assert!(!not_up_yet(&AcpError::Closed));
    }

    #[tokio::test]
    async fn a_closed_port_is_worth_waiting_out() {
        // The retryable answer is "nothing is listening yet", and this is what
        // that looks like from reqwest: a connect error. Port 1 on loopback:
        // nothing is ever listening there.
        let err = reqwest::Client::new()
            .post("http://127.0.0.1:1/acp")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(err.is_connect(), "{err}");
        assert!(not_up_yet(&AcpError::Request(err)));
    }

    #[test]
    fn a_server_that_was_accepted_and_then_went_quiet_is_not_worth_waiting_out() {
        // `initialize` was accepted and no reply arrived: the server is there.
        // Three ways of saying "I am here and I am not serving you".
        assert!(!not_up_yet(&AcpError::Timeout {
            method: "initialize".to_string(),
            secs: 1
        }));
        assert!(!not_up_yet(&AcpError::NoConnectionId));
        assert!(!not_up_yet(&AcpError::Closed));
    }

    #[test]
    fn a_tap_keeps_the_last_lines_and_says_so_when_there_were_none() {
        let tap = Tap::default();
        assert_eq!(tap.tail(), "it printed nothing");
        for i in 0..(OUTPUT_TAIL + 5) {
            tap.push(format!("line {i}"));
        }
        let tail = tap.tail();
        assert!(
            tail.contains(&format!("line {}", OUTPUT_TAIL + 4)),
            "the newest line is in the tail: {tail}"
        );
        assert!(
            !tail.contains("line 0 /"),
            "the oldest lines are dropped rather than accumulated: {tail}"
        );
        assert_eq!(tap.0.lock().unwrap().len(), OUTPUT_TAIL);
    }

    #[test]
    fn health_reports_restarts_rather_than_spawns() {
        let status = ServeStatus::new();
        assert_eq!(
            status.health(),
            Health {
                state: "starting",
                pid: None,
                restarts: 0
            }
        );

        status.spawned(4242);
        status.set(ServeState::Ready { pid: 4242 });
        assert_eq!(
            status.health(),
            Health {
                state: "ready",
                pid: Some(4242),
                restarts: 0,
            },
            "the first child is not a restart"
        );

        status.gone();
        status.spawned(4243);
        assert_eq!(status.health().restarts, 1);
        assert_eq!(status.health().pid, Some(4243));
        status.gone();
        assert_eq!(status.health().pid, None, "no pid once it is gone");
    }

    #[test]
    fn every_state_has_a_word_for_status() {
        assert_eq!(ServeState::Starting.label(), "starting");
        assert_eq!(ServeState::Ready { pid: 1 }.label(), "ready");
        assert_eq!(ServeState::Restarting { restarts: 1 }.label(), "restarting");
        assert_eq!(ServeState::Stopped.label(), "stopped");
        assert_eq!(
            ServeState::GaveUp {
                exits: 6,
                window_secs: 60
            }
            .label(),
            "gave_up"
        );
    }
}
