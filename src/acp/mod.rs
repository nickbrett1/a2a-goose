//! The ACP leg: talk to a host's `goose serve` over its HTTP+SSE transport.
//!
//! This module is the *only* place that knows ACP exists. The A2A side
//! ([`crate::executor`]) resolves a skill and hands over a prompt; what happens
//! between that and an answer — the connection, the streams, the session ids —
//! stays here.
//!
//! It is split in two along the seam that matters:
//!
//! - [`transport`] is the multiplexing: one HTTP connection to `goose serve`,
//!   two SSE stream scopes, and a JSON-RPC `id` → awaiting caller map. Nothing
//!   in it knows what a session *is*, which is what makes it testable by
//!   replaying the frames [S3](../../spikes/S3.md) recorded.
//! - [`client`] is the lifecycle: `session/new`, `session/prompt`,
//!   `session/close`, and the `cwd` check that is this host's security boundary.
//! - [`pool`] is the reuse policy: which `contextId` keeps which session, when
//!   that session is handed back, and what evicts it. It holds no I/O, so the
//!   rules are testable on their own.
//! - [`turns`] is the implementation of [`crate::turn::Turns`]: it is what the
//!   A2A executor calls, so the executor never names an ACP method.
//!
//! The shapes here are not guesses. `goose serve`'s transport is HTTP POST for
//! requests **plus SSE for replies and notifications** — S3 corrects the plan on
//! this point, and the correction is why there is no WebSocket client anywhere
//! in this binary.

pub mod client;
pub mod pool;
pub mod transport;
pub mod turns;

pub use client::{AcpClient, CwdError, Session, resolve_cwd, secret_key};
pub use pool::{Acquired, Claim, Idle, Pool};
pub use transport::{
    ACP_PATH, AcpAddress, AcpAddressError, AcpError, CONNECTION_ID_HEADER, SECRET_HEADER,
    SESSION_ID_HEADER, Scope, Transport, acp_address, acp_endpoint,
};
pub use turns::AcpTurns;
