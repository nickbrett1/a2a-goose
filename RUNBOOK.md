# Runbook — operating a fleet of agents

What to do when something about a host's agent needs changing, and the three
framings that keep being asked for. `LAUNCHING.md` is how a host starts,
`deploy/README.md` is why it is shaped that way, `RELEASING.md` is how code
reaches a host. This file is the operator's side: the actions, in the order they
have to happen, with what was actually measured.

Everything here assumes the host is deployed as `deploy/` describes — launcher at
a fixed path, `goose.acp.serve: own`, host-local values in `ENV_FILE`
(`$HOME/.config/a2a-goose/env`, mode `0600`).

## Rotating a host's agent bearer

`A2A_GOOSE_BEARER_TOKEN` authenticates *the agent's own surface*. Since v0.1.35
registration carries it to LiteLLM as the row's top-level `static_headers`, which
is the only header LiteLLM's `/a2a/{agent_id}` route presents — so **the row is
no longer something a human edits**, and a rotation is: change the value in
`ENV_FILE`, restart the payload, done.

Measured 2026-09-17 on the NAS (`v0.1.35`), rotating a token that had leaked into
session transcripts:

| Step | What happened |
| ---- | ------------- |
| backup | `env.bak-rot2-20260917T202241Z` — a copy of the file, mode `0600` |
| leak scan | 225 config-shaped files across the env dir, the deploy checkout and the docker tree (`*.yml/*.yaml/*.json/*.conf/.env*/*.sh`, depth ≤ 3 for the docker tree) — the old token was in `env` and its backups, **nowhere else**, so nothing besides registration consumes it |
| rotate | one line rewritten in place, `bash -n` clean, still sources with the variable set, mode still `0600`, old value gone |
| restart | `SIGTERM` to the payload → `deregistered from LiteLLM fd34ec17-…` (20:23:59Z) → wrapper relaunched the launcher → `registered with LiteLLM 51f7c550-…` (20:24:45Z) |
| verified | the row created at registration carries `Authorization` = `"Bearer <the new env token>"` (`sha256[0:16] 4196f2d9659ee06c`) and is *not* the pre-rotation value; turns answered through `/a2a/{id}` afterwards |

The sequence to run, per host:

1. `cp -p "$ENV" "$ENV.bak-rotate-$(date -u +%Y%m%dT%H%M%SZ)"` and check the copy
   is `0600`.
2. Fingerprint, never print: `printf %s "$A2A_GOOSE_BEARER_TOKEN" | sha256sum | cut -c1-16`.
3. `openssl rand -hex 32`; confirm the file has exactly **one**
   `export A2A_GOOSE_BEARER_TOKEN=` line before rewriting it.
4. Rewrite that line, `chmod 600`, check `bash -n` and that a throwaway
   `set -a; . "$ENV"; set +a` still sets the variable.
5. `SIGTERM` the payload so the wrapper's loop relaunches the launcher — see the
   traps below.
6. Verify from *outside* the host — the row, and a turn — rather than believing
   the host's own report.

### Traps, all measured

- **Never `SIGKILL` the payload.** `SIGTERM` lets it stop the `goose serve` it
  started; a `SIGKILL` orphans that goose, and the next payload refuses to adopt
  it, so the host stays down until a human or a reboot clears it (S8 §5).
- **A shutdown is not instant.** The measured `SIGTERM` took **69 seconds** to
  reach `stopped the goose serve this agent started` — so budget ~2 minutes from
  signal to a registered agent: shutdown, the wrapper's 30 s restart delay, the
  launcher's fetch, then goose's own startup (~1 s here).
- **The token must not touch a command line.** A `grep -rlF "$TOKEN"` puts the
  secret in that process's `argv`, where `ps` shows it to every user on the box —
  observed on this run. Pass secrets through a file, stdin or an environment
  variable, and read them in a language that need not echo them.
- **A full-content scan of a media tree does not terminate in useful time.** The
  first scan attempt walked `/volumeUSB1/usbshare/docker` with a 2 MiB per-file
  cap and never finished, which is what turned this into the run that found the
  next trap. Restrict to config-shaped names, bound the depth, and give the scan
  a deadline (`SCAN_DEADLINE = 60` in the script this run used), reporting
  partial scans *as* partial.
- **One rotator at a time.** Two overlapping runs each rotated the file; the
  later write won and the payload started on the winner's value, so the outcome
  was still consistent — but only because the last writer happened to run before
  the restart. Nothing enforces that, and the loser's value is history.
- **`pgrep` does not exist on DSM.** Use `ps -o pid,ppid,args -w | grep …`, and
  identify the payload by its path plus the wrapper parent (the wrapper is the
  `a2a-goose-boot.sh` process with ppid 1).

### Housekeeping this run exposed

The backups an operator makes are copies of a *leaked* secret, and the oldest one
(`env.bak-s9reach`) is mode `0777`. Purge backups once a rotation is verified —
keep at most the most recent, `0600` — and treat any world-readable file under
`~/.config/a2a-goose/` as a finding in its own right.

## The three framings that keep being asked for

**An agent is a model in the dropdown, and will never be in `GET /v1/models`.**
LiteLLM's model list is a configured artefact; an A2A agent is a row in
`/v1/agents`. Open WebUI surfaces one as a model only because a Pipe function
synthesises it (`integrations/openwebui/`, S15 §7) — and that is also why the
entry appears only while the function is active. Asking "why isn't my agent in
the model list" is asking the wrong list.

**An agent is a tool everywhere else.** `integrations/a2a-mcp/` exposes
`list_agents` and `ask_agent` as MCP tools, registered in mcphub and therefore
present in every group — so a goose session, an Open WebUI chat and any other MCP
client reach the roster the same way they reach `memos` (S16). The roster is read
live per call, so a newly registered host appears with no code change and no
deploy.

**Attribution is one header, not a budget.** `LITELLM_CUSTOM_HEADERS` in
`ENV_FILE` is `Name: value` lines (JSON spelling makes goose exit
`invalid HTTP header name`); it lands as `metadata.user_agent` on every spend-log
row, which is what makes "which host did this" answerable. No budget is attached
anywhere, deliberately (S2).

**A turn can outlive its caller.** Measured on this run: a request through the
hub timed out (`MCP error -32001`) after several minutes while the agent was
still working, and the agent kept going. Tool-call timeouts against a
multi-minute turn are the open item (S16 §5) — until they are settled, treat
"the tool call failed" and "the agent failed" as different claims.
