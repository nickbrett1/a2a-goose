//! The `a2a-goose` node agent: a thin A2A facade over a host's `goose serve`.
//!
//! One of these runs per *host*, not per project: `session/new` takes a working
//! directory, so a single `goose serve` handles many projects and `cwd` is the
//! namespace.
//!
//! The agent holds **no conversation state** (hard constraint #1). `goose`'s own
//! `sessions.db` is the system of record; what lives here is a `contextId` →
//! `sessionId` map, which is recoverable via ACP `session/list` if it is lost.
//! You lose the label, not the conversation.

pub mod acp;
pub mod activity;
pub mod card;
pub mod config;
pub mod executor;
pub mod goose;
pub mod recipes;
pub mod registry;
pub mod serve;
pub mod server;
pub mod skills;
pub mod tunnel;
pub mod turn;
