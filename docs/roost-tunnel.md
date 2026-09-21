# The roost tunnel — findings and M2a design

Findings memo for M2a: making an `a2a-goose` agent a first-class citizen of the
`roost` mission-control hub. Read against **roost `src/protocol.rs`,
`src/fleet.rs`, `src/server.rs` and `src/fake_agent.rs`** (cloned at
`/tmp/roost`), which are the spec. Where this memo and roost disagree, roost
wins; the disagreements that mattered are in `docs/m2a-plan.md` §"Calls made".

## 1. The agent's current shape

`a2a-goose` is one process per **host** (not per project), a thin A2A facade over
a host's `goose serve`:

- **`server::Agent`** (`src/server.rs`) holds every long-lived handle: `config`,
  `skills`, `card`, `turns: Arc<dyn Turns>`, `activity: Arc<ActivityHub>`,
  `registry`, `serve`, `started`. It is shared as an `Arc` and never mutated.
- **`ActivityHub`** (`src/activity.rs`) is the operator's view of a turn: a
  process-wide monotonic `seq` (`AtomicU64`, first event `seq = 0`), a bounded
  in-memory ring (`backlog_cap`), and a `broadcast` channel. `subscribe()` hands
  a new consumer **the backlog and a receiver, in that order**, and documents
  that the seam can duplicate — a consumer must drop duplicates by `seq`. It
  already serialises each `Activity` as `{seq, at, contextId?, taskId?,
  sessionId?, skill?, type, ...}` — the exact §3.1 envelope roost forwards.
- **`GET /sessions`** (`server::sessions_payload`) is the retained-session list:
  `{ inFlight, retained, sessions: [{ contextId, sessionId, cwd, skillId,
  idleSecs }] }`, behind the bearer token.
- **`GET /status`** (`server::status_payload`) already carries the three fields
  roost reads: `acp.inFlight`, `sessions.count`, `activity.enabled`.
- **`Config`** (`src/config.rs`) is `deny_unknown_fields`, camelCase, and follows
  one rule for secrets: every secret is a **name of an env var**, never a value
  (`server.bearerTokenEnv`, `goose.acp.secretEnv`, `registry.masterKeyEnv`).
- **The A2A wire is the SDK's**, never hand-rolled (hard constraint #17). The
  roost wire is **not** A2A and roost is not published as a crate.

## 2. The four greenfield gaps

1. **Tunnel** — there is no outbound WebSocket client at all. Cargo.toml says so
   in as many words ("no WebSocket client at all", Spike S3): the ACP hop is
   HTTP + SSE, so nothing dials out and holds a socket.
2. **Identity** — there is no per-process `bootId`, no `kind`, and no `hello`.
   `seq` exists, but it is only the activity counter; it is not paired with a
   boot id, so it cannot be an ordering key across restarts.
3. **Publish bridge** — `ActivityHub` is read by `GET /events` (SSE) and by
   nothing else. Nothing subscribes to it and forwards frames to a second party.
4. **Query answering** — the agent answers A2A JSON-RPC on `POST /`; it has no
   notion of answering a *hub's* `request` frame (`status.get`,
   `sessions.list`).

## 3. What roost actually expects (the spec, read from source)

**Frames** (`src/protocol.rs`) — every frame is a JSON object with a `type`
tag; unknown tags parse to `Unknown` and are ignored, never fatal.

- agent → hub: `hello`, `activity`, `log`, `response`.
  - `hello { type, agentId, host, kind, agentVersion, protocolVersion, bootId,
    startedAt?, skills[], capabilities[] }`. `protocolVersion` is roost's
    **wire** version (`PROTOCOL_VERSION = 1`) — **not** the A2A card's
    `"0.3"`/`"1.0"`.
  - `activity { type, bootId, seq, at?, contextId?, taskId?, sessionId?,
    skill?, event }`. `event` is **opaque JSON** — the hub forwards it verbatim
    and reads only `event.type`.
  - `response { type, id, ok, body?, error? }`.
- hub → agent: `request { type, id, method, params }` and
  `command { type, id, action, mode?, force? }`.

**Registration and ordering** (`src/fleet.rs`, `src/server.rs`):

- The agent dials `GET /agent/ws`; the **first text frame must be `hello`**
  within 10 s or the tunnel is dropped (`read_hello`). No ack is sent back.
- The hub's ordering key is `(agentId, bootId, seq)`: an activity whose `bootId`
  is not the tracked one is `StaleBoot`; a `seq` `<= last_seq` is `Duplicate`;
  a new boot **resets the floor**, so a reconnecting agent may replay its whole
  backlog and the hub drops the overlap.
- The hub polls `status.get` every `status_poll_ms` (default 15 s) and reads
  `inFlight` from `body.acp.inFlight`, `session_count` from
  `body.sessions.count`, `activity_enabled` from `body.activity.enabled`.
  "Stuck" = `inFlight > 0` and no new activity for `stuck_after_ms`.
- `history.*` is **request/response over the tunnel**; roost's fake agent also
  serves `sessions.list`. M2a answers `status.get` and `sessions.list`.

**Auth: there is none yet.** `src/server.rs::agent_ws` reads a `hello` and
registers whatever `agentId` it is given — no token, header, query or field is
checked anywhere in roost, and `Hello` has no credential field. The credential
is *our* side of a boundary roost has not built yet (see §5 and the memo's
calls).

## 4. M2a design

Four small pieces, in `src/tunnel/`, plus a config block and one `main` wiring
line. Nothing else in the agent changes; **`history.*` and goose's
`sessions.db` are untouched**.

```
src/tunnel/protocol.rs   wire frames, mirrored from roost's protocol.rs
src/tunnel/identity.rs   bootId, host, kind, the `hello`
src/tunnel/answer.rs     QueryAnswerer: status.get, sessions.list
src/tunnel/mod.rs        the client: dial, hello, replay+live, backoff, respond
```

- **Identity.** `bootId` is a fresh `uuid::Uuid::new_v4()` minted once at
  process start and held for the process's life. `kind` is `hub.kind`
  (default `"a2a-goose"`). `agentId = card.name`. `agentVersion = card.version`.
  `host` is the machine hostname. `protocolVersion = 1` (roost's wire version).
- **Tunnel.** One task, spawned from `main` next to the registry registration.
  It dials `hub.url`, sends `hello` first, then subscribes to `ActivityHub`. It
  reconnects forever with capped exponential backoff; **an unreachable hub is a
  retry, never a crash** (fail open). Unknown server frames are ignored;
  a `command` is answered `ok:false, error:"unsupported"` (commands are M4).
- **Publish bridge.** After `hello`, `subscribe()` gives the backlog and a live
  receiver. Every `Activity` becomes an `activity` frame with the
  `(agentId, bootId, seq)` envelope: `seq` is the existing `ActivityHub` seq,
  `at` its RFC 3339 stamp, `contextId`/`taskId`/`sessionId`/`skill` its identity,
  and `event` the inner `ActivityEvent` — **opaque JSON, no new schema**.
  Reconnect replays the backlog; the hub's boot/seq floor drops the overlap.
- **Answers.** A `request` is dispatched to a `QueryAnswerer`:
  `status.get` → `server::status_payload`, `sessions.list` →
  `server::sessions_payload`, everything else (`history.*`, `logs.tail`,
  unknown) → `ok:false, error:"unsupported_method: …"`. A trait (not a direct
  `Agent` dependency) so the tunnel is testable without a `goose serve`.
- **Credential.** `hub.credentialEnv` names the env var (repo convention); the
  value is sent on the WS handshake as `Authorization: Bearer <cred>`. This is
  the one field roost does not yet read; see the calls.

## 5. Out of scope for M2a

- `history.*` (M1/M3) and goose's `sessions.db`. Unsupported answers, so the
  hub's History tab 502s honestly instead of lying.
- `log` frames / the log subscription.
- `command` handling (reboot — M4). Answered `unsupported`.
- Hub-side auth enforcement. We send the credential; roost must learn to check
  it. Tracked as a call in `docs/m2a-plan.md`.
