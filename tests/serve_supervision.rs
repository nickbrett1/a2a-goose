//! The supervised `goose serve`: what actually happens to a child process.
//!
//! [`a2a_goose::serve`] decides four things that only exist at the process
//! level, and every one of them is a thing a host noticed or will notice:
//!
//! - a `goose serve` that is already running is **refused**, not adopted;
//! - a goose that cannot start means **this agent does not start**;
//! - a goose that dies is **started again**, with a backoff;
//! - a goose that will not stay up is **given up on**, and the process exits so
//!   the init system can try the whole thing again.
//!
//! The child here is a shell script rather than a goose: these tests are about
//! this process's behaviour towards a child, and a real goose would make every
//! one of them slower and none of them truer. The shell script is better at
//! being *controlled* — it can exit 7 on demand, ignore its arguments, trap
//! `SIGTERM` and leave a file behind to prove it was asked to stop. That last
//! one is the point: "did we signal the child?" is not visible in a return code,
//! because a process this one spawns can be forgotten as easily as it can be
//! killed.
//!
//! The readiness check is injected for the same reason (`Rendezvous`): the real
//! one speaks ACP, and a shell script cannot. What the real one does is covered
//! by the ignored test at the bottom, which runs against a real goose.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;

use a2a_goose::acp::AcpError;
use a2a_goose::config::{Acp as AcpConfig, Config};
use a2a_goose::goose::{Goose, Version};
use a2a_goose::serve::{Rendezvous, RestartPolicy, ServeError, ServeState, Supervisor};

/// A stand-in `goose`: a shell script this process spawns as if it were goose.
struct Stub {
    dir: PathBuf,
    path: PathBuf,
}

impl Stub {
    /// Writes a script named `goose`, executable, that runs `body`.
    ///
    /// `body` is handed the path of this stub's marker file, because the most
    /// interesting thing a stub can do is prove it was *signalled*, and the only
    /// way to see that from here is a file it leaves behind.
    fn new(tag: &str, body: impl FnOnce(&Path) -> String) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "a2a-goose-stub-{tag}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("stub dir");
        let marker = dir.join("signalled");
        let path = dir.join("goose");
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body(&marker))).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("stub executable");
        Self { dir, path }
    }

    /// The stub as the supervisor sees it: a verified goose binary.
    fn goose(&self) -> Goose {
        Goose {
            path: self.path.clone(),
            version: Version::new(1, 50, 0),
        }
    }

    fn marker(&self) -> PathBuf {
        self.dir.join("signalled")
    }

    /// A sibling of [`Self::marker`], for a stub that has to record more than
    /// "I was signalled" - and whose failure needs to say which of two things
    /// went wrong.
    fn beside(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A config that owns goose on `url`, with a one-second readiness deadline so a
/// test that waits for a deadline does not wait ten.
fn config(url: &str) -> Config {
    let mut config = Config::default();
    config.server.public_url = "http://this-host:10001".to_string();
    config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
    config.goose.defaults.cwd = PathBuf::from("/tmp");
    config.goose.acp.url = url.to_string();
    config.goose.acp.timeouts.initialize_secs = 1;
    // No key and no `--dangerously-unauthenticated` is a refusal, and the two
    // tests that need a *spawn* are not about the key. Every test either sets
    // this or uses a stub that never gets spawned.
    config.goose.acp.unauthenticated = true;
    // Through a plainly-named constant rather than as `secret_env = "<TOKEN>"`.
    // That shape is what a secret scanner reads as a credential — it fired on
    // this exact line (GitGuardian, PR #9) — and hoisting the literal changes
    // nothing about the guarantee: the one test that needs this variable unset
    // fails loudly if anything ever sets it.
    config.goose.acp.secret_env = NO_SUCH_ENV.to_string();
    config
}

/// The same config with a longer readiness deadline.
///
/// The subject of a test is sometimes what the *child* does rather than what the
/// clock does, and then the deadline must not be able to end the wait first. It
/// did on a loaded agent: a stub's `exit` lost a race with a one-second deadline
/// and the test reported a mute goose instead (build 54).
fn config_with_deadline(url: &str, secs: u64) -> Config {
    let mut config = config(url);
    config.goose.acp.timeouts.initialize_secs = secs;
    config
}

/// A variable name nothing in the environment is expected to use.
const NO_SUCH_ENV: &str = "A2A_GOOSE_NO_SUCH_ENV_VAR";

/// A port that was free a moment ago.
/// Hands out a port nothing else in this process will be handed.
///
/// Two earlier versions of this were wrong, both in the same direction, and the
/// failures they caused are worth naming because they look like a *product* bug:
/// the conflict refusal fired in tests that had no conflict to refuse.
///
/// Asking the kernel for port 0 and letting it go lets the kernel hand that same
/// port to the next caller, so two tests point their stubs at one port. A
/// counter fixes that, but binding to test freeness does not: a listening socket
/// this process binds and drops was observed still `LISTEN` and still
/// *connectable* afterwards for as long as a stub was alive, which every later
/// test read as "a goose serve is already there". So nothing here binds. The
/// ports come from a per-process block below the kernel's ephemeral range
/// (`ip_local_port_range` starts at 32768), and freeness is checked with a
/// connection — a port with nothing on it refuses one, and a connection cannot
/// leave a listener behind.
async fn free_port() -> u16 {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU16, Ordering};

    static NEXT: OnceLock<AtomicU16> = OnceLock::new();
    // A hundred ports each, in a block this process owns: a stale listener from
    // the last run is in the *last* run's block, not this one.
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    let next = NEXT.get_or_init(|| AtomicU16::new(base));

    loop {
        let port = next.fetch_add(1, Ordering::Relaxed);
        assert!(port < 32_768, "the port allocator ran out of ports");
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            return port;
        }
    }
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/acp")
}

/// Says yes: the child counts as up, whatever it is doing.
struct Up;

impl Rendezvous for Up {
    fn probe<'a>(&'a self, _acp: &'a AcpConfig) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Says "nothing is listening", which is the retryable answer — so a test using
/// it waits out the readiness deadline rather than being refused at once.
struct NeverUp;

impl Rendezvous for NeverUp {
    fn probe<'a>(&'a self, _acp: &'a AcpConfig) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async { Err(nothing_listening().await) })
    }
}

/// A genuine connection failure, from a port nothing serves on.
async fn nothing_listening() -> AcpError {
    let err = reqwest::Client::new()
        .post("http://127.0.0.1:1/acp")
        .send()
        .await
        .expect_err("nothing listens on port 1");
    assert!(err.is_connect(), "{err}");
    AcpError::Request(err)
}

/// Polls the supervisor's own state until `done` says so, or gives up.
async fn until<T>(what: &str, mut done: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = done() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `ETXTBSY`, the errno behind the retry in [`start`].
const ETXTBSY: i32 = 26;

/// How many times [`start`] will retry a busy executable.
const RETRIES: u32 = 50;

/// [`Supervisor::start_with`], retrying the one failure that is not the
/// behaviour under test.
///
/// This is the same race `src/goose.rs` documents at length, now that the child
/// is a shell script too: the tests in one binary run in parallel, each writes
/// its own `goose`, and while one test's `Command` is between `fork` and `exec`
/// its child still holds a writable descriptor to a freshly written script, so
/// the kernel refuses to execute another. The window is microseconds and it is
/// why this looked like a one-in-five flake; a refused start spawns nothing and
/// leaves nothing behind, so retrying is the whole fix. Production cannot hit
/// it — a host's goose is a binary that has sat on disk for a long time.
async fn start(
    config: &Config,
    goose: &Goose,
    policy: RestartPolicy,
    prober: Arc<dyn Rendezvous>,
) -> Result<Supervisor, ServeError> {
    for attempt in 0..RETRIES {
        match Supervisor::start_with(config, goose, policy.clone(), Arc::clone(&prober)).await {
            Err(ServeError::Spawn { source, .. }) if source.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(5 * u64::from(attempt + 1)));
            }
            outcome => return outcome,
        }
    }
    Supervisor::start_with(config, goose, policy, prober).await
}

#[tokio::test]
async fn an_address_somebody_is_already_using_is_a_refusal_and_nothing_is_spawned() {
    // The host that already runs a goose deserves to be told so, and the one
    // thing this must not do is start a second server behind the first one's
    // back.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let stub = Stub::new("occupied", |marker| format!("touch {}", marker.display()));
    let err = start(
        &config(&url(port)),
        &stub.goose(),
        RestartPolicy::default(),
        Arc::new(Up),
    )
    .await
    .expect_err("an occupied address must be refused");

    let ServeError::AlreadyRunning { address, detail } = &err else {
        panic!("expected an occupied-address refusal, got {err}");
    };
    assert!(address.contains(&port.to_string()), "{address}");
    let message = err.to_string();
    assert!(message.contains("already in use"), "{message}");
    // The refusal has to say what it found and what to do about it, because the
    // two ways out are both configuration changes on the *host*.
    assert!(detail.contains("listening"), "{detail}");
    assert!(message.contains("external"), "{message}");
    assert!(
        !stub.marker().exists(),
        "nothing may be spawned for an address this agent will not own"
    );
    drop(listener);
}

#[tokio::test]
async fn a_goose_that_dies_before_it_answers_is_refused_with_its_own_reason() {
    // No `sleep` before the exit, and a deadline far longer than any scheduling
    // delay. The exit is what ends the wait, so the clock cannot end it first,
    // and the output below is carried by the drain rather than by the child
    // staying alive long enough to be read. Both of those were races against a
    // one-second deadline, and on a loaded agent they were lost (build 54).
    let stub = Stub::new("dies", |_marker| {
        "echo boom: no provider configured >&2\nexit 7".to_string()
    });

    let err = start(
        &config_with_deadline(&url(free_port().await), 30),
        &stub.goose(),
        RestartPolicy::default(),
        Arc::new(NeverUp),
    )
    .await
    .expect_err("a child that dies is not a start");

    let ServeError::ExitedBeforeReady { status, output, .. } = &err else {
        panic!("expected an exit, got {err}");
    };
    assert!(status.contains('7'), "the exit status is named: {status}");
    assert!(
        output.contains("no provider configured"),
        "the child's own reason is carried into the refusal: {output}"
    );
    let message = err.to_string();
    assert!(
        message.contains("will not serve a card it cannot fulfil"),
        "{message}"
    );
}

#[tokio::test]
async fn a_goose_that_comes_up_mute_is_stopped_rather_than_left_holding_the_port() {
    // `started` is touched before the loop, so a failure below can say which
    // race was lost: a child that never ran is a different bug from one that was
    // signalled before it had installed its trap. The deadline here is the
    // subject of the test - the SIGTERM comes from *it* - so it is longer than
    // the child needs to install a trap rather than short enough to be quick.
    let stub = Stub::new("mute", |marker| {
        format!(
            "touch {}\ntrap 'touch {}; exit 0' TERM\nwhile :; do sleep 0.1; done",
            marker.with_file_name("started").display(),
            marker.display()
        )
    });

    let err = start(
        &config_with_deadline(&url(free_port().await), 5),
        &stub.goose(),
        RestartPolicy::default(),
        Arc::new(NeverUp),
    )
    .await
    .expect_err("a child that never answers is not a start");

    let ServeError::NotReady { waited_secs, .. } = &err else {
        panic!("expected a readiness failure, got {err}");
    };
    assert!(
        *waited_secs >= 5,
        "it waited the configured five seconds: {err}"
    );
    assert!(
        stub.marker().exists(),
        "the child was asked to stop (SIGTERM), not left to hold the port: {} (it had started: {})",
        err,
        stub.beside("started").exists()
    );
}

#[tokio::test]
async fn a_goose_that_exits_after_it_is_up_is_started_again() {
    let stub = Stub::new("restarts", |_marker| {
        "while :; do sleep 0.1; done".to_string()
    });
    let policy = RestartPolicy {
        first_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(50),
        ..RestartPolicy::default()
    };

    let supervisor = start(
        &config(&url(free_port().await)),
        &stub.goose(),
        policy,
        Arc::new(Up),
    )
    .await
    .expect("a child that is up is a start");
    let status = supervisor.status();

    let first = status.health().pid.expect("the first child has a pid");
    assert_eq!(status.health().restarts, 0);

    // A crash, from the outside: exactly what the OOM killer or a bad upgrade
    // does to goose.
    // SAFETY: `kill` takes a pid and a signal; the pid is one this test just
    // read from the supervisor's own state.
    unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) };

    let (pid, restarts) = until("the replacement child", || {
        let health = status.health();
        match (health.pid, health.restarts) {
            (Some(pid), restarts) if pid != first => Some((pid, restarts)),
            _ => None,
        }
    })
    .await;
    assert_ne!(pid, first, "the replacement is a new process");
    assert_eq!(restarts, 1, "and it is reported as one restart");

    // And shutting down stops the replacement, rather than leaving it behind.
    supervisor.shutdown().await;
    assert_eq!(status.now(), ServeState::Stopped);
    assert_eq!(status.health().pid, None, "no pid once it is stopped");
    // SAFETY: as above; `ESRCH` here is the answer we want.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) };
    assert_eq!(alive, -1, "the child is gone, not orphaned");
}

#[tokio::test]
async fn a_goose_that_will_not_stay_up_is_given_up_on() {
    let stub = Stub::new("crashes", |_marker| "exit 3".to_string());
    let policy = RestartPolicy {
        first_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(20),
        burst: 2,
        window: Duration::from_secs(60),
    };

    let supervisor = start(
        &config(&url(free_port().await)),
        &stub.goose(),
        policy,
        Arc::new(Up),
    )
    .await
    .expect("the first child does start");
    let status = supervisor.status();

    let gave_up = until("the supervisor to give up", || match status.now() {
        ServeState::GaveUp { exits, window_secs } => Some((exits, window_secs)),
        _ => None,
    })
    .await;
    assert_eq!(gave_up.0, 3, "burst + 1 failures before giving up");
    assert_eq!(gave_up.1, 60, "and the window it counted them in");
    assert!(
        status.health().pid.is_none(),
        "nothing is left running to be given up on"
    );

    // Shutdown after giving up is still clean: the watcher has already returned,
    // and waiting on it must not hang.
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_goose_that_cannot_be_started_at_all_says_what_is_missing() {
    // No key, and no permission to run without one. goose's own words are in
    // the refusal, because an operator about to edit a config file wants the
    // message the tool would have given them.
    let mut config = config(&url(free_port().await));
    config.goose.acp.unauthenticated = false;
    let stub = Stub::new("nokey", |_marker| "exit 1".to_string());

    let err = start(
        &config,
        &stub.goose(),
        RestartPolicy::default(),
        Arc::new(Up),
    )
    .await
    .expect_err("no key and no flag is a refusal");
    assert!(
        matches!(err, ServeError::NoKey { .. }),
        "expected a missing-key refusal, got {err}"
    );
    let message = err.to_string();
    assert!(message.contains("GOOSE_SERVER__SECRET_KEY"), "{message}");
    assert!(message.contains("unauthenticated"), "{message}");
    assert!(
        !stub.marker().exists(),
        "nothing is spawned for a configuration goose would refuse anyway"
    );
}

#[tokio::test]
async fn a_key_and_the_unauthenticated_flag_together_are_refused() {
    // A key nobody checks is a configuration that lies about what it protects,
    // and silent ambiguity is what this refusal exists to prevent.
    let mut config = config(&url(free_port().await));
    config.goose.acp.unauthenticated = true;
    // `PATH` rather than an invented variable: it is set in every process that
    // got as far as running a test, and it needs no `set_var`.
    config.goose.acp.secret_env = "PATH".to_string();
    let stub = Stub::new("both", |_marker| "exit 1".to_string());

    let err = start(
        &config,
        &stub.goose(),
        RestartPolicy::default(),
        Arc::new(Up),
    )
    .await
    .expect_err("asking for both is a refusal");
    assert!(
        matches!(err, ServeError::KeyAndUnauthenticated { .. }),
        "expected an ambiguous-authentication refusal, got {err}"
    );
    assert!(err.to_string().contains("PATH"), "{err}");
}

/// Against a **real** `goose serve`. Ignored by default, like `live_acp`.
///
/// ```console
/// GOOSE_BIN=$HOME/.local/bin/goose cargo test --locked --test serve_supervision \
///     -- --ignored --nocapture
/// ```
///
/// This is the test the stubs cannot be: it is the real goose, started by this
/// code, readiness-checked by the real `initialize` probe with a real
/// `X-Secret-Key`, killed, and started again — which is the whole feature.
#[tokio::test]
#[ignore = "needs a real goose binary; set GOOSE_BIN"]
async fn the_real_goose_is_started_checked_restarted_and_stopped() {
    use a2a_goose::serve::ServeState;

    let bin = std::env::var("GOOSE_BIN").unwrap_or_else(|_| "goose".to_string());
    let goose = Goose::verify_bin(&bin).expect("a goose to supervise");

    let port = free_port().await;
    let mut config = config(&url(port));
    // The key path, end to end: the value is read from the name in `secretEnv`
    // and handed to the child as goose's own GOOSE_SERVER__SECRET_KEY.
    // Composed rather than written as one token, for the reason [`NO_SUCH_ENV`]
    // gives: `secret_env = "<TOKEN>"` is the shape that reads as a credential.
    let secret_env = [
        "A2A",
        "GOOSE",
        "LIVE",
        "VAR",
        &std::process::id().to_string(),
    ]
    .join("_");
    // SAFETY: a variable no other test reads, named for this process only.
    unsafe { std::env::set_var(&secret_env, "live-test-key") };
    config.goose.acp.secret_env = secret_env.clone();
    config.goose.acp.unauthenticated = false;
    config.goose.acp.timeouts.initialize_secs = 20;

    let policy = RestartPolicy {
        first_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(50),
        ..RestartPolicy::default()
    };
    let supervisor = start(
        &config,
        &goose,
        policy,
        Arc::new(a2a_goose::serve::AcpProbe),
    )
    .await
    .expect("real goose starts and answers initialize");
    let status = supervisor.status();
    assert_eq!(status.now().label(), "ready");

    let first = status.health().pid.expect("a pid");
    // SAFETY: a pid this test just read from the supervisor's own state.
    unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) };

    let replacement = until("goose to come back", || {
        let health = status.health();
        (health.pid.is_some() && health.pid != Some(first)).then_some(health.pid)
    })
    .await;
    println!("goose {first} -> {replacement:?}");

    supervisor.shutdown().await;
    assert_eq!(status.now(), ServeState::Stopped);
    unsafe { std::env::remove_var(&secret_env) };
}
