//! The A2A executor: the one module that knows both protocols.
//!
//! In M1 it is a **stub**, and it is a stub on purpose: everything downstream of
//! the wire — the card, the skill catalogue, registration, the control surface —
//! can be built and tested against an executor that returns a canned task, and
//! LiteLLM can be made to list the agent (and S13 can run) before a single byte
//! of ACP exists. What it must *not* be is a stub that skips the contract: skill
//! resolution is already real here, including the 400 on an unknown
//! `metadata.skillId`, because that rule is a routing guarantee callers depend
//! on (constraint #11) and a stub that falls back to `ask` would teach the tests
//! the wrong behaviour.
//!
//! M2 replaces the canned task with `session/new` → `session/prompt` over
//! [`crate::acp`]; the resolution logic below is the part that survives.

use std::sync::Arc;

use a2a::{
    A2AError, Message, Part, StreamResponse, Task, TaskState, TaskStatus, error_code, methods,
};
use a2a_server::{AgentExecutor, ExecutorContext};
use futures::stream::BoxStream;

use crate::skills::{Dispatch, SkillSet};

/// The request metadata bag, as the SDK hands it to an executor.
pub type Metadata = std::collections::HashMap<String, serde_json::Value>;

/// The metadata key a caller uses to select a skill (§5.1).
pub const SKILL_ID_KEY: &str = "skillId";

/// The metadata key a caller uses to select a working directory (§5.1).
pub const CWD_KEY: &str = "cwd";

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
        .and_then(serde_json::Value::as_str);

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

/// The M1 executor. Returns one canned task per message.
#[derive(Clone)]
pub struct StubExecutor {
    skills: Arc<SkillSet>,
}

impl StubExecutor {
    pub fn new(skills: Arc<SkillSet>) -> Self {
        Self { skills }
    }

    /// What the stub does about `metadata.skillId`, so M1's tests can assert the
    /// routing contract without an ACP server behind it.
    pub fn resolve(&self, metadata: Option<&Metadata>) -> Result<Resolved, A2AError> {
        resolve_skill(&self.skills, metadata)
    }
}

impl AgentExecutor for StubExecutor {
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

        let result = self.resolve(metadata.as_ref()).map(|resolved| {
            // The stub's "answer" says what it would have done, which is exactly
            // what makes it a useful round-trip: the caller can see the skill
            // that was selected, the dispatch kind, and the directory it would
            // have run in — none of which comes back out of a real turn this
            // cheaply.
            let cwd = metadata
                .as_ref()
                .and_then(|meta| meta.get(CWD_KEY))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<goose default>");
            let dispatch = match &resolved.dispatch {
                Dispatch::Ask => "ask".to_string(),
                Dispatch::Instruction(_) => "instruction".to_string(),
                // The path is deliberately not echoed: the answer is what a
                // caller sees, and a host's directory layout is not theirs.
                Dispatch::Recipe(_) => "recipe".to_string(),
            };
            format!(
                "stub: skill={} dispatch={dispatch} cwd={cwd} message={text}",
                resolved.skill_id
            )
        });

        let event = match result {
            Ok(answer) => {
                let task = Task {
                    id: task_id,
                    context_id,
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: Some(Message::new(a2a::Role::Agent, vec![Part::text(answer)])),
                        timestamp: Some(chrono::Utc::now()),
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                };
                Ok(StreamResponse::Task(task))
            }
            Err(err) => Err(err),
        };

        Box::pin(futures::stream::once(async move { event }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
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
    use std::path::PathBuf;

    use crate::config::{Recipes, Skills};

    fn skills() -> SkillSet {
        SkillSet::load(&Skills {
            default: "ask".to_string(),
            recipes: Recipes::default(),
            d: PathBuf::from("/definitely/not/a/skills/dir"),
            overrides: Default::default(),
        })
        .expect("skills")
    }

    #[test]
    fn an_omitted_skill_id_resolves_to_ask() {
        let resolved = resolve_skill(&skills(), None).expect("default");
        assert_eq!(resolved.skill_id, "ask");
        assert_eq!(resolved.dispatch, Dispatch::Ask);
    }

    fn metadata(pairs: &[(&str, serde_json::Value)]) -> Metadata {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn an_unknown_skill_id_is_an_error_that_lists_what_exists() {
        let metadata = metadata(&[("skillId", serde_json::json!("code-review"))]);
        let err = resolve_skill(&skills(), Some(&metadata)).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("code-review"), "{}", err.message);
        assert!(err.message.contains("ask"), "{}", err.message);
    }

    #[test]
    fn a_non_string_skill_id_is_treated_as_omitted_not_as_a_match() {
        let metadata = metadata(&[("skillId", serde_json::json!(7))]);
        let resolved = resolve_skill(&skills(), Some(&metadata)).expect("default");
        assert_eq!(resolved.skill_id, "ask");
    }

    #[test]
    fn the_advertised_methods_are_the_sdk_s_own_names() {
        assert_eq!(ADVERTISED_METHODS[0], "SendMessage");
        assert_eq!(ADVERTISED_METHODS[1], "SendStreamingMessage");
        assert_eq!(UNKNOWN_SKILL_CODE, error_code::INVALID_PARAMS);
    }
}
