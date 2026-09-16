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
  Scheduler own restarts; eviction is the liveness sweeper's job (a separate
  service, out of scope).
- **No env file.** Every host-local value — the bearer token, `LITELLM_BASE_URL`,
  the bind address — lives in `ENV_FILE` (`$HOME/.config/a2a-goose/env`, mode
  `0600`), which the launcher sources on its way to the exec. Secrets never
  enter a release.

## Install

macOS, as the user that owns goose:

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
  Not yet run.
- **S12** — does the published binary actually `exec` on both hosts (musl/static
  on DSM, the ad-hoc signature on a downloaded Darwin binary)?
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
