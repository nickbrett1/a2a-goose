# M2a implementation plan — the roost tunnel client

Branch: `feat/roost-tunnel`. Status is kept live in this file (the working
agreement asks for it): `[ ]` pending, `[~]` in progress, `[x]` done.

Spec: roost `src/protocol.rs` (wire types), `src/fleet.rs` (registration,
`(bootId, seq)` floor), `src/server.rs` (`/agent/ws`, the queries it sends).
Design: `docs/roost-tunnel.md`.

## Steps

1. **`[x]` Recon.** Clone roost read-only; read `protocol.rs`, `fleet.rs`,
   `server.rs`, `fake_agent.rs`. Record findings in `docs/roost-tunnel.md`.
2. **`[x]` Deps.** `Cargo.toml`: `tokio-tungstenite 0.30` (no default features,
   `connect` + `handshake` + `rustls-tls-webpki-roots` — the stack roost uses and
   the musl-safe TLS the ACP hop already chose) and `uuid 1` (`v4`).
   *Test:* `cargo fetch` resolves (done); the build in step 6 proves feature names.
3. **`[x]` Wire frames — `src/tunnel/protocol.rs`.** Mirror roost's agent→hub
   frames (`Hello`, `ActivityFrame`, `ResponseFrame`) and the hub→agent
   `ServerFrame` (`Request`, `Command`, `Unknown`). `PROTOCOL_VERSION = 1`.
   *Test:* parse roost's own `protocol.rs` test vectors — the memo shape for
   `hello`, opaque `event` for `activity`, unknown tag → `Unknown`, malformed
   `hello` → error, `request` round-trip.
4. **`[x]  `Identity — `src/tunnel/identity.rs`.** `mint_boot_id` (uuid v4),
   `hostname` (libc `gethostname`, already a dep), `Identity{…}::hello(boot_id,
   started_at)`. `protocolVersion = 1`, `kind = hub.kind`.
   *Test:* two boot ids differ and are non-empty; hello serialises camelCase and
   `protocolVersion == 1`; hostname is non-empty.
5. **`[x]  `Answers — `src/tunnel/answer.rs`.** `QueryAnswerer` trait and the
   dispatch: `status.get` → `status_payload`, `sessions.list` →
   `sessions_payload`, else `Unsupported`. `impl QueryAnswerer for Agent` lives in
   `src/tunnel/mod.rs` so the trait stays free of `Agent`.
   *Test:* a stub answerer pins the four dispatch arms and the `response` frame.
6. **`[x] `Tunnel client — `src/tunnel/mod.rs`.** `Tunnel{identity, url,
   credential, activity, answers}`; `spawn(&Config, Arc<Agent>) -> Option<JoinHandle>`.
   Dial → send `hello` first → replay backlog → live loop; `select!` over the
   activity receiver and the socket; respond to `request`; ignore `Unknown`;
   reconnect forever with capped backoff. Sends the credential as
   `Authorization: Bearer`.
   *Test:* unit-test the backoff sequence (pure fn, capped); integration test
   (step 9) covers the loop.
7. **`[x]` Config — `src/config.rs`, `config/config.example.yaml`.** New
   `hub {}` block: `enabled`, `url`, `credentialEnv`, `kind`,
   `connectTimeoutSecs`; `hub_credential()` reads the env var by name (repo
   convention); a warning (not a refusal) for `enabled` with an empty/odd url.
   *Test:* defaults; `deny_unknown_fields` still rejects a typo; the example
   config parses (`tests/example_config.rs`).
8. **`[x]` Wiring — `src/main.rs`, `src/lib.rs`.** `tunnel::spawn` beside
   `registry.spawn_registration`; `pub mod tunnel` in `lib.rs`.
9. **`[x]` Integration — `tests/tunnel_e2e.rs`.** Stand up a **minimal WS server**
   with `tokio_tungstenite::accept_async` (no axum `ws` feature needed), then
   assert: (a) `hello` arrives first and carries the boot id;
   (b) a `record()`ed activity arrives as an `activity` frame with the
   `(bootId, seq)` envelope and opaque `event`; (c) a hub-sent `request`
   `status.get` is answered `ok:true`; (d) a dropped server is reconnected with
   the **same** boot id.
10. **`[x]` Full check.** `cargo fmt --check`, `cargo clippy --all-targets -- -D
    warnings`, `cargo test`, `cargo test --locked`. Commit early/often.

## Calls made (things to review)

1. **The credential is nowhere in roost.** `protocol.rs`'s `Hello` has no
   credential field and `server.rs` checks none. *Call:* follow the repo's secret
   convention (`hub.credentialEnv` = a **name**), and send the value as
   `Authorization: Bearer <cred>` on the WebSocket handshake. The hub must be
   taught to read it — until then this is authentication-by-convention only.
   Recorded here because the brief said "as protocol.rs expects"; it expects
   nothing.
2. **`protocolVersion` is roost's wire version (`1`), not the A2A card's
   `"0.3"`/`"1.0"`.** Sending the card value would be a plausible-looking bug.
   The tunnel sends `1`.
3. **`capabilities` is an unconstrained string list** (roost stores it verbatim
   and the UI may render it). *Call:* advertise only what M2a implements —
   `["activity", "status", "sessions"]` — and **not** `"history"`, `"logs"` or
   `"reboot"`, so the hub does not offer a tab that would 502.
4. **The roost frames are mirrored, not depended on.** roost is a hub binary
   (axum + a Svelte bundle), not a published library, so a git dependency would
   drag the whole hub into the agent's build for ~120 lines of types. *Call:*
   mirror them in `src/tunnel/protocol.rs` with a header naming
   `roost/src/protocol.rs` as the spec, and pin them with roost's **own test
   JSON** so drift is a test failure. (Hard constraint #17 — "do not hand-roll
   protocol types" — is about the **A2A** wire and does not reach a third-party
   hub's private wire.)
5. **No ack frame.** roost sends nothing after `hello`; the fake agent treats
   the send as success and starts publishing. *Call:* do the same — subscribe to
   `ActivityHub` immediately after `hello`, so no event is lost (the backlog
   covers the gap).
6. **`sessions.list` has no consumer-side schema.** roost parses the body of
   `status.get` but never of `sessions.list` (it is proxied to the browser
   verbatim). *Call:* answer with our own `GET /sessions` payload, so the hub and
   the agent agree on one definition of a session.
7. **Reconnect and the backlog seam.** The hub drops duplicates by `(bootId,
   seq)`, and `ActivityHub::subscribe` can duplicate at the seam. *Call:* replay
   the whole backlog on every reconnect and let the hub dedup — the documented
   contract, and the reason `seq` travels with every frame.
