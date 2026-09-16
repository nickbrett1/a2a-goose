//! The ACP session lifecycle, on top of [`Transport`].
//!
//! Three methods, in the order [S3](../../spikes/S3.md) recorded them:
//!
//! ```text
//! session/new     {cwd, mcpServers}       -> result.sessionId   (connection stream)
//! session/prompt  {sessionId, prompt[]}   -> result.stopReason  (session stream)
//! session/close   {sessionId}             -> result              (session stream)
//! ```
//!
//! **The stream-open ordering is the load-bearing detail.** `session/new`'s reply
//! arrives on the connection-level stream, which is already open; a session's
//! notifications arrive on a stream selected by `Acp-Session-Id`, which must be
//! opened *before* the first session-scoped request, because a reply that arrives
//! on a stream nobody is reading is simply lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::acp::transport::{AcpError, Scope, Transport};
use crate::config::{Acp as AcpConfig, Config};

/// A refusal to use a working directory, named so the caller can be told which
/// directory was refused and what this host does allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdError {
    /// The caller asked for a directory that does not exist on this host.
    NotFound { cwd: String },
    /// The caller asked for a directory outside every allowed root.
    ///
    /// This is the security boundary (constraint #4): `cwd` is how a caller
    /// chooses what a turn can see, so it is checked here, before `session/new`
    /// — goose validates it too (S4), but relying on the far side of a protocol
    /// to enforce our own boundary would be the wrong place to find out.
    OutsideRoots { cwd: PathBuf, roots: Vec<PathBuf> },
}

impl std::fmt::Display for CwdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { cwd } => write!(f, "cwd {cwd:?} does not exist on this host"),
            Self::OutsideRoots { cwd, roots } => {
                let roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
                write!(
                    f,
                    "cwd {} is outside every allowed root ({}); this host only runs turns \
                     inside an allowed root",
                    cwd.display(),
                    roots.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CwdError {}

/// Resolves a caller-supplied `cwd` against the host's allowed roots.
///
/// Symlinks are resolved *before* the prefix check, so a symlink pointing out of
/// an allowed root cannot be used to leave it. A relative path is refused rather
/// than interpreted, because interpreting it would mean choosing a base
/// directory for the caller, and there is no defensible choice.
pub fn resolve_cwd(config: &Config, requested: Option<&str>) -> Result<PathBuf, CwdError> {
    let requested = requested
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::expand_tilde(&config.goose.defaults.cwd));
    let resolved = requested.canonicalize().map_err(|_| CwdError::NotFound {
        cwd: requested.display().to_string(),
    })?;

    if !resolved.is_dir() {
        return Err(CwdError::NotFound {
            cwd: resolved.display().to_string(),
        });
    }

    let roots: Vec<PathBuf> = config
        .goose
        .defaults
        .allowed_roots
        .iter()
        .map(|root| crate::config::expand_tilde(root))
        // An allowed root that cannot be resolved cannot be matched against, and
        // config validation already refuses a missing one at startup.
        .filter_map(|root| root.canonicalize().ok())
        .collect();

    if roots.iter().any(|root| resolved.starts_with(root)) {
        return Ok(resolved);
    }
    Err(CwdError::OutsideRoots {
        cwd: resolved,
        roots,
    })
}

/// Resolves `goose.acp.secretEnv` to the key goose is expecting, if this process
/// has one.
///
/// The config names the variable and never holds the value (secrets are only ever
/// named — `*Env` fields), so this lookup *is* the feature. It was missing until
/// mac-studio (2026-09-16): the field parsed, nothing read it, no `X-Secret-Key`
/// was ever sent, and the only way to run the release against a key-protected
/// `goose serve` was `--dangerously-unauthenticated`.
///
/// `None` is a legitimate answer, not a misconfiguration to refuse: whether goose
/// wants a key is goose's decision, and it says so with a 401 that names the
/// header — which beats this process guessing and refusing to start.
pub fn secret_key(acp: &AcpConfig) -> Option<String> {
    let name = acp.secret_env.trim();
    if name.is_empty() {
        return None;
    }
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The connection to `goose serve`, plus the timeouts from config.
pub struct AcpClient {
    transport: Arc<Transport>,
    timeouts: crate::config::Timeouts,
}

impl AcpClient {
    /// Connects and `initialize`s.
    pub async fn connect(acp: &AcpConfig) -> Result<Self, AcpError> {
        let timeout = Duration::from_secs(acp.timeouts.initialize_secs);
        Ok(Self {
            transport: Arc::new(
                Transport::connect(&acp.url, secret_key(acp).as_deref(), timeout).await?,
            ),
            timeouts: acp.timeouts.clone(),
        })
    }

    pub fn connection_id(&self) -> &str {
        self.transport.connection_id()
    }

    pub fn in_flight(&self) -> usize {
        self.transport.in_flight()
    }

    /// Creates a session rooted at an already-validated directory, and hands
    /// back the update stream that belongs to it.
    ///
    /// The stream is *returned* rather than kept inside the [`Session`] because
    /// a streamed turn reads updates while awaiting the prompt's reply, and a
    /// session that owned its receiver could not be borrowed for the one and
    /// mutably borrowed for the other. Handing it over also gives the two
    /// callers one shape instead of two: a fresh session and a retained session
    /// both need "a session, and the stream to read it on", and neither has to
    /// ask the session for it. A retained session's stream is the one it was
    /// created with — subscribing again would open a *second* SSE stream for the
    /// same session and deliver every frame twice.
    pub async fn new_session(
        &self,
        cwd: &Path,
    ) -> Result<(Session, mpsc::Receiver<Value>), AcpError> {
        let result = self
            .transport
            .request(
                &Scope::Connection,
                "session/new",
                serde_json::json!({
                    "cwd": cwd.display().to_string(),
                    "mcpServers": [],
                }),
                Duration::from_secs(self.timeouts.initialize_secs),
            )
            .await?;

        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Rpc {
                code: 0,
                message: "session/new returned no sessionId".to_string(),
                data: Some(result.clone()),
            })?
            .to_string();

        // After the id exists and before any session-scoped request. This is the
        // ordering S3 warns about.
        let updates = self.transport.subscribe(&session_id).await?;

        Ok((
            Session {
                scope: Scope::Session(session_id.clone()),
                id: session_id,
                timeout: Duration::from_secs(self.timeouts.prompt_secs),
                transport: Arc::clone(&self.transport),
            },
            updates,
        ))
    }

    pub fn shutdown(&self) {
        self.transport.shutdown();
    }
}

/// One ACP session: its id, and the transport it lives on.
///
/// Its update stream is not here — see [`AcpClient::new_session`] for why — and
/// neither is a lifecycle beyond `close`, because a session is opened, prompted
/// and closed, and the transport is what knows how to do all three.
pub struct Session {
    id: String,
    scope: Scope,
    timeout: Duration,
    transport: Arc<Transport>,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn prompt(&self, text: &str) -> Result<Value, AcpError> {
        self.transport
            .request(
                &self.scope,
                "session/prompt",
                serde_json::json!({
                    "sessionId": self.id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
                self.timeout,
            )
            .await
    }

    /// Closes the session. Best-effort by design: a session that cannot be
    /// closed is not worth failing a completed turn over, and the transport
    /// dropping the connection reaps it anyway.
    pub async fn close(&self) {
        let result = self
            .transport
            .request(
                &self.scope,
                "session/close",
                serde_json::json!({ "sessionId": self.id }),
                Duration::from_secs(10),
            )
            .await;
        if let Err(err) = result {
            tracing::warn!(session_id = %self.id, %err, "could not close the ACP session");
        }
        self.transport.unsubscribe(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> Config {
        let mut config = Config::default();
        config.goose.defaults.allowed_roots = vec![root.to_path_buf()];
        config.goose.defaults.cwd = root.to_path_buf();
        config
    }

    /// `secretEnv` names a variable, and this is the lookup that turns the name
    /// into the key. It is tested because it was *missing*: the field parsed and
    /// nothing read it, so no request ever carried `X-Secret-Key` and a
    /// key-protected goose answered 401 to every turn (mac-studio, 2026-09-16).
    #[test]
    fn the_named_variable_is_what_is_read_not_the_name_itself() {
        // `PATH` rather than a variable this test invents: a process that got as
        // far as running cargo has one, it is not a secret, and reading it needs
        // no `set_var` — which is unsafe in edition 2024 and would race every
        // other test in this binary.
        let path = std::env::var("PATH").expect("a process running cargo has a PATH");
        let named = AcpConfig {
            secret_env: "PATH".to_string(),
            ..AcpConfig::default()
        };
        assert_eq!(
            secret_key(&named).as_deref(),
            Some(path.as_str()),
            "the value behind the name is the key, never the name"
        );

        // The name is trimmed, because it comes from a YAML file a human edits.
        let padded = AcpConfig {
            secret_env: "  PATH  ".to_string(),
            ..AcpConfig::default()
        };
        assert_eq!(
            secret_key(&padded).as_deref(),
            Some(path.as_str()),
            "surrounding whitespace is not part of a variable's name"
        );
    }

    #[test]
    fn a_name_that_resolves_to_nothing_is_no_key() {
        // Naming a blank variable, or one that is not set, is how a host says
        // "goose here is unauthenticated". It is not a reason to refuse to
        // start: whether goose wants a key is goose's answer to give, and a 401
        // names the header and settles it faster than this process guessing.
        let blank = AcpConfig {
            secret_env: "   ".to_string(),
            ..AcpConfig::default()
        };
        assert_eq!(secret_key(&blank), None, "whitespace names no variable");

        let unset = AcpConfig {
            secret_env: "A2A_GOOSE_SURELY_NOT_SET_9f3a".to_string(),
            ..AcpConfig::default()
        };
        assert_eq!(secret_key(&unset), None, "an unset variable is no key");
    }

    /// A unique scratch directory, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("a2a-goose-cwd-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_inside_an_allowed_root_is_used_as_given() {
        let scratch = Scratch::new("inside");
        let inside = scratch.0.join("project");
        std::fs::create_dir(&inside).expect("project dir");

        let resolved =
            resolve_cwd(&config(&scratch.0), Some(&inside.display().to_string())).expect("allowed");
        assert_eq!(resolved, inside.canonicalize().expect("canonical"));
    }

    #[test]
    fn an_omitted_cwd_falls_back_to_the_configured_default() {
        let scratch = Scratch::new("default");
        let resolved = resolve_cwd(&config(&scratch.0), None).expect("default is allowed");
        assert_eq!(resolved, scratch.0.canonicalize().expect("canonical"));
    }

    #[test]
    fn a_directory_outside_every_allowed_root_is_refused_and_the_roots_are_named() {
        // The security boundary: `cwd` is how a caller chooses what a turn can
        // see, so it must not be possible to point it at the filesystem.
        let scratch = Scratch::new("outside");
        let err = resolve_cwd(&config(&scratch.0), Some("/etc")).expect_err("refused");
        // The refusal names the directory that was *checked*, which is the
        // resolved one: symlinks are followed before the prefix check, so on a
        // host where /etc is a symlink (macOS: -> /private/etc) the caller is
        // told where it actually landed rather than where it typed. Comparing
        // against the literal "/etc" passed on Linux and failed on the macOS
        // agent, which is a test bug, not a behaviour difference.
        let outside = PathBuf::from("/etc").canonicalize().expect("/etc exists");
        match &err {
            CwdError::OutsideRoots { cwd, roots } => {
                assert_eq!(cwd, &outside);
                assert_eq!(roots.len(), 1, "the refusal should say what is allowed");
            }
            other => panic!("expected an outside-roots refusal, got {other:?}"),
        }
        assert!(err.to_string().contains("/etc"), "{err}");
    }

    #[test]
    fn a_symlink_out_of_an_allowed_root_does_not_leave_it() {
        let scratch = Scratch::new("symlink");
        let escape = scratch.0.join("escape");
        std::os::unix::fs::symlink("/etc", &escape).expect("symlink");

        let err = resolve_cwd(&config(&scratch.0), Some(&escape.display().to_string()))
            .expect_err("a symlink must be resolved before the prefix check");
        assert!(matches!(err, CwdError::OutsideRoots { .. }), "{err}");
    }

    #[test]
    fn a_directory_that_does_not_exist_is_refused_by_name() {
        let scratch = Scratch::new("missing");
        let missing = scratch.0.join("no-such-dir");
        let err = resolve_cwd(&config(&scratch.0), Some(&missing.display().to_string()))
            .expect_err("refused");
        assert!(matches!(err, CwdError::NotFound { .. }), "{err}");
        assert!(err.to_string().contains("no-such-dir"), "{err}");
    }

    #[test]
    fn a_file_is_not_a_working_directory() {
        let scratch = Scratch::new("file");
        let file = scratch.0.join("README");
        std::fs::write(&file, "not a directory").expect("write");
        assert!(matches!(
            resolve_cwd(&config(&scratch.0), Some(&file.display().to_string())),
            Err(CwdError::NotFound { .. })
        ));
    }

    #[test]
    fn the_default_cwd_is_checked_too_rather_than_trusted() {
        // A host whose configured default sits outside its own allowlist is a
        // broken host, and it should say so rather than quietly run turns there.
        let scratch = Scratch::new("mistrusted");
        let mut config = config(&scratch.0);
        config.goose.defaults.cwd = PathBuf::from("/etc");
        assert!(matches!(
            resolve_cwd(&config, None),
            Err(CwdError::OutsideRoots { .. })
        ));
    }
}
