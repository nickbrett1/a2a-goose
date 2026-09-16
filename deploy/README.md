# deploy

Host-process boot persistence. **genproj emits no `deploy/`** — these files are
authored here (phase-1 plan, §2.2), because the deployment channel is a GitHub
Release consumed by `scripts/fetch-launch.sh`, not an image.

Two files, one per OS, both doing the same job: run the **launcher** at boot and
restart it when it exits.

| Host | File | Restart mechanism |
| ---- | ---- | ----------------- |
| macOS | `launchd/com.nick.a2a-goose.plist` | `launchd` `KeepAlive` |
| DSM 7 | `dsm/a2a-goose-boot.sh` | DSM Task Scheduler (boot) + a wrapper loop |

## goose is a child process, and this deployment no longer starts it

There are two long-lived processes on a host, and the agent needs both: itself,
and the `goose serve` whose ACP server every turn runs on. **The agent starts
goose.** `goose.acp.serve: own` (the default) makes the payload spawn
`goose serve --host <host> --port <port>` out of its own `goose.acp.url`, wait
for `initialize` to answer, and restart it if it dies. That means:

- **The reboot hole is closed.** Before this, only the agent had a unit: after a
  reboot the launcher came back, the agent bound its port and served its card,
  and every turn failed, because nothing had started the ACP leg (mac-studio,
  2026-09-16). Now nothing binds a port until goose answers, so a goose that
  cannot start means an agent that visibly does not start.
- **A goose this agent did not start is a refusal, not a merge.** If something
  is already listening on the ACP address, the agent exits non-zero and says
  what it found. A host that has been running goose by hand must either stop it
  (`launchctl`, `kill`, or a reboot with the unit gone) or set
  `goose.acp.serve: external` and own goose itself.
- **The key comes from the ENV_FILE below.** goose reads
  `GOOSE_SERVER__SECRET_KEY` and nothing else; the agent reads the variable its
  config names (`goose.acp.secretEnv`) and hands the value to the child. Without
  a key, goose will not start unless `goose.acp.unauthenticated: true` says to
  start it unauthenticated — an explicit decision, not a default.
- **goose has to be on the unit's PATH**, which is why both units pin one: the
  child is spawned with the launcher's environment, not a login shell's.

The agent supervises goose and the init system supervises the agent. When goose
exits more than five times in a minute the agent gives up and exits non-zero,
which hands the whole thing back to launchd's `KeepAlive` / the DSM wrapper
loop — one restart mechanism per level, no second daemon.

## Why the launcher is what gets supervised

Hard constraint #14: the fetch has to happen before the agent starts. The
launcher ends in `exec`, so from the init system's point of view the launcher
*is* the agent, and "restart the agent" means "fetch, then start".

That also makes a crash-restart a possible silent upgrade: the next restart
fetches whatever the newest release is. `NO_FETCH=1` pins a host.

## What is *not* here

- **No container.** Hard constraint #7. `session/new` takes a working directory
  that has to resolve inside the *goose* process's filesystem, so the agent runs
  where goose runs — on the host, as the same user.
- **No supervisor daemon.** Hard constraint #9. `launchd` and DSM's Task
  Scheduler own restarts; the agent owns the one process it started (see above),
  and eviction is the liveness sweeper's job (a separate service, out of scope).
- **No env file.** Every host-local value — the bearer token, `LITELLM_BASE_URL`,
  the bind address — lives in `ENV_FILE` (`$HOME/.config/a2a-goose/env`, mode
  `0600`), which the launcher sources on its way to the exec. Secrets never
  enter a release.

## Install

macOS, as the user that owns goose:

If the host already runs `goose serve` by hand (as mac-studio did), stop it
first: the agent refuses to start while something is listening on the ACP
address. `pkill -f 'goose serve'`, or unload whatever unit starts it, or set
`goose.acp.serve: external` in the config and let it stay somebody else's job.

```bash
mkdir -p ~/Library/LaunchAgents ~/Library/Logs/a2a-goose
cp deploy/launchd/com.nick.a2a-goose.plist ~/Library/LaunchAgents/
# edit the checkout path in the copy if this host keeps the repo elsewhere
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/com.nick.a2a-goose.plist
launchctl print gui/"$(id -u)"/com.nick.a2a-goose | head
tail -f ~/Library/Logs/a2a-goose/launcher.log
```

DSM 7: Control Panel → Task Scheduler → Create → Triggered Task, event
**Boot-up**, user = the user that owns goose's configuration (**not** root — root
has its own `$HOME` and therefore its own, empty, recipe directory), script =
`deploy/dsm/a2a-goose-boot.sh`. Details are in the script's header.

## Before trusting either one

- **S8** — does the agent come back after a DSM reboot, and survive a DSM update?
  Not yet run. It now also owns a question of its own: the agent stops the goose
  it started on `SIGTERM`/`SIGINT`, and launchd's default is to kill a job's
  remaining processes when the job exits — but nothing here has proven that a
  **`SIGKILL`ed** agent on DSM does not leave a goose behind. The answer matters
  because the next boot would then refuse to start ("something is already
  listening there"), which is loud and correct but not self-healing.
- **S12** — does the published binary actually `exec` on both hosts (musl/static
  on DSM, the ad-hoc signature on a downloaded Darwin binary)?
- **S14** — does the agent really own goose, on a host, against the host's goose?
  Run: `spikes/S14.md` records what has been proven (start, readiness gate, the
  key, a crash and the restart, the refusal, a clean stop) and what has not (a
  DSM reboot; a host whose goose was already running).
- `scripts/check-goose.sh` must pass on the host first: an agent that cannot find
  goose refuses to start, by design.

## Host identity and per-agent attribution

`deploy/env/` holds the per-host `ENV_FILE` templates, with the reasoning in
`deploy/env/README.md`. The load-bearing line in each is the host naming itself:

```sh
LITELLM_CUSTOM_HEADERS='{"User-Agent":"a2a-goose/mac-studio"}'
```

LiteLLM records that as `metadata.user_agent` on every spend-log row, which is
what makes "which agent did this" answerable — with one shared master key, every
row is otherwise filed under `litellm_proxy_master_key`. **Attribution only: no
budget is attached anywhere, deliberately** (the decision is in `spikes/S2.md`).

The env file is also where a host's identity belongs rather than the code,
because the agent is one-per-host and `cwd` is the namespace: the host is the
unit, so it sets its own name once at deploy.

Both halves of that mechanism are proven but not yet proven *together*
(LiteLLM records the header; goose forwards `LITELLM_CUSTOM_HEADERS`) — the
end-to-end check needs a host, so it rides with **S8/S12**.
