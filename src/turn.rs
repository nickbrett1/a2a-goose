//! What a turn *is*, with neither wire in sight.
//!
//! [`crate::executor`] speaks A2A and [`crate::acp`] speaks ACP; this module is
//! the shape that crosses between them, and it exists so that the crossing is
//! testable without a `goose serve`. The executor resolves a skill and a
//! directory, builds a [`TurnRequest`], and hands it to a [`Turns`]; what comes
//! back is a stream of [`TurnEvent`]s that the executor turns into A2A frames.
//! A test can therefore substitute a fake `Turns` and assert the *mapping* — one
//! place where a guess would be invisible in production.
//!
//! The vocabulary is deliberately goose's, not A2A's. Anything A2A cannot express
//! (context-window readings, goose's `stopReason`) stays visible here and is
//! translated at the edge, rather than being flattened into an A2A shape that
//! would silently drop it.

use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::acp::CwdError;
use crate::skills::Dispatch;

/// Token accounting as goose reports it on a finished turn
/// (`result.usage.{totalTokens,inputTokens,outputTokens}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub total: u64,
    pub input: u64,
    pub output: u64,
}

/// The context-window reading goose emits *during* a turn
/// (`usage_update`: `used` of `size`). Not a spend figure — it is the size of the
/// conversation goose is holding, which is exactly what a loop bound should
/// count (the S2 decision: bound the loop, not the wallet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
}

/// Which agent-enforced bound stopped a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// `registry.limits.maxWallClockSecondsPerTask`.
    WallClock,
    /// `registry.limits.maxTokensPerContext`.
    Tokens,
}

/// Why a turn could not run, or could not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    /// The requested working directory is not one this host will run in.
    Cwd(CwdError),
    /// The selected skill dispatches by a mechanism this build does not have.
    /// A refusal rather than a substitution: the routing contract says an
    /// unrunnable request is an error, never a different turn (constraint #11).
    Unsupported { skill_id: String, reason: String },
    /// Everything else about the ACP hop, already rendered for a human.
    Transport(String),
    /// A bound, named, with what it allowed and what it saw.
    Limit {
        limit: Limit,
        allowed: u64,
        observed: u64,
    },
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cwd(err) => write!(f, "{err}"),
            Self::Unsupported { skill_id, reason } => {
                write!(f, "skill {skill_id:?} cannot run on this build: {reason}")
            }
            Self::Transport(message) => write!(f, "{message}"),
            Self::Limit {
                limit,
                allowed,
                observed,
            } => {
                let (what, unit) = match limit {
                    Limit::WallClock => ("registry.limits.maxWallClockSecondsPerTask", "seconds"),
                    Limit::Tokens => ("registry.limits.maxTokensPerContext", "tokens"),
                };
                write!(
                    f,
                    "the turn exceeded {what} ({allowed} {unit}); it was stopped at {observed} \
                     {unit}. This is an agent-side bound, not a provider quota"
                )
            }
        }
    }
}

impl std::error::Error for TurnError {}

impl From<CwdError> for TurnError {
    fn from(err: CwdError) -> Self {
        Self::Cwd(err)
    }
}

/// One thing that happened during a turn, in goose's vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// A delta of the answer. Deltas, not the whole answer: the caller sees the
    /// turn as it happens, which is the point of streaming.
    Text(String),
    /// A context-window reading, for the token bound.
    ContextUsage(ContextUsage),
    /// The turn ended. `stop_reason` is goose's own string, unmapped — the
    /// decision about what it means for a task's state is made at the A2A edge,
    /// in one place, where it can be read.
    Finished { stop_reason: String, usage: Usage },
}

/// A turn, ready to run.
///
/// `cwd` is already validated: [`crate::acp::resolve_cwd`] has canonicalised it
/// and checked it against the host's allowlist, so a [`Turns`] implementation
/// never sees a directory the host would refuse.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    /// The A2A `contextId` this turn belongs to, when the caller sent one.
    ///
    /// Carried rather than interpreted: the A2A layer knows nothing about
    /// sessions, and the ACP layer is the only thing that can decide whether a
    /// given context may reuse one. Today every turn still gets a fresh
    /// session; this is what a reuse policy would key on.
    pub context: Option<String>,
    pub cwd: PathBuf,
    pub prompt: String,
    /// The skill this turn runs under.
    ///
    /// Carried for the same reason `context` is, and no more: the control
    /// surface reports which skill a held session last ran, and the executor is
    /// the only place that knows. It is **not** part of a session's identity
    /// (§6.3) — reusing a context under another skill is legal and must not
    /// start a second session — so this is display data that the pool carries
    /// and never routes on.
    pub skill: String,
    /// The ceiling for the whole turn. Enforced where the waiting actually
    /// happens — inside the ACP implementation — because a bound checked only
    /// between events would never fire on a turn that has gone quiet.
    pub wall_clock: Duration,
}

/// The connection a [`Turns`] keeps, as `/status` reports it.
///
/// Synchronous on purpose: `/status` is a question a human asks when something
/// is wrong, so it must never be the thing that blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnHealth {
    /// This build has no way to run a turn (a test double, or nothing wired).
    Unavailable,
    /// Wired, but no connection to `goose serve` has been needed yet.
    Idle,
    /// A live connection; the next turn will reuse it.
    Connected,
}

/// One session being held for reuse, as `GET /sessions` reports it.
///
/// A *snapshot value*, not a handle. The pool can move under a reader — a turn
/// can start, a TTL can expire — so the listing is taken in one go and rendered
/// from copies. Holding anything live across the render is the one thing a
/// control route must never do, because `/sessions` is what an operator asks
/// when a turn will not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The A2A `contextId` the session is held for.
    pub context_id: String,
    /// goose's own id for it — what a `session/close` or a goose log names.
    pub session_id: String,
    /// The directory it is rooted at. Part of the session's *identity*, not
    /// decoration: a turn asking for another directory is never handed this one.
    pub cwd: PathBuf,
    /// The skill its **last** turn ran under. See [`TurnRequest::skill`]: this
    /// is what ran most recently, not what the session *is*.
    pub skill_id: String,
    /// Seconds since it was last used. The number that says whether the idle TTL
    /// is about to take it.
    pub idle_secs: u64,
}

/// What `DELETE /sessions/{contextId}` found.
///
/// Three outcomes rather than a boolean, because the caller does something
/// different for each: nothing (closed), `tasks/cancel` (busy), or nothing at
/// all (absent) — and collapsing "busy" into "absent" would tell a caller their
/// conversation was gone when it was mid-sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionClose {
    /// There was a session held for this context, and it is now closed.
    Closed { session_id: String },
    /// A turn is running for this context, so its session is checked out and is
    /// not this route's to close. The caller has a `taskId`; `tasks/cancel` is
    /// how a running turn is stopped. Closing it here would race the turn's own
    /// teardown for the same session, which is exactly the second-owner problem
    /// the executor's `cancel` refuses to create.
    Busy,
    /// Nothing is held for this context: it never had a session, or it was
    /// already closed. Not an error — a retried `DELETE` must not fail the
    /// second time for having been right the first time (the shape S5 recorded
    /// for the registry: `200` then `404`).
    Absent,
}

impl Default for SessionClose {
    /// `Absent`, and it is not arbitrary: it is the same answer the trait's own
    /// [`Turns::close_session`] default gives, so a runner that holds no
    /// sessions has one meaning for "nothing here" rather than two.
    fn default() -> Self {
        Self::Absent
    }
}

/// Runs turns. Implemented for real over ACP in [`crate::acp::turns`], and by a
/// fake in the executor's tests.
pub trait Turns: Send + Sync + 'static {
    fn run(&self, request: TurnRequest) -> BoxStream<'static, Result<TurnEvent, TurnError>>;

    /// See [`TurnHealth`]. Defaults to `Unavailable`, which is the honest answer
    /// for anything that is not a real ACP client.
    fn health(&self) -> TurnHealth {
        TurnHealth::Unavailable
    }

    /// How many turns are running right now.
    fn in_flight(&self) -> usize {
        0
    }

    /// How many sessions are being held for reuse.
    ///
    /// Only meaningful next to [`Self::in_flight`]: the two numbers together say
    /// whether a context's session is being kept (the reuse policy working) or
    /// whether every turn is starting over (an operator's "why does my agent
    /// have no memory" question).
    fn retained(&self) -> usize {
        0
    }

    /// The sessions being held for reuse, for `GET /sessions`.
    ///
    /// Synchronous, like [`Self::health`] and [`Self::retained`]: it is a
    /// snapshot of in-memory state, and a control route that had to await a
    /// lock would be the thing that hangs when a turn is stuck. The default is
    /// empty, which is the honest answer for anything that holds no sessions.
    fn sessions(&self) -> Vec<SessionInfo> {
        Vec::new()
    }

    /// Closes the session held for `context` and forgets it, for
    /// `DELETE /sessions/{contextId}`.
    ///
    /// A boxed future rather than an `async fn` because this trait is used as
    /// `Arc<dyn Turns>`: an injected lifetime would make it non-object-safe.
    /// The future is `'static` — the implementation clones what it needs out of
    /// `&self` before returning — so the handler can hold it across an await.
    fn close_session(&self, _context: String) -> BoxFuture<'static, SessionClose> {
        Box::pin(async { SessionClose::Absent })
    }
}

/// How long a turn may take, from `registry.limits.maxWallClockSecondsPerTask`.
pub fn wall_clock(config: &crate::config::Config) -> Duration {
    Duration::from_secs(config.registry.limits.max_wall_clock_seconds_per_task)
}

/// A [`Turns`] that runs nothing, and says so.
///
/// It exists so that `/status` and the wire tests can be built without a
/// `goose serve`, and so the "nothing is wired" case is a *value* rather than a
/// missing field: the executor, the router and `/status` all behave the same way
/// with it as with any other runner, which is what makes it useful in a test.
pub struct NoTurns;

impl Turns for NoTurns {
    fn run(&self, _request: TurnRequest) -> BoxStream<'static, Result<TurnEvent, TurnError>> {
        Box::pin(futures::stream::once(async {
            Err(TurnError::Transport(
                "this process has no ACP turn runner wired in, so no turn can run".to_string(),
            ))
        }))
    }
}

/// What a skill says to goose, given what the caller said.
///
/// The caller's text is always last, so an instruction reads as a preamble to the
/// payload rather than the other way round.
///
/// A recipe is **refused** rather than approximated. Handing goose a prompt that
/// mentions a recipe file would run *something*, and the caller would have no way
/// to tell that from the recipe; S10 left open how a recipe is invoked over ACP
/// (its `parameters` are not expressible in a `session/prompt`), so the honest
/// answer until that is settled is an error that names the skill.
pub fn prompt_for(skill_id: &str, dispatch: &Dispatch, text: &str) -> Result<String, TurnError> {
    match dispatch {
        Dispatch::Ask => Ok(text.to_string()),
        Dispatch::Instruction(instruction) => {
            let instruction = instruction.trim_end();
            if text.trim().is_empty() {
                Ok(instruction.to_string())
            } else {
                Ok(format!("{instruction}\n\n{text}"))
            }
        }
        Dispatch::Recipe(path) => Err(TurnError::Unsupported {
            skill_id: skill_id.to_string(),
            reason: format!(
                "recipe dispatch ({}) has no ACP mechanism yet: a recipe's parameters are not \
                 expressible in a session/prompt, and S10 left the invocation open",
                path.display()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_skill_says_exactly_what_the_caller_said() {
        let prompt = prompt_for("ask", &Dispatch::Ask, "what is in this repo?").expect("prompt");
        assert_eq!(prompt, "what is in this repo?");
    }

    #[test]
    fn an_instruction_precedes_the_callers_text() {
        let dispatch = Dispatch::Instruction("Review the diff.\n".to_string());
        let prompt = prompt_for("code-review", &dispatch, "diff --git a/x b/x").expect("prompt");
        assert_eq!(prompt, "Review the diff.\n\ndiff --git a/x b/x");
    }

    #[test]
    fn an_instruction_with_no_caller_text_does_not_leave_a_dangling_gap() {
        let dispatch = Dispatch::Instruction("Say hello.\n".to_string());
        assert_eq!(
            prompt_for("hello", &dispatch, "  ").expect("prompt"),
            "Say hello."
        );
    }

    #[test]
    fn a_recipe_is_refused_by_name_rather_than_approximated() {
        let dispatch = Dispatch::Recipe(PathBuf::from("/recipes/scaffold-project.yaml"));
        let err = prompt_for("scaffold-project", &dispatch, "go").unwrap_err();
        match &err {
            TurnError::Unsupported { skill_id, reason } => {
                assert_eq!(skill_id, "scaffold-project");
                assert!(reason.contains("scaffold-project.yaml"), "{reason}");
            }
            other => panic!("expected an unsupported-dispatch refusal, got {other:?}"),
        }
        assert!(err.to_string().contains("scaffold-project"), "{err}");
    }

    #[test]
    fn a_bound_names_the_setting_it_came_from_and_both_numbers() {
        let err = TurnError::Limit {
            limit: Limit::Tokens,
            allowed: 400_000,
            observed: 412_345,
        };
        let text = err.to_string();
        assert!(text.contains("maxTokensPerContext"), "{text}");
        assert!(text.contains("400000") && text.contains("412345"), "{text}");

        let err = TurnError::Limit {
            limit: Limit::WallClock,
            allowed: 900,
            observed: 901,
        };
        assert!(
            err.to_string().contains("maxWallClockSecondsPerTask"),
            "{err}"
        );
    }
}
