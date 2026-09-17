# a2a-mcp — the registered agents, as MCP tools

The Open WebUI bridge in [`../openwebui/`](../openwebui/) turns each registered
agent into a **model**: what you want when the agent is the thing you are talking
to. This is the other direction — a conversation with *any* model that wants to
**hand work** to an agent:

> "please follow up with the nas agent and finish the task"

A model cannot discover an agent it has no tool for, so this is that tool. MCP is
the surface both callers already speak: goose as an extension, Open WebUI 0.11+
as a Tool Server. One server, two consumers, no duplication between them.

```
integrations/a2a-mcp/
├── server.py          the MCP server: two tools, one proxy
├── test_server.py     27 tests, run in the image that ships
├── smoke.py           against a live server: what a client sees, and one real turn
├── wire_mcphub.py     registers the server in mcphub and offers it in every group
├── requirements.txt   pinned to what Open WebUI 0.11.3 itself runs
├── Dockerfile         python:3.12-slim, one dependency tree, unprivileged
└── compose.yaml       the NAS stack: pulls the GHCR image, no local build
```

## The two tools

| Tool | What it does |
| ---- | ------------ |
| `list_agents()` | The roster, read live from `GET /v1/agents` on **every call** — so an agent that has just come up is askable immediately and one that has been cleared is not offered. |
| `ask_agent(agent, message, context_id=None)` | One A2A 1.0 `SendMessage` to `/a2a/{agent_id}`, answered. `context_id` continues the agent's own conversation; omit it and the turn is a fresh one. |

`agent` is matched generously on purpose, because the request that motivated this
is phrased in prose: `the nas agent` → `nas` → `nas-goose`. Exact match first,
then a unique substring either way round. An unknown *or ambiguous* name is an
error that **lists the roster**, so a model corrects itself in one step instead of
guessing twice.

Three things are deliberately *not* done here, each for a measured reason
([`../../spikes/S15.md`](../../spikes/S15.md)):

- **The 1.0 request is built by hand.** LiteLLM's `/a2a/{agent_id}` route forwards
  a JSON-RPC method verbatim, so a hand-built `SendMessage`/`ROLE_USER` reaches an
  `a2a-lf` agent; LiteLLM's own model paths speak the 0.3 dialect and fail.
- **The credential is a proxy virtual key.** The agent's bearer stays in its
  LiteLLM row's `static_headers`; this process never holds it.
- **The answer is collected, not indexed.** A completed turn carries a second,
  empty `answer` artifact, so `artifacts[0].parts[0].text` would work only by luck.

## Deploying on the NAS

The image is built and published by CI — merging to `main` runs `:docker: Build
and publish image (GHCR, a2a-mcp)` (after `:python: Test (a2a-mcp tools)`
passes), which pushes `ghcr.io/nickbrett1/a2a-mcp:latest` for `linux/amd64`. The
box only pulls it:

```bash
sudo mkdir -p /volumeUSB1/usbshare/docker/a2a-mcp
cd /volumeUSB1/usbshare/docker/a2a-mcp
# copy compose.yaml — the image comes from GHCR, nothing is built here
umask 077 && printf 'LITELLM_API_KEY=%s\n' 'sk-…' > .env     # a **virtual** key
sudo docker compose pull && sudo docker compose up -d
```

`LITELLM_API_KEY` must be a LiteLLM virtual key, not the agent's bearer and not
the master key. It only needs to be able to reach `GET /v1/agents` and the
`/a2a/{agent_id}` route — both answer to a virtual key (measured).

The container joins the external `ai_proxy` network, which is where `litellm` and
`mcphub` live too, so `http://litellm:4000/a2a` and the container's own name
resolve without a published port. Traefik publishes it at
**`http://100.82.223.13:8092/a2a-mcp/mcp`** for MCP clients elsewhere on the
Tailnet.

### Updating it

Nothing to do by hand. The stack carries
`com.centurylinklabs.watchtower.scope=nick`, so **watchtower-nick** (60s) pulls
the new `latest` within a minute of the publish step finishing. To take a
release immediately instead of waiting: `sudo docker compose pull`.
`:latest` is retagged on every publish, so a bad one is fixed by publishing a
good one rather than by rolling a tag back.

To work on `server.py` on the box without publishing, add `build: .` to the NAS
copy of `compose.yaml` and `sudo docker compose up -d --build` — a local override,
not something to leave in the repo copy, which is the source of truth.

The image is `linux/amd64` because the NAS is x86_64 while the CI agents are
Apple silicon; the build step pins the platform rather than inheriting it.

## Wiring it in

### mcphub (goose sessions, and everything else)

```bash
# on the NAS
python3 wire_mcphub.py --all-groups            # reads MCPHUB_ADMIN_PASSWORD
```

It creates one mcphub server (`a2a`, `type: streamable-http`,
`url: http://a2a-mcp:8090/mcp`) and adds it to **every** group, so whichever group
endpoint a client uses — `/mcp/core`, `/mcp/dev`, `/mcp/container`, … — it now
also offers `a2a-list_agents` and `a2a-ask_agent`. That is the same footing the
memos tools have: foundational, everywhere.

Two things about mcphub's API are worth knowing, and the script exists so nobody
has to rediscover them:

- the dashboard API reads its JWT from **`x-auth-token`**, not
  `Authorization: Bearer` — the failure is a bare
  `{"message":"No token, authorization denied"}`;
- adding a server to a group takes `{"serverName": "…"}`, and the obvious
  `{"name": "…"}` is a 400.

mcphub caches a server's tool list from when it connected, so after a change to
`server.py` reload it (`POST /api/servers/a2a/reload`) or restart mcphub.

### Open WebUI (any model, `core` included)

Nothing to add if the deployment already points at the hub: Open WebUI's existing
tool servers **are** mcphub group endpoints (`http://mcphub:3000/mcp/<group>`,
`type: mcp`, `config.enable: true`), and the workspace model `core` is bound to
one of them (`meta.toolIds: ["server:mcp:mcphub"]` → the `core` group). Adding the
server to the group is what puts `mcphub_a2a-list_agents` /
`mcphub_a2a-ask_agent` in that model's tool list.

To point Open WebUI at this server directly instead — just the two agent tools,
without the rest of the group — add a connection like the others:

```
POST /api/v1/configs/tool_servers            (admin; replaces the whole list)
{"TOOL_SERVER_CONNECTIONS": [ …existing…, {
  "type": "mcp", "url": "http://a2a-mcp:8090", "path": "/mcp",
  "auth_type": "none", "headers": {},
  "config": {"enable": true, "function_name_filter_list": "", "access_grants": []},
  "info": {"id": "a2a", "name": "A2A agents", "description": "list_agents, ask_agent"},
  "id": "a2a", "name": "A2A agents"}]}
```

then `POST /api/v1/configs/tool_servers/verify` with the same connection object to
see the tools it discovers, and bind it to a model with
`meta.toolIds: ["server:mcp:a2a"]`.

### goose (a human session)

Either add the hub group the session already uses, or point an extension straight
at it — the same shape as any other streamable-HTTP MCP extension:

```yaml
extensions:
  a2a-agents:
    type: streamable_http
    uri: http://nas:8092/a2a-mcp/mcp
    enabled: true
    timeout: 300
```

The hub route is the better default: it is one place to change, and every session
that already has a group gets the agent tools without editing config.

## Tests

The dev box this repo is worked on has no working Python packaging, so the tests
run in the image they ship in:

```bash
cd /volumeUSB1/usbshare/docker/a2a-mcp
sudo docker run --rm -v "$PWD/test_server.py":/app/test_server.py -w /app \
    a2a-mcp:latest python -m unittest -v test_server        # 27 tests
```

They cover the three things the design leans on: addressing (`/a2a` → `/v1/agents`
is derived, not configured), naming (`the nas agent` → `nas-goose`, and an
ambiguous name is an error, not a coin toss), and parsing (the captured live turn,
second empty artifact and all). `ThroughTheWire` finishes the job: it drives
`ask_agent` over real HTTP against a stub proxy and asserts the request that left
— the path, the 1.0 method, `ROLE_USER`, the text, the `contextId`, the bearer.

Then, against the live thing:

```bash
sudo docker run --rm --network ai_proxy \
    -v "$PWD/smoke.py":/app/smoke.py a2a-mcp:latest python /app/smoke.py
# and through the hub, exactly as a goose session gets it:
sudo docker run --rm --network ai_proxy -e MCP_URL=http://mcphub:3000/mcp/core \
    -v "$PWD/smoke.py":/app/smoke.py a2a-mcp:latest \
    python /app/smoke.py "the nas agent" "Which host runs you? One word."
```

## Measured (NAS, 2026-09-17)

| What | Result |
| ---- | ------ |
| 27 tests in the image | `OK` |
| `smoke.py` direct to the server | `a2a-agents 1.27.2`; tools `['list_agents', 'ask_agent']`; `ask_agent('the nas agent', …)` → `NAS` (14.7 s, cold) |
| `smoke.py` through `/mcp/core` | `mcphub_core_group 1.0.38`; tools `memos-*`, `fetch-fetch`, `a2a-list_agents`, `a2a-ask_agent`; a turn → `hub` |
| mcphub | connected, 2 tools, added to all 9 groups (`core`, `media`, `container`, `llm-cost`, `dev`, `ops`, `dev-ui`, `doppler`, `vikunja`) |
| Traefik | `GET http://100.82.223.13:8092/a2a-mcp/mcp` → `406` — routed and answered by the server, which is POST-only by design |
| Open WebUI | its MCP client connects to `http://mcphub:3000/mcp/core` and lists the tools; mcphub's activity log records `a2a.list_agents` (128 ms) then `a2a.ask_agent` (2.1 s), `status=success`, `group_name=core` — from Open WebUI chats on the `core` model |
| CI publishes, the box updates itself | Build 92 (`:docker: Build and publish image (GHCR, a2a-mcp)`) passed; watchtower-nick's following poll logged `Found new image container=a2a-mcp new_id=3efcb7d873d3`, `Stopping container`, `Started new container`, `Removing image image_id=3a0a17503fe4`, `updated=1` — with no hand on the box. Its scan count had gone 15 → 16 when the scope label was added, while the unscoped nightly instance's container list still excluded `a2a-mcp` |

## Caveats

- **It inherits the M4 hole.** Like the bridge, this reaches the agent through
  LiteLLM's route, so it depends on the agent row's `static_headers` carrying the
  bearer. On the NAS that is currently a hand-made admin `PUT`; nothing in the
  repo restores it. `registry.rs` owes that, and now three surfaces depend on it.
- **A tool call blocks for the whole turn, and now says so.** `TIMEOUT_SECONDS`
  is 300 s, and both tool descriptions, `list_agents`' footer and the server's
  `instructions` carry that number and the advice that goes with it: these calls
  are for **relatively short-lived work**. Each agent line in `list_agents` also
  carries the ceiling that agent's *own* card advertises
  ([`docs/turn-deadline.md`](../../docs/turn-deadline.md)), because a caller that
  can only see one of the two numbers is guessing. That number is fetched from the
  agent itself — the row cannot supply it, because LiteLLM stores a *normalised*
  card and drops `capabilities.extensions` (measured 2026-09-17: a registered
  a2a-goose row reads back `capabilities: {"streaming": true}` while the agent's
  own card carries the extension) — and the fetch fails open, so an agent that is
  down costs the roster that line and nothing else. The caller's ceiling is usually
  lower still (Open WebUI's bridge valve defaults to 180 s; mcphub's own tool-call
  timeout is still unmeasured), so the number advertised is the one this process
  enforces, and the guidance is about *what to send*. A call that gives up does
  **not** cancel the turn — measured, the agent carries on with nobody listening
  — so `ask_agent` now returns that fact as an answer instead of an exception,
  and points at the durable workaround: ask for the result to be written down
  and collect it on a later call with the same `context_id`. What the deadline is
  **not** is a warm-up: a new conversation on a running agent answers in ~2 s, the
  first turn after a restart in ~2 s, three simultaneous turns in ~3 s
  (`spikes/S18.md`). Size for the task, not for the agent waking up.
- **A turn is unary.** Nothing streams; `SendStreamingMessage` exists on the agent
  and the route would forward it, but relaying a stream is unmeasured.
- **The parse-and-call code is a copy of the bridge's**, not an import. The bridge
  must stay a single file that installs through Open WebUI's UI, so a shared
  package would break exactly that. It is two small functions; the tests on both
  sides are what keep them honest.
- **FastMCP reads a `.env` from the working directory.** Not a problem in the
  image (`/app` has no `.env`), but it is why the test command mounts
  `test_server.py` at `/app` rather than running from the deployment directory,
  where the `.env` is 0600 and owned by another user.
- **The SDK's DNS-rebinding guard is switched off deliberately.** It is a host
  allowlist, and an allowlist would have to name every container name and proxy
  hostname that reaches this port — the same maintenance trap as two URLs that
  must agree. The bind and the Docker network are the boundary.
