//! `goose` is a host prerequisite that is **verified, never installed**.
//!
//! Hard constraint #8: every host already has goose, with its own MCP config and
//! its own recipes, and those recipes are the skill library this project mines.
//! Installing a second copy — or starting degraded without one — would fork the
//! configuration we depend on. So the agent's first act is to find goose and
//! refuse to run if it is missing or too old.
//!
//! `scripts/check-goose.sh` enforces the same policy for the deploy units and
//! for a human about to start the agent by hand. The duplication is deliberate:
//! the script can run before there is a binary, and the binary cannot depend on
//! a repository checkout that is not part of the release payload.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

/// The environment variable that overrides which `goose` to use.
pub const GOOSE_BIN_ENV: &str = "GOOSE_BIN";

/// The name looked up on `PATH` when `GOOSE_BIN` is unset.
pub const DEFAULT_GOOSE_BIN: &str = "goose";

/// The oldest `goose` whose ACP surface this project is built against.
///
/// Kept in step with `MIN_GOOSE_VERSION` in `scripts/check-goose.sh`.
pub const MIN_GOOSE_VERSION: Version = Version::new(1, 50, 0);

/// A dotted numeric version, as `goose --version` prints it.
///
/// Deliberately not `semver`: this only ever compares goose's own version
/// string, and a pre-release or build suffix must not make it unparsable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Reads the first dotted numeric token out of a `--version` line.
    ///
    /// goose prints a bare `1.50.0` today, but `goose 1.50.0` is just as
    /// likely from a future release, so scan tokens rather than assuming a
    /// position.
    pub fn parse(output: &str) -> Option<Self> {
        output
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|token| {
                token.contains('.') && token.split('.').all(|part| part.parse::<u64>().is_ok())
            })
            .and_then(|token| {
                let mut parts = token.split('.').map(|part| part.parse::<u64>().ok());
                Some(Self {
                    major: parts.next()??,
                    minor: parts.next().flatten().unwrap_or(0),
                    patch: parts.next().flatten().unwrap_or(0),
                })
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The goose installation this process will talk to.
#[derive(Debug, Clone)]
pub struct Goose {
    pub path: PathBuf,
    pub version: Version,
}

#[derive(Debug, thiserror::Error)]
pub enum GooseError {
    #[error(
        "`{bin}` was not found on PATH. goose must be installed for this host user \
         (this project verifies goose, it never installs it - hard constraint #8), \
         or point {env} at it."
    )]
    NotFound { bin: String, env: &'static str },

    #[error("could not run `{path} --version`: {source}")]
    VersionFailed {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("`{path} --version` printed something with no version in it: {output:?}")]
    UnparsableVersion { path: PathBuf, output: String },

    #[error(
        "goose {found} at {path} is older than the required {required}; upgrade goose on this \
         host (this project will not install it for you)"
    )]
    TooOld {
        path: PathBuf,
        found: Version,
        required: Version,
    },
}

impl Goose {
    /// Locates and checks `goose`, honouring `GOOSE_BIN`.
    pub fn verify() -> Result<Self, GooseError> {
        let bin = std::env::var(GOOSE_BIN_ENV).unwrap_or_else(|_| DEFAULT_GOOSE_BIN.to_string());
        Self::verify_bin(&bin)
    }

    /// Locates and checks a named `goose` binary.
    ///
    /// Resolution is delegated to the platform's own `PATH` lookup: the binary
    /// has to be something an init unit could exec, not a shell alias, and
    /// spawning it is the only way to be sure it runs.
    pub fn verify_bin(bin: &str) -> Result<Self, GooseError> {
        let path = which(bin).ok_or_else(|| GooseError::NotFound {
            bin: bin.to_string(),
            env: GOOSE_BIN_ENV,
        })?;

        let output = Command::new(&path)
            .arg("--version")
            .output()
            .map_err(|source| GooseError::VersionFailed {
                path: path.clone(),
                source,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let version = Version::parse(&stdout)
            .or_else(|| Version::parse(&stderr))
            .ok_or_else(|| GooseError::UnparsableVersion {
                path: path.clone(),
                output: format!("{}{}", stdout.trim(), stderr.trim()),
            })?;

        if version < MIN_GOOSE_VERSION {
            return Err(GooseError::TooOld {
                path,
                found: version,
                required: MIN_GOOSE_VERSION,
            });
        }

        Ok(Self { path, version })
    }
}

/// The `PATH` lookup, spelled out rather than shelling out to `which(1)`.
///
/// A binary only counts if it is an executable *file*: a directory on `PATH`, or
/// a non-executable name, is not something this process can spawn.
fn which(bin: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(bin);
    if candidate.components().count() > 1 {
        return is_executable(&candidate).then_some(candidate);
    }

    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_version() {
        assert_eq!(Version::parse("1.50.0\n"), Some(Version::new(1, 50, 0)));
    }

    #[test]
    fn parses_a_named_version() {
        assert_eq!(Version::parse("goose 1.50.2"), Some(Version::new(1, 50, 2)));
    }

    #[test]
    fn pads_a_short_version() {
        assert_eq!(Version::parse("2.1"), Some(Version::new(2, 1, 0)));
    }

    #[test]
    fn rejects_output_with_no_version() {
        assert_eq!(Version::parse("goose: command failed\n"), None);
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn orders_by_field_not_lexically() {
        assert!(Version::new(1, 50, 0) > Version::new(1, 9, 0));
        assert!(Version::new(1, 50, 1) > Version::new(1, 50, 0));
        assert_eq!(Version::new(1, 50, 0), Version::new(1, 50, 0));
    }

    #[test]
    fn a_missing_binary_is_not_found() {
        let err = Goose::verify_bin("definitely-not-goose-6f3a").unwrap_err();
        assert!(matches!(err, GooseError::NotFound { .. }), "{err:?}");
    }

    /// `ETXTBSY`, the errno behind the retry in [`verify_fake`].
    const ETXTBSY: i32 = 26;

    /// How many times [`verify_fake`] will retry a busy executable.
    const RETRIES: u32 = 50;

    /// Writes a stand-in `goose` that prints `output`, so the failure paths are
    /// tested against a real spawn rather than a real goose's real version.
    fn fake_goose(name: &str, output: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("a2a-goose-test-{name}-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create fake goose");
        // A script rather than a binary: the point is what `--version` prints.
        writeln!(file, "#!/bin/sh").expect("write shebang");
        writeln!(file, "echo '{output}'").expect("write body");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake goose");
        path
    }

    /// [`Goose::verify_bin`], retrying the one failure that is not the behaviour
    /// under test.
    ///
    /// `ETXTBSY` ("Text file busy") here is a race with the *other* tests in this
    /// binary, not with anything this crate does. Rust opens the script
    /// `O_CLOEXEC`, but that flag only takes effect at `execve`: while some other
    /// test's `Command` is between `fork` and `exec`, its child holds a writable
    /// descriptor to this freshly written script, and the kernel refuses to exec
    /// it. The window is microseconds and it is why this looked like a 1-in-20
    /// flake. Production cannot hit it — `goose` is a binary that has existed on
    /// disk for a long time — so the fix belongs in the test.
    fn verify_fake(path: &std::path::Path) -> Result<Goose, GooseError> {
        let bin = path.to_str().expect("a UTF-8 path");
        for attempt in 0..RETRIES {
            match Goose::verify_bin(bin) {
                Err(GooseError::VersionFailed { source, .. })
                    if source.raw_os_error() == Some(ETXTBSY) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(
                        5 * u64::from(attempt + 1),
                    ));
                }
                outcome => return outcome,
            }
        }
        Goose::verify_bin(bin)
    }

    #[test]
    fn an_unparsable_binary_names_its_output() {
        let path = fake_goose("unparsable", "goose: something went wrong");
        let err = verify_fake(&path).unwrap_err();
        assert!(
            matches!(err, GooseError::UnparsableVersion { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("something went wrong"), "{err}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_older_goose_is_refused_with_both_versions() {
        let path = fake_goose("old", "goose 1.49.9");
        let err = verify_fake(&path).unwrap_err();
        let GooseError::TooOld {
            found, required, ..
        } = err
        else {
            panic!("expected TooOld, got {err:?}");
        };
        assert_eq!(found, Version::new(1, 49, 9));
        assert_eq!(required, MIN_GOOSE_VERSION);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_recent_enough_goose_is_accepted() {
        let path = fake_goose("current", "1.50.0");
        let goose = verify_fake(&path).expect("verify fake goose");
        assert_eq!(goose.version, MIN_GOOSE_VERSION);
        let _ = std::fs::remove_file(path);
    }
}
