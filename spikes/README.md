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
| [S2](S2.md) | Does goose forward `X-LiteLLM-Trace-Id` to its provider calls? | **FAIL as specified** — 🚩 escalated, then **decided**: bound the loop, not the wallet |
| [S3](S3.md) | Exact shapes of the ACP methods | **PASS** — fixtures committed; the transport is POST **+ SSE** |
| [S4](S4.md) | Does one `goose serve` handle concurrent sessions? | **PASS** — concurrent, isolated, 2.0s for two turns |
| [S5](S5.md) | Does `DELETE /v1/agents/{id}` 404 on an already-deleted id? | **PASS** — 404; the sweeper's guard #3 holds |
| [S6](S6.md) | Does the deployed goose advertise `sessionCapabilities.close`? | **PASS** — `close`, `list` and `delete` are all advertised |
| [S7](S7.md) | Does `protocolVersion: "1.0"` survive a caller with no `a2a-version` header? | **PASS** — the server never reads the header |
| [S8](S8.md) | Can the agent run as a host process on DSM 7? | **NOT RUN** — needs the NAS shell |
| [S9](S9.md) | Can the LiteLLM container reach the agent at its `card.url`? | **NOT RUN** — needs the NAS shell |
| [S10](S10.md) | Recipe mining: where do recipes live, and is the shape stable? | **PASS, with one correction to §6.1** |
| [S11](S11.md) | Does the emitted launcher resolve the right triple, fail open, and *upgrade*? | **Item 5 failed, now fixed** — the flip was a no-op on every upgrade; the rest needs a host |
| [S12](S12.md) | Does the published binary run on the target host? | **NOT RUN** — needs both hosts and a release |
| [S13](S13.md) | Is `a2a-rs` wire-compatible with LiteLLM's A2A routes? | **PASS** — the framing agrees; the *card* needs work on our side |
| [S14](S14.md) | Does the agent really own `goose serve`? | **PASS on this box** — starts, gates, restarts, refuses; a host is [S8](S8.md)/[S12](S12.md) |

## What the spikes changed

Eight spikes changed the plan, this repo, or the launcher. They are the ones
worth reading:

- **S2** — cost control changes shape, twice over. goose 1.50.x cannot carry a
  per-session trace id upstream, **and** LiteLLM 1.103.0 silently drops
  `max_iterations` / `max_budget_per_session` /
  `require_trace_id_on_calls_by_agent` on `POST /v1/agents` — so §6.7 as written
  is not implementable on either side. Escalated (§12.6), then **decided** —
  and then *corrected*: the owner pushed back on proxy-side budgets, so the
  accounting was tested. goose and LiteLLM agree **exactly** on tokens
  (34001 = 34001) but price the same call **3.5x apart**, because both are local
  price tables and neither is the invoice. One A2A turn also fans out into more
  than one provider call, and the extra one is invisible in the ACP usage block.
  So no budgets and no per-agent keys: the agent bounds the **loop** (iterations,
  concurrent sessions, context, wall clock), which is exact and ours, and spend
  stays an after-the-fact question for LiteLLM's logs. Full reasoning at the end
  of [S2](S2.md).
- **S3** — the ACP transport is **HTTP POST for requests *plus* SSE for replies
  and notifications**, not "POST for requests, WebSocket for notifications" as
  §6.4 assumed. `session/new` and `session/prompt` return 202 with an empty body.
- **S4** — one `goose serve` multiplexes concurrent sessions, isolated, on one
  connection. The agent does not need a process or a port per context; it needs a
  demultiplexer. Also: goose enforces `cwd` (`-32602 invalid directory path`).
- **S10** — recipes have **no `name` field**. The skill `id` must come from the
  filename, not a recipe field.
- **S13** — the wire is compatible, but the **card** is not: LiteLLM synthesises
  its own card for a registered agent (its `skills: [{id: "chat"}]`, its address,
  its security scheme), so our skills are invisible to a proxy-side caller. And a
  second `POST /v1/agents` is a **400** on a duplicate name, so
  `re_register_on_card_change` must PUT. Its card-fetch path (the errors name the
  legacy `/.well-known/agent.json`) could not be confirmed — the container has no
  route to the agent, which is [S9](S9.md)'s question. Not a §12.6 escalation;
  the card work is ours.
- **S14** — the deploy tree shipped one unit, the launcher's, and **nothing
  started `goose serve`**: on reboot the agent came back, served a card, accepted
  calls and failed every turn with `connection refused` on `:3284` — a
  healthy-looking agent that could not answer. The fix is not a second unit (one
  restart mechanism per level): the **agent owns goose as a child**, checks the
  address before it binds, and refuses to start rather than adopt a server it did
  not start. It also turned `goose.acp.url` into a contract — the address is
  *dialled* and the child is *started from it*, so with `serve: own` it must be
  `http` on an IPv4 loopback with the bare `/acp` path.
- **S7** — `A2A-Version` is **decorative**: the pinned server never reads it, so
  a caller without the header is indistinguishable from one with it. That voids
  the risk it was gating — and also voids the plan's implied "a wrong version
  fails cleanly". A version refusal, if we ever want one, is ours to write.
- **S11 (item 5)** — the launcher's `current` flip was a **no-op on every
  upgrade**: `mv -f tmp current` follows `current` to the release directory it
  points at and moves the new link *inside* the old release. The host logged
  `installed 0.1.14` and kept executing 0.1.12 — self-update was fake, and it
  said so in the log. Fixed in the `fetch-launch` capability (genproj #28) and in
  this repo's seeded copy, with tests that run the launcher for real from a host
  that already has a release installed, because no static test could see it —
  the buggy script passed all of them.

## Not run, and what unblocks each

| Spike | Blocked on |
| ----- | ---------- |
| S8, S9 | a shell on the NAS. S9 is now *narrowed*: the LiteLLM container cannot resolve the tailnet name, cannot reach the sandbox bridge, and **hangs** on the agent's tailnet IP — so the obstacle is routing, and a wrong card `url` fails as a hang. It also blocks two S13 follow-ups (which card path the proxy fetches, and whether it forwards `metadata`/SSE). |
| S11, S12 | both hosts **and** a real GitHub Release (merge M0 to `main` first) |
