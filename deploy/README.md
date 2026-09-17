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

The launcher is itself a **release asset** (S11): the cold start above installs
it, and every start it replaces itself with whatever the manifest advertises —
verify the `sha256`, parse-check it, rename it over its own path, fail open. An
init unit therefore names a fixed path that nothing else owns, not a checkout.
The DSM *wrapper* is the one file still taken from a checkout, because it is the
init glue itself; it is thirty lines and changes rarely, which is the property
the launcher did not have.

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
# 1. Cold start: the launcher is a release asset, not a file in a checkout. It
#    maintains itself from here on, so this path never changes again.
dir="$HOME/.local/share/a2a-goose"; mkdir -p "$dir"
curl -fsSL https://github.com/nickbrett1/a2a-goose/releases/latest/download/fetch-launch.sh \
  -o "$dir/fetch-launch.sh"
chmod +x "$dir/fetch-launch.sh"

# 2. The agent, supervised. It starts and supervises goose itself.
mkdir -p ~/Library/LaunchAgents ~/Library/Logs/a2a-goose
cp deploy/launchd/com.nick.a2a-goose.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/com.nick.a2a-goose.plist
launchctl print gui/"$(id -u)"/com.nick.a2a-goose | head
tail -f ~/Library/Logs/a2a-goose/launcher.log
```

The plist names `/Users/nick/.local/share/a2a-goose/fetch-launch.sh`; on a host
with a different user, change that one path in the copy. It must not point back
at a checkout — a launcher that is a repository file is a launcher that goes
stale (S11), and the self-update cannot replace a file something else owns.

DSM 7, as the user that owns goose:

The task's script is **one line**, not this file's path, and the file lives in a
*checkout* — the only deployment file that does (the launcher is a release asset,
S11). Create the boot-up task from the GUI (Control Panel → Task Scheduler →
Create → Triggered Task, event **Boot-up**, user = the user that owns goose's
configuration — **not** root) or from the CLI, which is what S8 measured:

```bash
sudo /usr/syno/sbin/esynoscheduler --create task_name=a2a-goose event=bootup \
  'description=a2a-goose node agent (host process, DSM)' \
  'owner={"1026":"nick"}' enable=true operation_type=script \
  'operation=nohup /volume1/homes/nick/a2a-goose/deploy/dsm/a2a-goose-boot.sh \
     >> /volume1/homes/nick/.local/share/a2a-goose/boot.log 2>&1 &'
```

The `nohup … &` is not decoration: the boot-up event runs its tasks
*synchronously*, so a body that ran the wrapper's loop in the foreground would
hold the event open and starve every other boot-up task on the box. Both the
identities — the uid in `owner`, the path in the operation — are the NAS's; see
the script's header for the two traps (`synoschedtask` cannot create tasks at
all; the task's environment is root's, whichever uid owns it).

## Before trusting either one

- **S8** — does the agent come back after a DSM reboot, and survive a DSM update?
  **Run** (2026-09-17, v0.1.25): the task fires, the loop starts, the payload and
  goose come up, a killed payload is restarted, and a cold box comes up through
  the real boot-up event — one wrapper, owned by init. Two limits, both recorded
  in `spikes/S8.md`: a real reboot has not been run, and a **`SIGKILL`ed** agent
  *does* leave a goose behind — the restarted payload refuses to adopt it (loudly,
  correctly, every 30 s) and the host stays down until a human kills the orphan or
  the box reboots.
- **S12** — does the published binary actually `exec` on both hosts (musl/static
  on DSM, the ad-hoc signature on a downloaded Darwin binary)? **Run on DSM**
  (2026-09-17, v0.1.25): the x86_64 musl static-PIE payload execs, `check-goose.sh`
  passes against the host's goose, the version refusal precedes the bind, and
  `/status` names the host's real goose. It also produced the deployment's first
  host-specific trap: the published binary is musl, and musl matches `/etc/hosts`
  names exactly where glibc matches them case-insensitively, so DSM's own
  uppercase `NAS` line is invisible to it — `LITELLM_BASE_URL` is an address on
  that host, not a name (`spikes/S12.md`).
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
LITELLM_CUSTOM_HEADERS='User-Agent: a2a-goose/mac-studio'
```

LiteLLM records that as `metadata.user_agent` on every spend-log row, which is
what makes "which agent did this" answerable — with one shared master key, every
row is otherwise filed under `litellm_proxy_master_key`. **Attribution only: no
budget is attached anywhere, deliberately** (the decision is in `spikes/S2.md`).

That variable is `Name: value` lines, **not JSON**: a JSON value makes goose exit
`Error invalid HTTP header name`, after which every turn fails with `-32603 …
"Provider not set"`. Measured on the NAS, 2026-09-17 — the table is in
`deploy/env/README.md`. Nothing written down here had it right before that run.
The agent refuses both bad spellings at startup, before it binds a port, so a host
deployed with one does not come up at all.

The env file is also where a host's identity belongs rather than the code,
because the agent is one-per-host and `cwd` is the namespace: the host is the
unit, so it sets its own name once at deploy.

Both halves of the mechanism are no longer merely proven separately: on the NAS
(v0.1.25, goose 1.50.0) a real turn came back `TASK_STATE_COMPLETED` and the row
LiteLLM wrote for it carried `metadata.user_agent = 'a2a-goose/nas'` (S12). The
same run found the second requirement — `ENV_FILE` is also the only place the
*child* goose's provider key can come from, since the agent starts `goose serve`
itself and it inherits that file's environment and nothing else.
