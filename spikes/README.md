# Spikes

One file per spike, `S<n>.md`, each with the question, the method, the raw
evidence and a verdict. Commit them (phase-1 plan §12.3): a spike that is not
written down is not done.

Every spike below was run against **goose 1.50.0** (the version the hosts run) on
2026-09-16, unless marked NOT RUN. The ACP leg ran against a real `goose serve`;
the LiteLLM leg ran against the live proxy on the NAS (`nas:4000`). Raw evidence
is quoted in each file; the sanitised S3 frames are committed as
`tests/fixtures/acp-turn.jsonl`.

| # | Question | Verdict |
| --- | -------- | ------- |
| [S1](S1.md) | Which `goose serve` invocation? | **PASS** — identical surface; pin the bare one |
| [S2](S2.md) | Does goose forward `X-LiteLLM-Trace-Id` to its provider calls? | **FAIL as specified** — 🚩 escalation, needs a decision |
| [S3](S3.md) | Exact shapes of the ACP methods | **PASS** — fixtures committed; the transport is POST **+ SSE** |
| [S4](S4.md) | Does one `goose serve` handle concurrent sessions? | **PASS** — concurrent, isolated, 2.0s for two turns |
| [S5](S5.md) | Does `DELETE /v1/agents/{id}` 404 on an already-deleted id? | **PASS** — 404; the sweeper's guard #3 holds |
| [S6](S6.md) | Does the deployed goose advertise `sessionCapabilities.close`? | **PASS** — `close`, `list` and `delete` are all advertised |
| [S7](S7.md) | Does `protocolVersion: "1.0"` survive a caller with no `a2a-version` header? | **NOT RUN** — needs a2a-rs's server (M1) |
| [S8](S8.md) | Can the agent run as a host process on DSM 7? | **NOT RUN** — needs the NAS shell |
| [S9](S9.md) | Can the LiteLLM container reach the agent at its `card.url`? | **NOT RUN** — needs the NAS shell |
| [S10](S10.md) | Recipe mining: where do recipes live, and is the shape stable? | **PASS, with one correction to §6.1** |
| [S11](S11.md) | Does the emitted launcher resolve the right triple and fail open? | **NOT RUN** — needs a host and a real release |
| [S12](S12.md) | Does the published binary run on the target host? | **NOT RUN** — needs both hosts and a release |
| [S13](S13.md) | Is `a2a-rs` wire-compatible with LiteLLM's A2A routes? | **NOT RUN** — gates M1's exit |

## What the spikes changed

Four spikes changed the plan or this repo. They are the ones worth reading:

- **S2** — cost control changes shape. goose 1.50.x cannot carry a per-session
  trace id upstream, so §6.7's per-thread budgets are not implementable on the
  deployed goose. This is an explicit **§12.6 escalation trigger**: stop and ask.
- **S3** — the ACP transport is **HTTP POST for requests *plus* SSE for replies
  and notifications**, not "POST for requests, WebSocket for notifications" as
  §6.4 assumed. `session/new` and `session/prompt` return 202 with an empty body.
- **S4** — one `goose serve` multiplexes concurrent sessions, isolated, on one
  connection. The agent does not need a process or a port per context; it needs a
  demultiplexer. Also: goose enforces `cwd` (`-32602 invalid directory path`).
- **S10** — recipes have **no `name` field**. The skill `id` must come from the
  filename, not a recipe field.

## Not run, and what unblocks each

| Spike | Blocked on |
| ----- | ---------- |
| S7, S13 | M1's server existing (deliberately deferred — M1 answers them for free) |
| S8, S9 | a shell on the NAS |
| S11, S12 | both hosts **and** a real GitHub Release (merge M0 to `main` first) |
