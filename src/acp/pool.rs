//! The policy for reusing one goose session across a context's turns.
//!
//! One A2A `contextId` is one conversation, and a conversation lives in a goose
//! session. M1 opened a session per turn and closed it when the turn ended,
//! which is correct and amnesiac: a caller's second message in the same context
//! had no memory of the first, because the session holding it was already gone.
//! This module is the missing half — a `contextId` → session map with rules.
//! The rules are the point of it, so they are stated here rather than inferred
//! from the code below.
//!
//! **1. A context owns at most one session, and a turn either owns it or finds
//! it busy.** A turn whose context has an idle session rooted at the same
//! directory takes it *out* of the pool and holds it for the turn, so the pool
//! has nothing to hand a second turn that arrives meanwhile. That second turn
//! does not queue: a turn may legitimately run for the whole wall-clock bound
//! (900s by default), so queueing behind one turns "concurrent" into "hung"
//! while the caller's own timeout expires. It does not share either — two
//! interleaved `session/prompt`s on one session are not a conversation, they are
//! a race. It gets a fresh session of its own, and that throwaway does *not*
//! become the context's session when it finishes: whoever arrived first owns the
//! memory. [`Acquired::Fresh::vacant`] is how the caller can tell the two apart.
//!
//! **2. A session is evicted for exactly three reasons, and eviction always
//! means "closed", never "forgotten".** The context's `cwd` changing (a session
//! is rooted where it was opened, so handing it to a turn for another directory
//! would run the caller's turn somewhere they did not ask for — the boundary
//! constraint #4 exists to hold); the idle TTL (a conversation nobody has
//! touched for an hour is not one that is about to be continued); and the pool
//! cap, which drops the least recently used session when a new one arrives.
//! Reaping is lazy — it happens when the pool is asked for a session, which is
//! the next turn — because there is no background sweeper, and adding one would
//! mean a task that owns the pool's lifetime for a host that is idle anyway. An
//! idle host therefore keeps its sessions until it serves one more turn; an idle
//! session is cheap and cannot be mistaken for a busy one.
//!
//! **3. A session's life is bounded by the connection that made it.** A session
//! handed out here is only meaningful on the connection it was opened on, so
//! the pool is *owned by* that connection in [`crate::acp::turns`]: losing the
//! connection loses the pool with it, in one operation, rather than leaving
//! sessions that the next turn would be handed and every one of which would
//! fail.
//!
//! Two things this module deliberately does not do.
//!
//! It does no I/O: sessions it drops are *returned* to the caller, which is what
//! lets the whole policy be tested without a `goose serve` (`S` is a `Retained`
//! in production and a string in the tests below).
//!
//! And it does not persist. `goose.sessions.dbPath` names the file a durable map
//! would live in, and [S6](../../spikes/S6.md) records that the deployed goose
//! advertises `loadSession`, which is what would make the map survive a restart
//! of this process — but no spike has actually driven a resumed session and
//! checked that goose answers with the conversation rather than a fresh one, and
//! a map that points at sessions nobody has proved are reattachable is worse
//! than no map. So the map lives in memory and a restart loses each context's
//! *label*; the conversation itself is in goose's own `sessions.db` either way
//! (hard constraint #1).
//!
//! [S6]: ../../spikes/S6.md

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One idle session, with what the policy needs to decide whether it may be
/// handed out again.
pub struct Idle<S> {
    /// The session itself. Generic so the policy can be exercised without a
    /// `goose serve` behind it, and so this module cannot accidentally reach
    /// into a session to do something the policy did not say.
    pub session: S,
    /// The directory the session was opened in. Part of the session's
    /// *identity*, not just of the request that created it: see rule 2.
    pub cwd: PathBuf,
    /// When it was last handed out. Drives both of the time-related rules.
    pub last_used: Instant,
}

/// What the pool had to say about a turn that named a context.
pub enum Acquired<S> {
    /// An idle session for this context, rooted where the turn asked to run. It
    /// is checked out — the pool no longer holds it — which is also the whole
    /// busy rule: a second turn for this context now sees nothing to take.
    Reuse(Idle<S>),
    /// No session. Either this context has never had one, or one is running
    /// right now.
    Fresh {
        /// Whether this turn is the context's *first*, and so the one whose
        /// session should become the context's session when the turn ends. See
        /// rule 1: a turn that arrives second runs a throwaway and gives it up.
        vacant: bool,
    },
}

/// The idle sessions, and which contexts currently have a turn running.
///
/// Not `Default`-constructed anywhere but [`crate::acp::turns`], which owns one
/// per connection.
pub struct Pool<S> {
    idle: HashMap<String, Idle<S>>,
    /// Contexts with a turn in flight. Kept alongside the sessions rather than
    /// inferred from them, because a session that is *checked out* looks exactly
    /// like a session that never existed, and the two want different answers
    /// (rule 1).
    claimed: HashSet<String>,
}

impl<S> Default for Pool<S> {
    fn default() -> Self {
        Self {
            idle: HashMap::new(),
            claimed: HashSet::new(),
        }
    }
}

impl<S> Pool<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims `context` for a turn and hands over its idle session, if there is
    /// one that may be reused.
    ///
    /// Everything the caller must close — sessions evicted by the TTL, or by
    /// this context having moved to another directory — comes back in the second
    /// half of the pair, because closing is I/O and this module does none.
    pub fn acquire(
        &mut self,
        context: &str,
        cwd: &Path,
        now: Instant,
        ttl: Duration,
    ) -> (Acquired<S>, Vec<Idle<S>>) {
        // Reap first, so a session that is too old to be used is not handed out
        // on the turn that notices.
        let mut dropped = self.reap(now, ttl);
        let vacant = self.claimed.insert(context.to_string());

        match self.idle.remove(context) {
            Some(idle) if idle.cwd == cwd => (Acquired::Reuse(idle), dropped),
            Some(idle) => {
                // Rooted somewhere else: not reusable for this turn, and not
                // worth keeping, because a context gets one session and this
                // turn is about to become it.
                dropped.push(idle);
                (Acquired::Fresh { vacant }, dropped)
            }
            None => (Acquired::Fresh { vacant }, dropped),
        }
    }

    /// Puts a finished session back as the context's own, evicting down to the
    /// cap. Returns whatever the pool dropped to make room, for the caller to
    /// close.
    pub fn retain(&mut self, context: &str, idle: Idle<S>, cap: usize) -> Vec<Idle<S>> {
        let mut dropped = Vec::new();
        if let Some(previous) = self.idle.insert(context.to_string(), idle) {
            // Belt and braces: `acquire` takes the context's session out before
            // any turn can run, so this should not happen. If it ever does, two
            // sessions for one context is worse than losing the older one.
            dropped.push(previous);
        }
        while self.idle.len() > cap {
            let Some(oldest) = self.oldest() else {
                break;
            };
            if let Some(idle) = self.idle.remove(&oldest) {
                dropped.push(idle);
            }
        }
        dropped
    }

    /// Releases the context for the next turn. Called from [`Claim`]'s `Drop`,
    /// which is what makes it un-forgettable.
    pub fn release(&mut self, context: &str) {
        self.claimed.remove(context);
    }

    /// Drops every session past its TTL, returning them to be closed.
    pub fn reap(&mut self, now: Instant, ttl: Duration) -> Vec<Idle<S>> {
        let expired: Vec<String> = self
            .idle
            .iter()
            .filter(|(_, idle)| now.saturating_duration_since(idle.last_used) >= ttl)
            .map(|(context, _)| context.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|context| self.idle.remove(&context))
            .collect()
    }

    /// Forgets every session *without* closing it, for a caller whose
    /// connection has died: `session/close` is a request, and the connection
    /// that would answer it is the one that is gone. Returns how many were
    /// discarded, for the log line.
    pub fn clear(&mut self) -> usize {
        let count = self.idle.len();
        self.idle.clear();
        self.claimed.clear();
        count
    }

    /// The context of the least recently used idle session.
    fn oldest(&self) -> Option<String> {
        self.idle
            .iter()
            .min_by_key(|(_, idle)| idle.last_used)
            .map(|(context, _)| context.clone())
    }

    /// Takes one context's session out, for a caller that means to close it:
    /// the control surface's `DELETE /sessions/{contextId}`.
    ///
    /// The pool has no opinion about closing — it does no I/O — so the session
    /// comes back to be closed by whoever asked for it, exactly as the eviction
    /// paths do it.
    pub fn take(&mut self, context: &str) -> Option<Idle<S>> {
        self.idle.remove(context)
    }

    /// Whether a turn is running for this context right now.
    ///
    /// The control surface needs the distinction, and cannot get it from
    /// [`Self::take`]: a checked-out session looks exactly like one that never
    /// existed, and "your conversation is running" is not the same answer as
    /// "you have no conversation" (rule 1).
    pub fn is_claimed(&self, context: &str) -> bool {
        self.claimed.contains(context)
    }

    /// Every session being held, with the context holding it. Read-only, for
    /// `GET /sessions`; the pool is never modified by a listing.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Idle<S>)> {
        self.idle
            .iter()
            .map(|(context, idle)| (context.as_str(), idle))
    }

    /// How many sessions are being held. `/status` reads this.
    pub fn len(&self) -> usize {
        self.idle.len()
    }

    pub fn is_empty(&self) -> bool {
        self.idle.is_empty()
    }
}

/// Marks a context as having a turn in flight, for as long as the turn holds it.
///
/// A guard rather than a flag, because the flag has to be cleared on the way out
/// of a turn and a turn has many ways out — the prompt fails, the wall clock
/// fires, the caller goes away. A missed clear would leave the context looking
/// permanently busy, and the symptom would be a conversation that silently stops
/// being remembered. `Drop` cannot miss.
pub struct Claim<'a, S> {
    pool: &'a Mutex<Pool<S>>,
    context: String,
}

impl<'a, S> Claim<'a, S> {
    pub fn new(pool: &'a Mutex<Pool<S>>, context: String) -> Self {
        Self { pool, context }
    }
}

impl<S> Drop for Claim<'_, S> {
    fn drop(&mut self) {
        match self.pool.lock() {
            Ok(mut pool) => pool.release(&self.context),
            // A poisoned pool is somebody else's bug; releasing a claim is not
            // worth a second panic, and dropping the guard is the last thing
            // that happens on the way out of a turn.
            Err(poisoned) => poisoned.into_inner().release(&self.context),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session type in these tests is a string: the policy is about *which*
    /// session and *when*, never about what a session is, and that is exactly
    /// what makes it testable without a `goose serve`.
    type Session = &'static str;

    const TTL: Duration = Duration::from_secs(600);
    const CAP: usize = 4;

    fn idle(session: Session, cwd: &str, at: Instant) -> Idle<Session> {
        Idle {
            session,
            cwd: PathBuf::from(cwd),
            last_used: at,
        }
    }

    /// A fixed origin, since `Instant` cannot be constructed from a number.
    fn origin() -> Instant {
        Instant::now()
    }

    fn seconds(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn an_unknown_context_has_nothing_to_reuse() {
        let mut pool: Pool<Session> = Pool::new();
        let (acquired, dropped) = pool.acquire("c1", Path::new("/tmp"), origin(), TTL);
        assert!(matches!(acquired, Acquired::Fresh { vacant: true }));
        assert!(dropped.is_empty());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn a_retained_session_is_handed_back_to_the_same_context_and_directory() {
        let t0 = origin();
        let mut pool = Pool::new();
        assert!(
            pool.retain("c1", idle("s1", "/tmp", t0), CAP).is_empty(),
            "the first session for a context evicts nothing"
        );

        let (acquired, dropped) = pool.acquire("c1", Path::new("/tmp"), seconds(t0, 1), TTL);
        match acquired {
            Acquired::Reuse(idle) => assert_eq!(idle.session, "s1"),
            Acquired::Fresh { .. } => panic!("the context's own session must be reused"),
        }
        assert!(dropped.is_empty());
        assert_eq!(pool.len(), 0, "a checked-out session is not in the pool");
    }

    #[test]
    fn a_second_turn_for_a_claimed_context_is_offered_nothing_and_owns_nothing() {
        // The busy rule, at the level the pool can see it: the first turn has
        // checked the session out, so the second finds no session *and* is told
        // the context is not vacant — which is what stops it from overwriting
        // the first turn's memory when it finishes.
        let t0 = origin();
        let pool = Mutex::new(Pool::new());
        pool.lock()
            .unwrap()
            .retain("c1", idle("s1", "/tmp", t0), CAP);

        let (first, claim, _) = {
            let mut guard = pool.lock().unwrap();
            let (acquired, dropped) = guard.acquire("c1", Path::new("/tmp"), t0, TTL);
            (acquired, Claim::new(&pool, "c1".to_string()), dropped)
        };
        assert!(matches!(first, Acquired::Reuse(_)));

        let (second, _) = pool
            .lock()
            .unwrap()
            .acquire("c1", Path::new("/tmp"), t0, TTL);
        assert!(
            matches!(second, Acquired::Fresh { vacant: false }),
            "a context with a turn running is busy, not unknown"
        );

        // And once that turn is over, the context is unknown again rather than
        // stuck busy forever.
        drop(claim);
        let (third, _) = pool
            .lock()
            .unwrap()
            .acquire("c1", Path::new("/tmp"), t0, TTL);
        assert!(matches!(third, Acquired::Fresh { vacant: true }));
    }

    #[test]
    fn a_session_rooted_elsewhere_is_not_reused_and_the_context_moves_with_it() {
        // `cwd` is how a caller chooses what a turn can see, so a session opened
        // somewhere else must never be handed to it (constraint #4).
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("c1", idle("s1", "/tmp", t0), CAP);

        let (acquired, dropped) = pool.acquire("c1", Path::new("/srv/app"), seconds(t0, 1), TTL);
        assert!(matches!(acquired, Acquired::Fresh { vacant: true }));
        assert_eq!(
            dropped.len(),
            1,
            "the session at the old directory is given up"
        );
        assert_eq!(dropped[0].session, "s1");
    }

    #[test]
    fn an_idle_session_past_its_ttl_is_dropped_rather_than_reused() {
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("c1", idle("s1", "/tmp", t0), CAP);

        let (acquired, dropped) =
            pool.acquire("c1", Path::new("/tmp"), seconds(t0, TTL.as_secs() + 1), TTL);
        assert!(matches!(acquired, Acquired::Fresh { vacant: true }));
        assert_eq!(
            dropped.len(),
            1,
            "an expired session must be closed, not handed out"
        );
        assert_eq!(dropped[0].session, "s1");
    }

    #[test]
    fn reaping_only_takes_the_sessions_that_are_past_the_ttl() {
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("old", idle("s1", "/tmp", t0), CAP);
        pool.retain("fresh", idle("s2", "/tmp", seconds(t0, 500)), CAP);

        let dropped = pool.reap(seconds(t0, 601), TTL);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].session, "s1");
        assert_eq!(pool.len(), 1, "the session inside its TTL stays");
    }

    #[test]
    fn the_pool_keeps_its_cap_and_drops_the_least_recently_used() {
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("a", idle("s1", "/tmp", t0), 2);
        pool.retain("b", idle("s2", "/tmp", seconds(t0, 1)), 2);

        let dropped = pool.retain("c", idle("s3", "/tmp", seconds(t0, 2)), 2);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].session, "s1", "the oldest session is evicted");
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn a_cap_of_zero_retains_nothing() {
        // `maxSessions: 0` is a way to say "do not hold sessions", and it must
        // not become a pool that grows anyway.
        let t0 = origin();
        let mut pool = Pool::new();
        let dropped = pool.retain("c1", idle("s1", "/tmp", t0), 0);
        assert_eq!(dropped.len(), 1);
        assert!(pool.is_empty());
    }

    #[test]
    fn one_context_never_holds_two_sessions() {
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("c1", idle("s1", "/tmp", t0), CAP);
        let dropped = pool.retain("c1", idle("s2", "/tmp", seconds(t0, 1)), CAP);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].session, "s1");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn clearing_forgets_sessions_without_handing_them_back_to_be_closed() {
        // The dead-connection case: there is nothing to close *with*, so the
        // sessions are dropped rather than returned.
        let t0 = origin();
        let mut pool = Pool::new();
        pool.retain("c1", idle("s1", "/tmp", t0), CAP);
        pool.retain("c2", idle("s2", "/tmp", t0), CAP);

        assert_eq!(pool.clear(), 2);
        assert!(pool.is_empty());
        // And the contexts are free again, not stuck busy behind a connection
        // that no longer exists.
        let (acquired, _) = pool.acquire("c1", Path::new("/tmp"), t0, TTL);
        assert!(matches!(acquired, Acquired::Fresh { vacant: true }));
    }
}
