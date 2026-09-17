# The container's own agent

**Audience: the genproj container agent.** This is the design for a genproj
capability that makes every generated devcontainer bring up and register *its
own* a2a-goose agent, so a project's agent exists wherever the project is being
worked on — not only on the two hosts that happen to run one. It is written
from the a2a-goose side, which owns the payload, the launcher and the registry
protocol; the genproj side owns the container.

Everything here is decided except where a section says **OPEN**. The five
questions carried in the plan (per-container vs per-host, bearer source,
name/card scheme, coexistence with the host-level agent, launcher reuse vs
container-native entry) are answered below, one per section.

## Why a container agent is not a violation of hard constraint #7

`deploy/README.md` says "no container", and the reason is precise: `session/new`
takes a working directory that has to resolve inside the **goose** process's
filesystem, so *the agent runs where goose runs*. On mac-studio and the NAS that
means a host process as the host user.

Inside a devcontainer, goose already runs **in the container**. The constraint
therefore says the opposite of "no agent in a container": it says the agent must
run *in the same container as goose*, and must not be a host-level agent
reaching into a container's filesystem. Per-container is not a workaround for
#7, it is what #7 requires once goose itself is containerised.

The host-level agents are unchanged. They keep the machine's identity
(`mac-studio-goose`, `nas-goose`) and the host's `deploy/` units. A container
agent is an **additional, project-scoped** agent.

## Reuse the release channel; do not build a second one

The payload a container runs is the same artifact the hosts run: `fetch-launch.sh`
from the newest GitHub Release, which fetches the manifest, verifies the target's
`sha256`, unpacks it and `exec`s `current/bin/a2a-goose` (`LAUNCHING.md`). The
release already publishes `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, so the container picks the target from `uname -m`
exactly as a host does.

This is the decision against "container-native entry" (an image with the payload
baked in): a baked image pins a version, needs its own publish job, and gives the
container a *different* update path from every host — three ways to be stale
instead of one. The launcher is the mechanism that already exists, and it
self-updates the launcher too (S11).

Cost to accept: the first start of a container needs network to GitHub. The
launcher is fail-open by design, but there is no previous release in a fresh
container, so the agent simply does not start. `post-start` must say so loudly
rather than fail the whole devcontainer — the project is still usable without an
agent.

## Identity, reachability and the registry row

| Thing | Value | Why |
| --- | --- | --- |
| `card.name` / `registry.agentName` | `<repo>-dev` | stable across restarts, so the row is reclaimed by name rather than accumulating; distinct from `<host>-goose` |
| `card.description` | `goose in the devcontainer for <repo>` | the roster is read by a model; "which of these is my project" has to be answerable |
| `server.bind` | `0.0.0.0:10001` (container-local) | the container is the boundary; nothing else on the host shares this port space |
| `server.publicUrl` | `http://<container-tailscale-name>:10001` | **must not be loopback** (startup fails on it) and must be resolvable by the LiteLLM container that dials it — the same rule as S9 |
| `goose.acp.url` | `http://127.0.0.1:3284/acp`, `serve: own` | the agent starts and supervises goose; unchanged from a host |
| `goose.defaults.cwd` | the workspace folder (`/workspaces/<repo>`) | |
| `goose.defaults.allowedRoots` | `["/workspaces/<repo>"]` | the same tight boundary as a host, and here it is *really* a container boundary |
| `goose.acp.secretEnv` | `GOOSE_SERVER__SECRET_KEY` | |

Reachability is the one hard requirement on the genproj side: **the container must
be dialable by the LiteLLM container**, which is the process that fronts every
`/a2a/{id}` turn. On this fleet that means joining the tailnet from inside the
container (this repository's own devcontainer does it: a `tailscale-state`
volume, `--cap-add=NET_ADMIN`, `--device=/dev/net/tun`). A devcontainer that only
exposes ports to the developer's host cannot be an agent. Whatever the capability
is called, it is a **dependency** of this one.

## Credentials

Three values, none of them in the repository:

1. **`A2A_GOOSE_BEARER_TOKEN`** — the agent's own bearer, one per container. The
   endpoint carries it because A2A has no authentication of its own (hard
   constraint #3). Generate per container; never share a host's.
2. **`LITELLM_MASTER_KEY`** — needed today, because registering is
   `POST /v1/agents` on LiteLLM. **OPEN, and the one thing worth doing before
   building this widely**: a master key in every devcontainer is a large grant
   for a small job. LiteLLM virtual keys can be route-scoped, so the target is a
   key that may create/delete *its own* agent row and nothing else. Until that
   exists, the capability should take the master key explicitly and say in its
   own README that it is a fleet-wide grant.
3. **`LITELLM_BASE_URL`** — `http://nas:4000` on this fleet; it belongs in the
   capability's configuration schema, not hard-coded.

Source: **Doppler**, mounted into the container (this devcontainer already mounts
`${localEnv:HOME}/.doppler`), read at start into `~/.config/a2a-goose/env` at
mode `0600`, which is the file the launcher sources before exec. Never in
`containerEnv` (it would land in `docker inspect` and in the devcontainer's git
history), never on a command line (world-readable via `ps`).

## Start, stop, and the stale row

- **Start**: `post-start` (not `post-create`) — an agent that only exists after a
  rebuild is an agent that is missing whenever the developer actually works. It
  writes the env file, then runs the launcher in the background with its output
  to `~/.local/state/a2a-goose/agent.log`.
- **Stop**: `docker stop` sends SIGTERM, and the payload deregisters on SIGTERM
  (measured: deregistered from LiteLLM, goose child stopped). But the shutdown
  includes stopping the goose it started — **8 s on macOS** — so the container
  needs `stop_grace_period` comfortably above that (30 s), or the stop becomes a
  SIGKILL and the row is left behind.
- **An unclean death leaves a stale row.** The roster will list an agent that no
  longer answers. This is tolerable and must not be papered over: the next start
  reclaims the row *by name* (`registry.rs` rewrites its own stale entry in place
  rather than failing on a taken name), so the state heals on the next boot, and a
  sweeper for the fleet is explicitly out of scope.
- **Two containers of the same repo** (two checkouts, or a rebuild overlapping a
  running container) both want `<repo>-dev`. The second start reclaims the row,
  and the first container's agent is then registered but unreachable-by-name. The
  capability's README must state that one agent per repo is the contract.

## What genproj emits

1. **A capability** (name it the way genproj names things — `container-agent`
   fits the catalog's `-`-separated ids) depending on the coding-agents
   capability and on whatever provides in-container tailnet access. Its
   configuration schema should carry at least `litellmBaseUrl`, the agent name
   suffix (default `-dev`) and the tailnet name the card should publish.
2. **`scripts/agent-dev.sh`** in the generated project — `start` / `stop` /
   `status`, app-owned like `scripts/fetch-launch.sh`, so regeneration never
   overwrites a fix. It is the one file a human runs when the agent misbehaves,
   and it is where the "no network on first start" message lives.
3. **The devcontainer changes**: the tailscale volume/tun/NET_ADMIN (from the
   dependency capability), `stop_grace_period`, and the `post-start` hook.
   `remote.autoForwardPortsSource: "process"` belongs here too — the S17 finding
   is that a printed `host:port` gets forwarded, and the container agent's own
   `10001` is exactly the kind of thing that gets printed (this repo has the fix
   in `.vscode/settings.json`).
4. **README/AGENTS text**: what the agent is called, how to ask it something, and
   that it is the project's agent rather than the machine's.

## Acceptance — what "it works" means

From outside the container, with no hand on it:

1. `curl http://<container-tailnet-name>:10001/.well-known/agent-card.json`
   returns the card, and it carries the `turn-deadline/v1` extension from
   `docs/turn-deadline.md` (`promptSecs`/`cancelSecs`).
2. The agent appears in `list_agents` (mcphub → a2a-mcp) with **its own turn
   ceiling** on its line — that read comes from the agent, not the row (#34).
3. A turn through LiteLLM's `/a2a/{agent_id}` completes and answers.
4. `docker stop` → the agent is gone from the roster; the same container started
   again reclaims the same row by name and answers again.
5. Break the network on first start: the devcontainer still comes up, and the
   message says the agent did not start and why.

## Backport to existing projects

The same capability applied to already-generated repositories. Two rules from
this repository's own history: the devcontainer change is safe to apply
mechanically (it is additive), but `scripts/` is app-owned and must never be
overwritten — a project that already has an `agent-dev.sh` has to be reported,
not clobbered. Existing projects also need the tailnet dependency first, which
means the backport is per-project and may need a rebuild, not a file copy.
