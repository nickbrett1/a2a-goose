//! The A2A executor: the one module that knows both protocols.
//!
//! The division of labour is the thing to keep in mind when reading this file.
//! The executor decides **what a message means** — which skill, which directory,
//! which prompt — and **what the caller sees** — the A2A frames. It does not know
//! how a turn is run: that is [`crate::turn::Turns`], implemented for real over
//! ACP in [`crate::acp::turns`] and by a fake in the tests below. So the mapping
//! from a turn's events to A2A frames is tested without a `goose serve`, and the
//! ACP hop is tested without an A2A caller.
//!
//! Skill resolution is real, and it is a refusal rather than a fallback: an
//! unknown `metadata.skillId` is an error naming what does exist (constraint #11).
//! A silent fallback would destroy the routing contract callers freeze, and it is
//! exactly the kind of mistake that only shows up as "the agent did the wrong
//! thing sometimes".

use std::collections::HashMap;
use std::sync::Arc;

use a2a::{
    A2AError, Artifact, Message, Part, Role, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent, error_code, methods,
};
use a2a_server::{AgentExecutor, ExecutorContext};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::acp::resolve_cwd;
use crate::config::Config;
use crate::skills::{Dispatch, SkillSet};
use crate::turn::{Limit, TurnError, TurnEvent, TurnRequest, Turns, Usage, prompt_for, wall_clock};

/// The request metadata bag, as the SDK hands it to an executor.
pub type Metadata = HashMap<String, Value>;

/// The metadata key a caller uses to select a skill (§5.1).
pub const SKILL_ID_KEY: &str = "skillId";

/// The metadata key a caller uses to select a working directory (§5.1).
pub const CWD_KEY: &str = "cwd";

/// The artifact id the answer streams under. One artifact per task, appended to
/// as the turn runs: a caller that wants the whole answer takes the last chunk,
/// and a caller that wants the text as it arrives does not have to reassemble it
/// from numbered pieces.
pub const ANSWER_ARTIFACT_ID: &str = "answer";

/// A resolved turn: which skill, and how it will be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub skill_id: String,
    pub dispatch: Dispatch,
}

/// Resolves `metadata.skillId` against the catalogue.
///
/// Omitted → the default (`ask`). **Unknown → an error, never a fallback**
/// (constraint #11): a silent fallback destroys the routing contract, and the
/// caller gets a 400 that lists what does exist.
pub fn resolve_skill(skills: &SkillSet, metadata: Option<&Metadata>) -> Result<Resolved, A2AError> {
    let requested = metadata
        .and_then(|meta| meta.get(SKILL_ID_KEY))
        .and_then(Value::as_str);

    match requested {
        None => Ok(resolved(skills.default_skill())),
        Some(id) => skills.get(id).map(resolved).ok_or_else(|| {
            A2AError::invalid_params(format!(
                "unknown skillId {id:?}; this agent offers: {}",
                skills.ids().join(", ")
            ))
        }),
    }
}

fn resolved(skill: &crate::skills::Skill) -> Resolved {
    Resolved {
        skill_id: skill.id.clone(),
        dispatch: skill.dispatch.clone(),
    }
}

/// The M1 executor: resolve, run, translate.
#[derive(Clone)]
pub struct GooseExecutor {
    skills: Arc<SkillSet>,
    config: Arc<Config>,
    turns: Arc<dyn Turns>,
}

impl GooseExecutor {
    pub fn new(skills: Arc<SkillSet>, config: Arc<Config>, turns: Arc<dyn Turns>) -> Self {
        Self {
            skills,
            config,
            turns,
        }
    }

    /// What the executor does about `metadata.skillId`, exposed so the routing
    /// contract can be asserted without a turn behind it.
    pub fn resolve(&self, metadata: Option<&Metadata>) -> Result<Resolved, A2AError> {
        resolve_skill(&self.skills, metadata)
    }

    /// Everything that can be decided before the turn starts.
    ///
    /// Deliberately before: a refused directory or an unrunnable skill must not
    /// open a session, because a session is a process on the host holding a
    /// model's worth of context, and paying for one to discover an invalid
    /// request is paying the wrong party.
    fn prepare(
        &self,
        resolved: &Resolved,
        metadata: Option<&Metadata>,
        text: &str,
    ) -> Result<TurnRequest, TurnError> {
        let requested_cwd = metadata
            .and_then(|meta| meta.get(CWD_KEY))
            .and_then(Value::as_str);
        let cwd = resolve_cwd(&self.config, requested_cwd)?;
        let prompt = prompt_for(&resolved.skill_id, &resolved.dispatch, text)?;
        Ok(TurnRequest {
            // Filled in by `execute`, which is where the A2A context is known.
            context: None,
            cwd,
            prompt,
            skill: resolved.skill_id.clone(),
            wall_clock: wall_clock(&self.config),
        })
    }
}

impl AgentExecutor for GooseExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let metadata = ctx.metadata.clone();
        let text = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap_or_default()
            .to_string();
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

        let request = self.resolve(metadata.as_ref()).and_then(|resolved| {
            self.prepare(&resolved, metadata.as_ref(), &text)
                .map_err(refusal_to_a2a)
        });

        let mut request = match request {
            Ok(request) => request,
            // Nothing has been claimed to the caller yet, so a refusal is the
            // JSON-RPC error the SDK turns into a non-2xx-coded reply rather
            // than a task that starts and then fails.
            Err(err) => return Box::pin(futures::stream::once(async move { Err(err) })),
        };

        // An empty context id is the SDK's "no context", and treating it as a
        // key would pool every contextless turn together — the opposite of what
        // it means.
        request.context = Some(context_id.clone()).filter(|id| !id.is_empty());

        let events = self.turns.run(request);
        let state = TurnState::new(
            task_id,
            context_id,
            self.config.registry.limits.max_tokens_per_context,
        );

        // The opening status is emitted before the first event so that a caller
        // watching a slow turn sees `working` immediately rather than silence.
        let opening_frame = state.opening_frame();
        let opening = futures::stream::once(async move { Ok::<_, A2AError>(opening_frame) });

        let body = futures::stream::unfold((events, state), |(mut events, mut state)| async move {
            loop {
                if state.done {
                    return None;
                }
                match events.next().await {
                    None => return None,
                    // A turn-level failure arrives as an error frame. It is
                    // already past the point where a JSON-RPC error is
                    // expressible, so it becomes a `failed` task: visible, and
                    // terminal, which is what the caller needs.
                    Some(Err(err)) => {
                        state.done = true;
                        return Some((
                            vec![Ok(state.failed_frame(&err.to_string()))],
                            (events, state),
                        ));
                    }
                    Some(Ok(event)) => {
                        let frames = state.apply(event);
                        if frames.is_empty() {
                            continue;
                        }
                        return Some((frames, (events, state)));
                    }
                }
            }
        })
        .flat_map(futures::stream::iter);

        Box::pin(opening.chain(body))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        // The session is closed by the turn's own task when its event stream is
        // dropped, so cancellation is a status transition here and a session
        // reaping there. Anything more would be a second owner of a session.
        let task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(futures::stream::once(async move {
            Ok(StreamResponse::Task(task))
        }))
    }
}

/// The frame-by-frame state of one turn.
///
/// It exists because the mapping is stateful in two ways that matter: the answer
/// is *one* artifact appended to rather than a series of unrelated ones, and the
/// token bound is a running maximum rather than a per-frame value.
struct TurnState {
    task_id: String,
    context_id: String,
    /// `registry.limits.maxTokensPerContext`.
    max_tokens: u64,
    /// The largest `usage_update.used` seen: goose reports the size of the
    /// conversation it is holding, so the bound is on the context, not a sum.
    peak_context: u64,
    usage: Usage,
    done: bool,
    /// Emitted once, before the status that ends the turn.
    answered: bool,
}

impl TurnState {
    fn new(task_id: String, context_id: String, max_tokens: u64) -> Self {
        Self {
            task_id,
            context_id,
            max_tokens,
            peak_context: 0,
            usage: Usage::default(),
            done: false,
            answered: false,
        }
    }

    fn opening_frame(&self) -> StreamResponse {
        self.status(TaskState::Working, None, None)
    }

    fn apply(&mut self, event: TurnEvent) -> Vec<Result<StreamResponse, A2AError>> {
        match event {
            TurnEvent::Text(delta) => {
                // An empty delta is not an answer chunk; emitting one would add
                // a frame per token for no content.
                if delta.is_empty() {
                    return Vec::new();
                }
                self.answered = true;
                vec![Ok(self.artifact_frame(delta, false))]
            }
            TurnEvent::ContextUsage(context) => {
                self.peak_context = self.peak_context.max(context.used);
                if self.peak_context <= self.max_tokens {
                    return Vec::new();
                }
                self.done = true;
                let err = TurnError::Limit {
                    limit: Limit::Tokens,
                    allowed: self.max_tokens,
                    observed: self.peak_context,
                };
                vec![Ok(self.failed_frame(&err.to_string()))]
            }
            TurnEvent::Finished { stop_reason, usage } => {
                self.usage = usage;
                self.done = true;
                let (state, message) = terminal_state(&stop_reason);
                let mut frames = Vec::with_capacity(2);
                if self.answered {
                    // Closes the streamed artifact, so a caller that only reads
                    // to the end still learns the answer is complete.
                    frames.push(Ok(self.artifact_frame(String::new(), true)));
                }
                frames.push(Ok(self.status(
                    state,
                    message,
                    Some(self.usage_metadata(&stop_reason)),
                )));
                frames
            }
        }
    }

    fn artifact_frame(&self, text: String, last_chunk: bool) -> StreamResponse {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(Part::text(text));
        }
        StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            artifact: Artifact {
                artifact_id: ANSWER_ARTIFACT_ID.to_string(),
                name: Some(ANSWER_ARTIFACT_ID.to_string()),
                description: None,
                parts,
                metadata: None,
                extensions: None,
            },
            // Append rather than replace: the artifact *is* the answer so far.
            append: Some(true),
            last_chunk: Some(last_chunk),
            metadata: None,
        })
    }

    fn failed_frame(&self, message: &str) -> StreamResponse {
        self.status(
            TaskState::Failed,
            Some(message.to_string()),
            Some(json!({ "usage": self.usage_json() })),
        )
    }

    fn status(
        &self,
        state: TaskState,
        message: Option<String>,
        metadata: Option<Value>,
    ) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            status: TaskStatus {
                state,
                message: message.map(|text| Message::new(Role::Agent, vec![Part::text(text)])),
                timestamp: Some(chrono::Utc::now()),
            },
            metadata: metadata.and_then(as_map),
        })
    }

    fn usage_metadata(&self, stop_reason: &str) -> Value {
        json!({
            "stopReason": stop_reason,
            "usage": self.usage_json(),
            "contextTokens": self.peak_context,
        })
    }

    fn usage_json(&self) -> Value {
        json!({
            "totalTokens": self.usage.total,
            "inputTokens": self.usage.input,
            "outputTokens": self.usage.output,
        })
    }
}

/// What goose's `stopReason` means for a task.
///
/// Only `end_turn` is completion. Every other reason — `max_tokens`, a refusal,
/// a reason this build has never seen — leaves the answer unfinished, and saying
/// so is the whole point: a truncated answer delivered as `completed` is a lie
/// the caller cannot detect.
fn terminal_state(stop_reason: &str) -> (TaskState, Option<String>) {
    if stop_reason == crate::acp::turns::STOP_END_TURN {
        return (TaskState::Completed, None);
    }
    (
        TaskState::Failed,
        Some(format!(
            "goose stopped the turn with stopReason {stop_reason:?}, so the answer is not a \
             finished one"
        )),
    )
}

/// A refusal *before* the turn starts, as a JSON-RPC error.
///
/// A bad `cwd` or an unrunnable skill is the caller's to fix, so it is
/// `INVALID_PARAMS`; anything about the ACP hop is this host's problem and is an
/// internal error. The distinction is what lets a caller tell "ask again
/// differently" from "retry later".
fn refusal_to_a2a(err: TurnError) -> A2AError {
    match err {
        TurnError::Cwd(_) | TurnError::Unsupported { .. } => {
            A2AError::invalid_params(err.to_string())
        }
        TurnError::Transport(_) | TurnError::Limit { .. } => A2AError::internal(err.to_string()),
    }
}

fn as_map(value: Value) -> Option<Metadata> {
    match value {
        Value::Object(map) => Some(map.into_iter().collect()),
        _ => None,
    }
}

/// The JSON-RPC method name list, used by `/status` and by the contract tests.
///
/// Named here rather than inlined so a caller reading `/status` sees the same
/// strings the SDK dispatches on — an `a2a-rs` bump that renames a method is a
/// failing test, not a silently unreachable route (constraint #18).
pub const ADVERTISED_METHODS: [&str; 4] = [
    methods::SEND_MESSAGE,
    methods::SEND_STREAMING_MESSAGE,
    methods::GET_TASK,
    methods::CANCEL_TASK,
];

/// Every JSON-RPC error code this module can raise, pinned by name so the
/// mapping in §5.4 is greppable.
pub const UNKNOWN_SKILL_CODE: i32 = error_code::INVALID_PARAMS;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::config::{Recipes, Skills};
    use crate::skills::Skill;

    fn skills() -> SkillSet {
        SkillSet::load(&Skills {
            default: "ask".to_string(),
            recipes: Recipes::default(),
            d: PathBuf::from("/definitely/not/a/skills/dir"),
            overrides: Default::default(),
        })
        .expect("skills")
    }

    /// A scratch directory, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "a2a-goose-exec-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
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

    fn config(root: &Path) -> Config {
        let mut config = Config::default();
        config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
        config.goose.defaults.cwd = root.to_path_buf();
        config.goose.defaults.allowed_roots = vec![root.to_path_buf()];
        config
    }

    /// A `Turns` that produces whatever the test says, and records what it was
    /// asked to run.
    #[derive(Default)]
    struct FakeTurns {
        events: Vec<Result<TurnEvent, TurnError>>,
        requests: Mutex<Vec<TurnRequest>>,
    }

    impl FakeTurns {
        fn answering(events: Vec<TurnEvent>) -> Self {
            Self {
                events: events.into_iter().map(Ok).collect(),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn refusing(err: TurnError) -> Self {
            Self {
                events: vec![Err(err)],
                requests: Mutex::new(Vec::new()),
            }
        }

        fn ran(&self) -> Vec<TurnRequest> {
            self.requests.lock().expect("lock").clone()
        }
    }

    impl Turns for FakeTurns {
        fn run(&self, request: TurnRequest) -> BoxStream<'static, Result<TurnEvent, TurnError>> {
            self.requests.lock().expect("lock").push(request);
            Box::pin(futures::stream::iter(self.events.clone()))
        }
    }

    fn context(metadata: Option<Metadata>, text: &str) -> ExecutorContext {
        ExecutorContext {
            message: Some(Message::new(Role::User, vec![Part::text(text)])),
            task_id: "task-1".to_string(),
            stored_task: None,
            context_id: "ctx-1".to_string(),
            metadata,
            user: None,
            service_params: ServiceParams::new(),
            tenant: None,
        }
    }

    use a2a_server::middleware::ServiceParams;

    async fn frames(
        executor: &GooseExecutor,
        ctx: ExecutorContext,
    ) -> Vec<Result<StreamResponse, A2AError>> {
        executor.execute(ctx).collect().await
    }

    #[tokio::test]
    async fn a_turn_streams_the_answer_as_one_appended_artifact_then_completes() {
        let scratch = Scratch::new("answer");
        let turns = Arc::new(FakeTurns::answering(vec![
            TurnEvent::Text("he".to_string()),
            TurnEvent::Text("llo".to_string()),
            TurnEvent::Finished {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    total: 42,
                    input: 40,
                    output: 2,
                },
            },
        ]));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let frames = frames(&executor, context(None, "hi")).await;
        let frames: Vec<StreamResponse> =
            frames.into_iter().map(|f| f.expect("no error")).collect();

        // working, chunk, chunk, close-artifact, completed.
        assert_eq!(frames.len(), 5);
        match &frames[0] {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Working);
                assert_eq!(update.task_id, "task-1");
            }
            other => panic!("expected the opening status, got {other:?}"),
        }

        let chunks: Vec<&TaskArtifactUpdateEvent> = frames[1..=2]
            .iter()
            .map(|frame| match frame {
                StreamResponse::ArtifactUpdate(update) => update,
                other => panic!("expected artifact updates, got {other:?}"),
            })
            .collect();
        assert_eq!(chunks[0].artifact.parts[0].as_text(), Some("he"));
        assert_eq!(chunks[1].artifact.parts[0].as_text(), Some("llo"));
        assert_eq!(
            chunks[0].artifact.artifact_id, chunks[1].artifact.artifact_id,
            "one artifact, appended to - not one artifact per chunk"
        );
        assert_eq!(chunks[0].append, Some(true));
        assert_eq!(chunks[0].last_chunk, Some(false));

        let closing = match &frames[3] {
            StreamResponse::ArtifactUpdate(update) => update,
            other => panic!("expected the closing artifact update, got {other:?}"),
        };
        assert_eq!(closing.last_chunk, Some(true));

        match &frames[4] {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Completed);
                assert!(
                    update.status.message.is_none(),
                    "the answer is the artifact"
                );
                let metadata = update.metadata.as_ref().expect("usage rides the status");
                assert_eq!(metadata["usage"]["totalTokens"], 42);
                assert_eq!(metadata["stopReason"], "end_turn");
            }
            other => panic!("expected the terminal status, got {other:?}"),
        }

        // And what the turn was actually asked to do.
        let ran = turns.ran();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].prompt, "hi", "an ask skill passes the text through");
        assert_eq!(ran[0].cwd, scratch.0.canonicalize().expect("canonical"));
    }

    #[tokio::test]
    async fn an_unknown_skill_is_refused_before_a_turn_is_ever_run() {
        let scratch = Scratch::new("unknown-skill");
        let turns = Arc::new(FakeTurns::answering(vec![TurnEvent::Text(
            "nope".to_string(),
        )]));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let metadata = Metadata::from([("skillId".to_string(), json!("code-review"))]);
        let frames = frames(&executor, context(Some(metadata), "hi")).await;
        assert_eq!(frames.len(), 1);
        let err = frames[0].as_ref().expect_err("refused");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("code-review"), "{}", err.message);
        assert!(turns.ran().is_empty(), "no session is paid for a refusal");
    }

    #[tokio::test]
    async fn a_directory_outside_the_allowed_roots_is_refused_before_a_turn_is_ever_run() {
        let scratch = Scratch::new("outside");
        let turns = Arc::new(FakeTurns::answering(vec![TurnEvent::Text(
            "nope".to_string(),
        )]));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let metadata = Metadata::from([("cwd".to_string(), json!("/etc"))]);
        let frames = frames(&executor, context(Some(metadata), "hi")).await;
        let err = frames[0].as_ref().expect_err("refused");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("/etc"), "{}", err.message);
        assert!(turns.ran().is_empty());
    }

    /// A catalogue with one declared skill, so a dispatch that is neither `ask`
    /// nor a recipe can be selected by id.
    fn skills_with_a_declared_instruction(root: &Path) -> SkillSet {
        let dir = root.join("skills.d");
        std::fs::create_dir_all(&dir).expect("skills.d");
        std::fs::write(
            dir.join("code-review.yaml"),
            "id: code-review\nname: Code review\ndescription: Review a diff\n\
             instruction: |\n  Review the current diff.\n",
        )
        .expect("write skill");
        SkillSet::load(&Skills {
            default: "ask".to_string(),
            recipes: Recipes::default(),
            d: dir,
            overrides: Default::default(),
        })
        .expect("skills")
    }

    #[tokio::test]
    async fn a_declared_instruction_skill_is_sent_as_a_preamble_to_the_callers_text() {
        let scratch = Scratch::new("instruction");
        let turns = Arc::new(FakeTurns::answering(vec![TurnEvent::Finished {
            stop_reason: "end_turn".to_string(),
            usage: Usage::default(),
        }]));
        let executor = GooseExecutor::new(
            Arc::new(skills_with_a_declared_instruction(&scratch.0)),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let metadata = Metadata::from([("skillId".to_string(), json!("code-review"))]);
        let _ = frames(&executor, context(Some(metadata), "diff --git a/x b/x")).await;

        let ran = turns.ran();
        assert_eq!(ran.len(), 1);
        assert_eq!(
            ran[0].prompt, "Review the current diff.\n\ndiff --git a/x b/x",
            "the instruction comes first and the caller's text last"
        );
    }

    #[tokio::test]
    async fn the_token_ceiling_fails_the_task_and_stops_reading() {
        let scratch = Scratch::new("tokens");
        let mut config = config(&scratch.0);
        config.registry.limits.max_tokens_per_context = 1_000;

        let turns = Arc::new(FakeTurns::answering(vec![
            TurnEvent::ContextUsage(crate::turn::ContextUsage {
                used: 5_000,
                size: 200_000,
            }),
            TurnEvent::Text("this must never be delivered".to_string()),
        ]));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let frames = frames(&executor, context(None, "hi")).await;
        let frames: Vec<StreamResponse> =
            frames.into_iter().map(|f| f.expect("no error")).collect();
        assert_eq!(frames.len(), 2, "working, then the bound");
        match &frames[1] {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Failed);
                let message = update
                    .status
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .expect("the failure names itself");
                assert!(message.contains("maxTokensPerContext"), "{message}");
                assert!(
                    message.contains("1000") && message.contains("5000"),
                    "{message}"
                );
            }
            other => panic!("expected the terminal status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stop_reason_that_is_not_end_turn_is_a_failed_task() {
        let scratch = Scratch::new("max-tokens");
        let turns = Arc::new(FakeTurns::answering(vec![
            TurnEvent::Text("a truncated ans".to_string()),
            TurnEvent::Finished {
                stop_reason: "max_tokens".to_string(),
                usage: Usage::default(),
            },
        ]));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let frames: Vec<StreamResponse> = frames(&executor, context(None, "hi"))
            .await
            .into_iter()
            .map(|f| f.expect("no error"))
            .collect();
        match frames.last().expect("a terminal frame") {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Failed);
                let message = update
                    .status
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .expect("the reason is named");
                assert!(message.contains("max_tokens"), "{message}");
            }
            other => panic!("expected the terminal status, got {other:?}"),
        }
        // The truncated answer is still delivered - hiding it would be worse.
        assert!(matches!(frames[1], StreamResponse::ArtifactUpdate(_)));
    }

    #[tokio::test]
    async fn a_transport_failure_mid_turn_becomes_a_failed_task_not_a_silent_end() {
        let scratch = Scratch::new("transport");
        let turns = Arc::new(FakeTurns::refusing(TurnError::Transport(
            "goose's ACP transport is gone".to_string(),
        )));
        let executor = GooseExecutor::new(
            Arc::new(skills()),
            Arc::new(config(&scratch.0)),
            Arc::clone(&turns) as Arc<dyn Turns>,
        );

        let frames: Vec<StreamResponse> = frames(&executor, context(None, "hi"))
            .await
            .into_iter()
            .map(|f| f.expect("a turn failure is a status, not a transport error"))
            .collect();
        match frames.last().expect("a terminal frame") {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Failed);
                let message = update
                    .status
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .expect("the reason is named");
                assert!(message.contains("gone"), "{message}");
            }
            other => panic!("expected the terminal status, got {other:?}"),
        }
    }

    #[test]
    fn an_omitted_skill_id_resolves_to_ask() {
        let resolved = resolve_skill(&skills(), None).expect("default");
        assert_eq!(resolved.skill_id, "ask");
        assert_eq!(resolved.dispatch, Dispatch::Ask);
    }

    fn metadata(pairs: &[(&str, Value)]) -> Metadata {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn an_unknown_skill_id_is_an_error_that_lists_what_exists() {
        let metadata = metadata(&[("skillId", json!("code-review"))]);
        let err = resolve_skill(&skills(), Some(&metadata)).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("code-review"), "{}", err.message);
        assert!(err.message.contains("ask"), "{}", err.message);
    }

    #[test]
    fn a_non_string_skill_id_is_treated_as_omitted_not_as_a_match() {
        let metadata = metadata(&[("skillId", json!(7))]);
        let resolved = resolve_skill(&skills(), Some(&metadata)).expect("default");
        assert_eq!(resolved.skill_id, "ask");
    }

    #[test]
    fn the_advertised_methods_are_the_sdk_s_own_names() {
        assert_eq!(ADVERTISED_METHODS[0], "SendMessage");
        assert_eq!(ADVERTISED_METHODS[1], "SendStreamingMessage");
        assert_eq!(UNKNOWN_SKILL_CODE, error_code::INVALID_PARAMS);
    }

    #[test]
    fn the_skill_type_is_reachable_from_these_tests() {
        // Keeps the import honest: `Skill` is what `resolve_skill` reads.
        let set = skills();
        let _: &Skill = set.default_skill();
    }
}
