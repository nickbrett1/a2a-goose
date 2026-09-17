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
run on the **NAS** on 2026-09-17 against the live proxy (`litellm 1.103.0`), and
**re-probed** the same day once the NAS's tailnet was fixed — the re-probe
moved `card.url` to `http://100.77.144.14:10001`. S8 and S12 were run on the
**NAS** on 2026-09-17 as well, against a published release (v0.1.25) — the first
time either half of the DSM deployment has been exercised on the box, and S8 was
then closed by a **real reboot** of the NAS. S12's run
went past `/status` to a real A2A turn on the host (`TASK_STATE_COMPLETED`), the
M3 control surface against a real runner, and a spend-log row naming the host.
S15 was run on the **NAS** on 2026-09-17 too, against the live proxy and a
registered `nas-goose`, to answer the question the cancelled n8n leg left behind
(how Open WebUI reaches the agent) — it closed the "the agent appears as a model"
assumption as **false**, and with it the reason it was assumed. S16 was run on the
**NAS** on 2026-09-17 as well, to answer the follow-on question ("can a session with
*any* model find and message the agents?") — it turned the agents into MCP tools
served through mcphub, which is the surface both goose and Open WebUI already use.
S17 was run on **mac-studio** on 2026-09-17 (`v0.1.37`, goose 1.50.1) to answer
why that host's agent kept losing its ACP port.

| # | Question | Verdict |
| --- | -------- | ------- |
| [S1](S1.md) | Which `goose serve` invocation? | **PASS** — identical surface; pin the bare one |
| [S2](S2.md) | Does goose forward `X-LiteLLM-Trace-Id` to its provider calls? | **FAIL as specified** — 🚩 escalated, then **decided**: bound the loop, not the wallet. Addendum 2026-09-17: the variable's syntax (`Name: value` lines, *not* JSON — the JSON spelling breaks the provider) |
| [S3](S3.md) | Exact shapes of the ACP methods | **PASS** — fixtures committed; the transport is POST **+ SSE** |
| [S4](S4.md) | Does one `goose serve` handle concurrent sessions? | **PASS** — concurrent, isolated, 2.0s for two turns |
| [S5](S5.md) | Does `DELETE /v1/agents/{id}` 404 on an already-deleted id? | **PASS** — 404; the sweeper's guard #3 holds |
| [S6](S6.md) | Does the deployed goose advertise `sessionCapabilities.close`? | **PASS** — `close`, `list` and `delete` are all advertised |
| [S7](S7.md) | Does `protocolVersion: "1.0"` survive a caller with no `a2a-version` header? | **PASS** — the server never reads the header |
| [S8](S8.md) | Can the agent run as a host process on DSM 7? | **PASS** — one boot-up task, created by CLI, owned by init (`ppid 1`), bringing the agent up from a cold box **and from a real reboot** (2m27s kernel-boot→`/healthz`, `/volume1` mounted before the task ran). The reboot exposed a registry-lockout fault (§5) that is owed to `registry.rs`, not to this question |
| [S9](S9.md) | Can the LiteLLM container reach the agent at its `card.url`? | **SPLIT** — addressing **PASS**, re-probed the same day: the **tailnet IP literal** `100.77.144.14:10001` works (the LAN literal it first passed on was a 24 h DHCP lease; the tailnet was unreachable when that was measured, then fixed); card fetch **FAIL** — LiteLLM 1.103.0 never fetches the card, 🚩 escalated, then **decided**: register the card ourselves |
| [S10](S10.md) | Recipe mining: where do recipes live, and is the shape stable? | **PASS, with one correction to §6.1** |
| [S11](S11.md) | Does the emitted launcher resolve the right triple, fail open, and *upgrade*? | **PASS on mac-studio** — the flip was a no-op on every upgrade, then fixed and proven by two real upgrades; item 4 and the DSM host pending |
| [S12](S12.md) | Does the published binary run on the target host? | **PASS on DSM** — the x86_64 musl static-PIE payload execs, `check-goose.sh` passes on the host, the version refusal precedes the bind, `/status` names the host's real goose, the manifest→tarball→running-payload digests agree, and a real turn answers `TASK_STATE_COMPLETED` with the host's name on LiteLLM's row; the darwin half was seen on mac-studio in S11/S14 |
| [S13](S13.md) | Is `a2a-rs` wire-compatible with LiteLLM's A2A routes? | **PASS** — the framing agrees; the *card* needs work on our side |
| [S14](S14.md) | Does the agent really own `goose serve`? | **PASS, box and darwin host** — starts, gates, restarts, refuses; DSM is [S8](S8.md) |
| [S15](S15.md) | Can Open WebUI reach the agent through LiteLLM? | **SPLIT** — LiteLLM's `/a2a/{agent_id}` route reaches it and a real turn came back (`pong`), but the path an OpenAI-compatible client can use — an agent as a **model** (`a2a/<name>`) — is blocked: LiteLLM 1.103.0 sends the A2A **0.3** dialect (`message/send`) while `a2a-lf` speaks **1.0** (`SendMessage`), with no negotiation and no fallback. Decided **and built the same day**: a small Open WebUI bridge onto the route (`integrations/openwebui/`), which answered a real turn through Open WebUI's own API — `zeta`, remembered on the second turn of the same chat (§7) |
| [S16](S16.md) | Can a session with *any* model find and message the agents? | **PASS** — the agents became two MCP tools (`list_agents`, `ask_agent`) in `integrations/a2a-mcp/`, registered in every one of mcphub's nine groups, so a goose session and an Open WebUI model get them the same way they get memos: `the nas agent` resolved, the agent answered (`NAS`), and through the hub `ask_agent(…) → hub`. From Open WebUI, mcphub's own log shows `a2a.list_agents` then `a2a.ask_agent` in group `core` (§4). The roster is read live per call, so a new agent appears by itself — and the `static_headers` row that LiteLLM's route needs is now written by registration itself, measured on the NAS in `v0.1.35` (S15) |
| [S17](S17.md) | Why did the Mac's agent keep losing its ACP port? | **PASS** — the holder was the **editor**: VS Code with `remote.autoForwardPortsSource: "hybrid"` forwards ports it scrapes out of **terminal output**, and a forwarded port is an all-interfaces listener on the host (probe: a printed `127.0.0.1:32901` was held within 25 s, the same probe after the setting became `"process"` was not). Fixed at the source; the agent moved to `127.0.0.1:32841` and answers through `/a2a/{id}`. The same run corrected S9 (LiteLLM's container **does** reach a peer's tailnet IP; the LAN literal was the editor's listener — TCP up, HTTP never) and measured **~8 min** for a turn it took at the time — since **overturned** by [S18](S18.md) |
| [S18](S18.md) | Is the first turn on a fresh `contextId` the expensive one? | **NO — overturns S17's latency claim.** Five timed turns on mac-studio (`0.1.37`, goose 1.50.1) through `/a2a/{agent_id}`: a brand-new `contextId` on a warm payload **1.9 s**, the first turn after a `SIGTERM` restart **2.4 s**, three simultaneous turns on three new contexts on a just-restarted payload **2.4 / 2.6 / 2.8 s**, a substantive tool-using turn **3.9 s**. No cold-session, cold-payload or concurrency warm-up exists to budget for — size a caller's timeout to the *task*, not to startup; pre-warming at boot buys nothing. Also: **`agent_id` changes on every payload restart**, so resolve agents by name |

## What the spikes changed

Eleven spikes changed the plan, this repo, or the launcher. They are the ones
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
  legacy `/.well-known/agent.json`) could not be confirmed — the container had no
  route to the agent at the time, which was [S9](S9.md)'s question (the route
  exists now; the fetch still does not, which is S9's *other* half). Not a §12.6
  escalation;
  the card work is ours.
- **S9** — the LiteLLM container **could not** reach the tailnet at all when this
  was first measured (no DNS, peer timeouts) and did reach the agent on the LAN,
  so `card.url` was a **LAN literal** — a 24 h DHCP lease. The NAS's Tailscale
  was fixed later that day and the spike was **re-probed**: `100.77.144.14:10001`
  now serves the real card from inside the container, MagicDNS still does not
  resolve there (`TS_ACCEPT_DNS=false` on the sidecar), and since option A makes
  the registered `url` ours to set, `card.url` is now the **static tailnet IP
  literal**. Separately, and unaffected by any of that, LiteLLM 1.103.0 **never
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
- **S8** — the DSM half of the host-process deployment, closed by a **real
  reboot**: one boot-up task created by CLI, owned by init (`ppid 1`), the agent
  up 2m27s after kernel boot with `/volume1` already mounted (the boot-up unit is
  `After=basic.target`). It corrected two of its own claims: the `while :` loop is
  **not** a backstop for a late-mounting volume (the launcher existence check is
  before the loop and `exit 1`s), and the wrapper's `logger` lines are **not**
  reliably in the journal after a real boot — `boot.log` (the task's redirect) is
  the log of record. The reboot also exposed a **registry lockout**, and this one
  is not a host-process question: a node that beats its proxy at boot makes four
  fast registration attempts, gives up, and never retries; and the previous run's
  stale `LiteLLM_AgentsTable` row then turns every later attempt into
  `500 Unique constraint failed on the fields: (agent_name)`. `GET /v1/agents`
  returns `[]`, so the "update in place" path never fires. Net: an agent healthy
  but **permanently** unregistered until the row is deleted by hand — the same
  shape as S14, one level out: not "a healthy agent nobody can talk to" but "a
  healthy agent nobody is registered to route to". The fix (bounded retry,
  treat an `agent_name` conflict as an update) is owed to `registry.rs`.
- **S15** — the chain works, but not down the path an OpenAI-compatible caller
  can take, and this is the finding that replaced the assumption the n8n work
  died with. LiteLLM's **route** `/a2a/{agent_id}` is a transparent JSON-RPC
  proxy: whatever method the caller sends is what the agent receives, and with
  `static_headers` on the agent row carrying the bearer, a real goose turn came
  back (`TASK_STATE_COMPLETED`, `pong`). LiteLLM's **model** paths
  (`a2a/<agent_name>` on `/v1/chat/completions` *and* `/v1/responses`) build the
  request themselves with the 0.3 method name `message/send`, role `"user"` and
  `parts[{"kind":"text"}]` — hardcoded, card not consulted, no fallback on
  `method not found` — while `a2a-lf`/`a2a-server-lf` accept only the 1.0 names
  (`SendMessage`, `ROLE_USER`). So "the agent appears as a model in the proxy" is
  not available to us, and Open WebUI needs one small function onto the route —
  which now exists (`integrations/openwebui/`), and answered a turn through Open
  WebUI's own API on the NAS the same day.
  Two faults on the way: the proxy sends **no** credential unless told to
  (`static_headers` are honoured on the route, ignored by the model paths;
  `litellm_params.api_key` is stored and ignored everywhere), and the NAS agent
  was binding **loopback** while registering a tailnet URL — so it answered
  `/healthz` on the box and nothing at the address everything was dialling. The
  bind is fixed on the box, in both `deploy/env` templates, and the agent now
  **warns** at startup when `bind` is loopback and `publicUrl` is not.
- **S16** — an agent as a *model* is half the story; the other half is a **tool**.
  A conversation with any *other* model could not discover an agent at all, so the
  agents were turned into two MCP tools (`list_agents`, `ask_agent`) —
  `integrations/a2a-mcp/` — and registered in **every** mcphub group, which is how
  both a goose session and an Open WebUI model already reach their tools. The
  finding that made it nearly free: Open WebUI's tool servers *are* mcphub group
  endpoints (`http://mcphub:3000/mcp/<group>`) and the `core` workspace model is
  bound to one of them (`meta.toolIds`), so adding the server to the group is the
  whole integration. Two API details cost time and are now scripted: mcphub's
  dashboard API wants the JWT in **`x-auth-token`** (not `Authorization: Bearer`,
  and it says only "No token"), and adding a server to a group takes
  `{"serverName": …}` while `{"name": …}` is a 400. On Open WebUI's side, the
  requests are not interchangeable: `tool_ids` is what attaches a tool server, a
  `chat_id` turns the call into a background task whose answer arrives over the
  socket (so a REST poll sees an empty chat), and with neither, tool resolution is
  skipped — one run had the model *narrate* its tool call as JSON text. The wiring
  is ours; whether a given model reaches for the tool is the model's.
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
| [S8](S8.md) | not blocked — the reboot is done. One measurement is still un-run but nothing gates on it: a real DSM **package** upgrade, whose answer ("cannot orphan the wrapper, because its `ppid` is 1") is structural. §5's registry lockout is a `registry.rs` fix, tracked there, not a spike blocker. |
| S11 | item 4 (a bad download on a host) and the launcher **self-update's swap** — the verify-then-`cmp` path now runs on every start on both hosts, but no host has yet had a launcher actually replaced. The DSM half is no longer blocked: S8 ran the launcher there. |
| [S15](S15.md) | not blocked — LiteLLM's route reaches the agent today, and the bridge onto it is built and measured ([S15](S15.md) §7, `integrations/openwebui/`). Owed rather than blocked: a LiteLLM bug report (its A2A *model* paths send 0.3 method names to a card that advertises 1.0). |
| [S16](S16.md) | not blocked — the tools are built, registered in every mcphub group and measured from a goose session, a raw MCP client and Open WebUI. Un-run, and named as such in §5: mcphub's and Open WebUI's **tool-call timeouts** against a multi-minute turn, and whether a given model reliably *chooses* the tool (`deepseek-v4-flash` called it on some runs and rendered the call as JSON text on another). Both are properties of the callers, not of this server. |
