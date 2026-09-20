//! The activity feed: what this agent is *doing*, for an operator watching.
//!
//! This is deliberately **not** [`crate::turn::TurnEvent`]. A `TurnEvent` is what
//! a **caller** sees, and it is shaped by what A2A can carry — an answer delta, a
//! context reading, a stop reason. An [`ActivityEvent`] is what an **operator**
//! sees, and it is shaped by what a human wants to know: a request arrived, a
//! session was reused, a tool was called, the answer is streaming, the turn
//! finished and here is what it cost. The two overlap (a turn both answers a
//! caller and is watched) but they are not the same stream, and collapsing them
//! would either put operator detail on the wire to callers or lose it entirely.
//!
//! Three properties make this safe to have on by default:
//!
//! - **It never blocks a turn.** Recording goes to a bounded ring buffer and a
//!   `broadcast` channel; a send with no receivers is a no-op and a slow
//!   subscriber is dropped frames, never a stalled turn. The turn's own path
//!   (`crate::acp::turns`) calls [`TurnObserver::record`], which cannot fail.
//! - **It is bounded.** The ring buffer holds the last [`ActivityHub::backlog_cap`]
//!   events and nothing is written to disk, so a process that runs for a month
//!   does not grow. It is a *view*, not a record: goose's own `sessions.db` is
//!   the record of a conversation (hard constraint #1).
//! - **It is not on the wire by default.** The only route that serves it is
//!   `GET /events` (`crate::server`), which is behind the bearer token because it
//!   names working directories and carries answer text — the same reason
//!   `GET /sessions` is.
//!
//! The vocabulary here is small and additive on purpose: a viewer can ignore
//! event types it does not know, and adding one does not change any other.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;

/// How many past events a new subscriber is handed before it is live.
///
/// Big enough to answer "what just happened?" for a viewer that attaches after
/// the fact, small enough that the buffer is never the reason a process holds
/// memory. A configured `observability.activity.backlog` overrides it, clamped by
/// [`ActivityHub::new`].
pub const DEFAULT_BACKLOG: usize = 512;

/// The largest backlog an operator may configure. A viewer is not a record; a
/// number large enough to be one is a number large enough to be a leak.
pub const MAX_BACKLOG: usize = 10_000;

/// How many events may be in flight to each subscriber before it is considered
/// too slow and its oldest frames are dropped. Frames dropped are *frames*, not
/// the turn: see the module comment.
const BROADCAST_CAPACITY: usize = 1024;

/// One thing that happened, as an operator would tell it.
///
/// Serialised with a `type` discriminator in `snake_case` and camelCase fields
/// (`rename_all_fields`), matching [`Activity`]'s envelope, so `/events` is
/// `jq`-able and a viewer can switch on `type` without a schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ActivityEvent {
    /// A request was accepted and resolved, before any session exists. The
    /// prompt itself is *not* carried — only its size — because a viewer that
    /// wants the content reads the answer, and a control surface that mirrors
    /// every prompt is a control surface that leaks every prompt.
    RequestReceived { cwd: String, prompt_bytes: usize },

    /// A request was refused before it ran: an unknown skill, a `cwd` outside
    /// the allowlist, a recipe with no ACP mechanism. The reason is the same
    /// string the caller was given, so the viewer and the caller agree.
    Refused { reason: String },

    /// A turn has a session and is about to prompt goose. `reused` says whether
    /// this is the context's own session (a conversation continuing) or a fresh
    /// one (a cold start, or a throwaway while the context is busy).
    TurnStarted { session_id: String, reused: bool },

    /// goose began a tool call. `tool_kind` is goose's own category (read, edit,
    /// execute, …) and `title` its human label; both are optional because the
    /// first frame of a call may carry no more than an id.
    ToolCall {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// An in-progress or finished tool call changed state. Kept distinct from
    /// [`Self::ToolCall`] so a viewer does not render one call twice.
    ToolCallUpdate {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// A reasoning step the model emitted. `text` is the thought chunk as goose
    /// sent it.
    Thought { text: String },

    /// goose published a plan. `entries` is how many steps it named — the plan
    /// body is deliberately not mirrored here.
    Plan { entries: usize },

    /// Answer text arrived. Byte count only: the content is the caller's, and a
    /// viewer showing a live conversation reads it from the turn's own artifact
    /// stream or from goose's record, not from a size that would tempt us to
    /// duplicate every token into the control plane.
    Answer { delta_bytes: usize },

    /// A context-window reading mid-turn, exactly as `usage_update` reported it.
    Usage { used: u64, size: u64 },

    /// The turn ended, with what it cost. `stop_reason` is goose's own string,
    /// unmapped — the same value the caller's task metadata carries.
    Finished {
        stop_reason: String,
        total_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        context_tokens: u64,
    },

    /// The turn failed. `error` is already rendered for a human.
    Failed { error: String },

    /// The ACP hop changed state: connected, or the connection was lost and the
    /// next turn will reconnect (which starts a fresh session per context).
    Connection { event: String },
}

/// An [`ActivityEvent`] with the identity of the thing it happened to, and a
/// total order.
///
/// `seq` is monotonic across the process and is the ordering key a viewer uses:
/// events from concurrent turns interleave, and arrival order is not turn order.
/// [`ActivityHub::subscribe`] documents why a consumer must still guard on it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Monotonic, process-wide, gaps only when a subscriber lagged.
    pub seq: u64,
    /// When this process recorded it. UTC, RFC 3339.
    pub at: DateTime<Utc>,
    /// The A2A context this belongs to, when the caller named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// The A2A task, when there is one. Disambiguates two turns in one context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// goose's session, once one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The skill this turn ran under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(flatten)]
    pub event: ActivityEvent,
}

/// The feed: a bounded ring buffer for late subscribers, and a broadcast for live
/// ones.
///
/// Cheap to share (`Arc`) and cheap to write to. Every method that mutates takes
/// `&self` and never blocks a turn: the lock is over a `VecDeque` of small
/// clones, held for a push and a pop, never across an await.
pub struct ActivityHub {
    enabled: bool,
    seq: AtomicU64,
    backlog_cap: usize,
    backlog: std::sync::Mutex<VecDeque<Activity>>,
    sender: broadcast::Sender<Activity>,
}

impl ActivityHub {
    /// `backlog` is clamped to `1..=MAX_BACKLOG`: a viewer asked for zero history
    /// still wants the live stream, and a viewer asked for a million events is
    /// asking for a leak.
    pub fn new(enabled: bool, backlog: usize) -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            enabled,
            seq: AtomicU64::new(0),
            backlog_cap: backlog.clamp(1, MAX_BACKLOG),
            backlog: std::sync::Mutex::new(VecDeque::new()),
            sender,
        }
    }

    /// A hub that records nothing. The honest default for a unit test that is
    /// not about activity, and for `/events` when an operator turned it off.
    pub fn disabled() -> Self {
        Self::new(false, DEFAULT_BACKLOG)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn backlog_cap(&self) -> usize {
        self.backlog_cap
    }

    /// How many subscribers are attached right now. `/status` reports it so a
    /// viewer can see whether its own connection is live.
    pub fn subscribers(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Records one event. Returns immediately; a full turn never waits here.
    ///
    /// Takes `&self`, so every call site borrows rather than clones the hub —
    /// which is what lets [`TurnObserver`] stay a cheap value.
    pub fn record(
        &self,
        context_id: Option<&str>,
        task_id: Option<&str>,
        session_id: Option<&str>,
        skill: Option<&str>,
        event: ActivityEvent,
    ) {
        if !self.enabled {
            return;
        }
        let activity = Activity {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            at: Utc::now(),
            context_id: context_id.map(str::to_string),
            task_id: task_id.map(str::to_string),
            session_id: session_id.map(str::to_string),
            skill: skill.map(str::to_string),
            event,
        };
        self.push_backlog(activity.clone());
        // A send with no receivers is the ordinary case (nobody watching), and
        // is not an error: there is nothing to do about it and nothing to say.
        let _ = self.sender.send(activity);
    }

    fn push_backlog(&self, activity: Activity) {
        let mut backlog = match self.backlog.lock() {
            Ok(backlog) => backlog,
            // A panic while holding this is somebody else's bug; losing the
            // backlog is not a reason to panic a turn.
            Err(poisoned) => poisoned.into_inner(),
        };
        while backlog.len() >= self.backlog_cap {
            backlog.pop_front();
        }
        backlog.push_back(activity);
    }

    /// A snapshot of recent events, newest last. For a one-shot read; a viewer
    /// that wants to follow uses [`Self::subscribe`].
    pub fn recent(&self) -> Vec<Activity> {
        match self.backlog.lock() {
            Ok(backlog) => backlog.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }

    /// Everything a subscriber needs: the backlog to replay, and the receiver to
    /// follow.
    ///
    /// **The receiver is subscribed *before* the backlog is read**, so an event
    /// recorded in between appears in both — a duplicate, never a gap. That is
    /// the right failure: a consumer drops the duplicate by its `seq` (which is
    /// what `seq` is for), and losing an event silently would be worse than
    /// showing one twice. A consumer must therefore skip any live event whose
    /// `seq` it has already seen.
    pub fn subscribe(&self) -> (Vec<Activity>, broadcast::Receiver<Activity>) {
        let receiver = self.sender.subscribe();
        (self.recent(), receiver)
    }
}

impl Default for ActivityHub {
    fn default() -> Self {
        Self::new(true, DEFAULT_BACKLOG)
    }
}

/// A handle that stamps every event with one turn's identity.
///
/// A turn is begun in one place (the executor, which knows the task) and run in
/// another (the ACP runner, which knows the session), so the identity has to be
/// carried across. This is that carrier: build it once where the request is
/// understood, and every subsequent `record` is attributed without repeating four
/// `Option`s at each call site.
///
/// A disabled observer (no hub) makes `record` a no-op, so a code path can record
/// unconditionally and a test need not install a hub to exercise it.
#[derive(Clone, Default)]
pub struct TurnObserver {
    hub: Option<Arc<ActivityHub>>,
    context_id: Option<String>,
    task_id: Option<String>,
    skill: Option<String>,
    session_id: Option<String>,
}

impl TurnObserver {
    pub fn new(
        hub: Option<Arc<ActivityHub>>,
        context_id: Option<String>,
        task_id: Option<String>,
        skill: Option<String>,
    ) -> Self {
        Self {
            hub,
            context_id,
            task_id,
            skill,
            session_id: None,
        }
    }

    /// The same observer with the session named. Called once the session exists.
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// The same observer with the resolved skill named. Called once routing has
    /// decided, which is after the request's *requested* skill is known.
    pub fn with_skill(mut self, skill: impl Into<String>) -> Self {
        self.skill = Some(skill.into());
        self
    }

    pub fn record(&self, event: ActivityEvent) {
        if let Some(hub) = &self.hub {
            hub.record(
                self.context_id.as_deref(),
                self.task_id.as_deref(),
                self.session_id.as_deref(),
                self.skill.as_deref(),
                event,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> ActivityHub {
        ActivityHub::new(true, 4)
    }

    #[test]
    fn seq_is_monotonic_and_the_backlog_is_bounded() {
        let hub = hub();
        for i in 0..10 {
            hub.record(
                None,
                None,
                None,
                None,
                ActivityEvent::Answer { delta_bytes: i },
            );
        }
        let recent = hub.recent();
        // Capped at `backlog_cap`, and it is the *newest* events that survive.
        assert_eq!(recent.len(), 4);
        assert_eq!(recent.first().map(|a| a.seq), Some(6));
        assert_eq!(recent.last().map(|a| a.seq), Some(9));
        let seqs: Vec<u64> = recent.iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![6, 7, 8, 9]);
    }

    #[test]
    fn a_disabled_hub_records_nothing() {
        let hub = ActivityHub::disabled();
        hub.record(
            None,
            None,
            None,
            None,
            ActivityEvent::Answer { delta_bytes: 1 },
        );
        assert!(hub.recent().is_empty());
        assert_eq!(hub.subscribers(), 0);
        assert!(!hub.enabled());
    }

    #[test]
    fn the_backlog_is_clamped_to_something_that_cannot_be_a_leak() {
        assert_eq!(ActivityHub::new(true, 0).backlog_cap(), 1);
        assert_eq!(
            ActivityHub::new(true, usize::MAX).backlog_cap(),
            MAX_BACKLOG
        );
    }

    #[tokio::test]
    async fn a_subscriber_gets_the_backlog_then_live_events_with_no_gap() {
        let hub = hub();
        hub.record(
            None,
            None,
            None,
            None,
            ActivityEvent::Answer { delta_bytes: 1 },
        );
        hub.record(
            None,
            None,
            None,
            None,
            ActivityEvent::Answer { delta_bytes: 2 },
        );

        let (backlog, mut receiver) = hub.subscribe();
        assert_eq!(backlog.len(), 2);
        let last_replayed = backlog.last().unwrap().seq;

        hub.record(
            None,
            None,
            None,
            None,
            ActivityEvent::Answer { delta_bytes: 3 },
        );

        // Live events are the ones with a seq past the backlog: the contract the
        // route relies on to drop the duplicate at the seam.
        let mut live = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            live.push(event);
        }
        let fresh: Vec<u64> = live
            .iter()
            .map(|a| a.seq)
            .filter(|seq| *seq > last_replayed)
            .collect();
        assert_eq!(fresh, vec![2]);
    }

    #[test]
    fn the_observer_stamps_identity_and_a_disabled_one_is_silent() {
        let hub = Arc::new(hub());
        let observer = TurnObserver::new(
            Some(Arc::clone(&hub)),
            Some("ctx".to_string()),
            Some("task".to_string()),
            Some("ask".to_string()),
        )
        .with_session("sess");

        observer.record(ActivityEvent::TurnStarted {
            session_id: "sess".to_string(),
            reused: false,
        });

        let recorded = hub.recent();
        assert_eq!(recorded.len(), 1);
        let activity = &recorded[0];
        assert_eq!(activity.context_id.as_deref(), Some("ctx"));
        assert_eq!(activity.task_id.as_deref(), Some("task"));
        assert_eq!(activity.session_id.as_deref(), Some("sess"));
        assert_eq!(activity.skill.as_deref(), Some("ask"));

        // No hub: records nothing, and does not panic.
        TurnObserver::default().record(ActivityEvent::Refused {
            reason: "x".to_string(),
        });
    }
}
