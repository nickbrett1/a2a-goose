# Spikes

One file per spike, `S<n>.md`, each with the question, the method, the raw
evidence and a verdict. Commit them (phase-1 plan §12.3): a spike that is not
written down is not done.

Every spike below was run against **goose 1.50.0** (the version the hosts run) on
2026-09-16, unless marked NOT RUN. The ACP leg ran against a real `goose serve`;
the LiteLLM leg ran against the live proxy on the NAS (`nas:4000`). Raw evidence
is quoted in each file; the sanitised S3 frames are committed as
`tests/fixtures/acp-turn.jsonl`. S11 and S14 were additionally run on the
**mac-studio host** on 2026-09-17 against a published release (v0.1.16); S9 was
run on the **NAS** on 2026-09-17 against the live proxy (`litellm 1.103.0`).

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
| [S9](S9.md) | Can the LiteLLM container reach the agent at its `card.url`? | **SPLIT** — addressing **PASS** (LAN literal; no tailnet); card fetch **FAIL** — LiteLLM 1.103.0 never fetches the card, 🚩 escalated, then **decided**: register the card ourselves |
| [S10](S10.md) | Recipe mining: where do recipes live, and is the shape stable? | **PASS, with one correction to §6.1** |
| [S11](S11.md) | Does the emitted launcher resolve the right triple, fail open, and *upgrade*? | **PASS on mac-studio** — the flip was a no-op on every upgrade, then fixed and proven by two real upgrades; item 4 and the DSM host pending |
| [S12](S12.md) | Does the published binary run on the target host? | **NOT RUN** — needs both hosts and a release |
| [S13](S13.md) | Is `a2a-rs` wire-compatible with LiteLLM's A2A routes? | **PASS** — the framing agrees; the *card* needs work on our side |
| [S14](S14.md) | Does the agent really own `goose serve`? | **PASS, box and darwin host** — starts, gates, restarts, refuses; DSM is [S8](S8.md) |

## What the spikes changed

Nine spikes changed the plan, this repo, or the launcher. They are the ones
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
- **S9** — the LiteLLM container **cannot** reach the tailnet at all (no DNS, peer
  timeouts), so `card.url` is a **LAN literal**; and LiteLLM 1.103.0 **never
  fetches the card** on the registration path — `POST /v1/agents` merges the
  posted `agent_card_params` and stamps its default `[chat]` skill into any card
  that lacks one. Re-registration cannot help (there is nothing to re-fetch), and
  the only fetcher, `POST /v1/a2a/discover`, is admin-only **and** SSRF-gated
  (`HTTP 400: URL targets a blocked address (192.168.1.33)`) until the host is in
  `user_url_allowed_hosts`. 🚩 §12.6: getting our skills into the registry is a
  decision (send the assembled card, add a discover+PUT reconciler, or fork
  LiteLLM), not a workaround. **Decided: send the assembled card** — the
  registration body now carries the card `card::assemble` builds (commit
  `e7bffb9`), verified end-to-end against the live proxy (stored `skills:
  ['ask']`). The reconciler and the fork remain the options only if the registry
  must track a **live** recipe edit without a restart.
- **S14** — the deploy tree shipped one unit, the launcher's, and **nothing
  started `goose serve`**: on reboot the agent came back, served a card, accepted
  calls and failed every turn with `connection refused` on `:3284` — a
  healthy-looking agent that could not answer. The fix is not a second unit (one
  restart mechanism per level): the **agent owns goose as a child**, checks the
  address before it binds, and refuses to start rather than adopt a server it did
  not start. It also turned `goose.acp.url` into a contract — the address is
  *dialled* and the child is *started from it*, so with `serve: own` it must be
  `http` on an IPv4 loopback with the bare `/acp` path. On darwin the whole
  thing now runs on mac-studio: goose is the agent's child (`ppid` = the agent),
  `kill -9` gives a new pid with `restarts: 1`, and the next turn still completes.
  The refusal earned its keep there too — a VS Code port forward on `*:3284`
  answers a real ACP `initialize`, so the agent **correctly** declined to start
  (the check is a *dial*, and a forwarded port is indistinguishable from a local
  goose); the host moved `goose.acp.url` to `:3285`.
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
  the buggy script passed all of them. mac-studio then ran **two real upgrades**
  (0.1.14 → 0.1.15 → 0.1.16), `current` moving each time with no strays, which is
  the proof the unit tests could not give.
- **S11 — the launcher on a host is a *git checkout*, not a release.** launchd on
  mac-studio runs `~/src/a2a-goose/scripts/fetch-launch.sh`, and it was **4
  commits behind**, so the first reinstall still ran the buggy flip and `git
  pull` was the actual fix. The code that supervises everything else is
  un-pinned; the release tarball carries the agent, not the launcher. **Decided:
  fetch it, digest-pinned** — the launcher becomes a release asset and replaces
  itself (verify sha256, `bash -n`, rename over the path) before it execs, with a
  `curl` cold start in the README. The contract is at the end of [S11](S11.md).

## Not run, and what unblocks each

| Spike | Blocked on |
| ----- | ---------- |
| S8 | a shell on the NAS. |
| S11 | item 4 (a bad download on a host), the **DSM host** (items 1–5 never run there), and the launcher **self-update** (decided, not yet on a host). |
| S12 | the **DSM** host. mac-studio now runs a published release (0.1.16), so the darwin half is seen. |
