#!/bin/bash
#
# The DSM 7 half of the host-process deployment. genproj emits no deploy/, so
# this file is authored here (phase-1 plan, section 2.2).
#
# Different from macOS for one reason: DSM has no launchd. There is nothing on
# the box that will restart a process, so this script is both halves - the boot
# trigger (DSM's Task Scheduler runs it) and the restart loop that launchd's
# `KeepAlive` provides on the Mac. That is a Task Scheduler plus a wrapper, which
# is what the plan sanctions for DSM; it is not a supervisor daemon, and nothing
# else on the host is supervised by it (hard constraint #9).
#
# Install: Control Panel -> Task Scheduler -> Create -> Triggered Task
#
#   Task name   a2a-goose
#   User        the user that owns goose's own configuration - NOT root.
#               goose keeps its config, recipes and sessions under $HOME, and
#               the recipes under that $HOME are the skill library this project
#               mines. As root you would mine an empty recipe directory and
#               serve a card with one skill in it.
#   Event       Boot-up
#   Script      one line, below - not this file's path, and not its body
#
# The task's whole body is the detach below, because the body runs
# *synchronously* during the boot event and this script never returns (S8,
# 2026-09-17: a 20-second task took 21 seconds, and
# esynoscheduler-bootup.service is a `oneshot` with `TimeoutStartSec=0`). A task
# that ran the loop in the foreground would hold the boot-up event open forever
# and starve every other boot-up task on the box - including whichever one
# starts Tailscale. So the task detaches the loop and returns:
#
#   nohup /volume1/homes/nick/a2a-goose/deploy/dsm/a2a-goose-boot.sh \
#     >> /volume1/homes/nick/.local/share/a2a-goose/boot.log 2>&1 &
#
# The same task can be created without the GUI: DSM keeps event-driven tasks in a
# sqlite database and ships a CLI for them. Measured on DSM 7.4.1 -
#
#   sudo /usr/syno/sbin/esynoscheduler --create task_name=a2a-goose event=bootup \
#     'owner={"1026":"nick"}' enable=true operation_type=script \
#     'operation=<the nohup line above>'
#
#   --list / --get / --run / --delete take `task_name=<name>`, and owner is a
#   JSON object mapping uid to user name: the uid carries the identity, the
#   environment does not (see the HOME note below). `--run` fires it immediately,
#   which is how S8 started the loop without rebooting the box.
#
# What this design does not detect is the loop itself dying. launchd restarts a
# job that exits; nothing here restarts the wrapper. It is thirty lines of bash
# in a `while :` loop whose only exit is a missing launcher, so the realistic
# exposure is "a reboot fixes it" rather than "it dies quietly under load" - but
# it is a real difference in kind from the macOS deployment, so it is written
# down here instead of being discovered later.
#
# S8 ran this on the box (v0.1.25, 2026-09-17): the task fires, the loop is
# started, the payload and goose come up under it, a SIGTERM to the payload is
# restarted, and the whole thing comes back from a **real reboot** - one wrapper,
# ppid 1, 2m27s from kernel boot to /healthz 200, with /volume1 mounted before the
# boot-up task ran (esynoscheduler-bootup.service is After=basic.target). One
# limit remains: a SIGKILLed payload leaves an orphaned goose the payload refuses
# to adopt, so that is a human's or a reboot's to fix. spikes/S8.md has the
# evidence, and §5 there records the one fault the reboot exposed that this script
# cannot fix - the agent coming back healthy but permanently unregistered when the
# proxy starts late (a registration-path bug, not a host-process one).
#
# What is run is the LAUNCHER, not the agent (hard constraint #14): the fetch
# happens first, and the launcher ends in `exec`, so the payload replaces this
# shell and the loop below restarts it when it exits.
#
# The agent starts and stops its own `goose serve`, under this: the agent owns
# goose (goose.acp.serve: own), and this loop owns the agent. One restart
# mechanism per level. A goose this agent did not start is a refusal at boot,
# not something to adopt - so if this host ran goose by hand before, stop it
# before the first run of a payload that owns goose.
set -uo pipefail

# $HOME is NOT the user's home here, and this is the one thing about DSM that
# costs an afternoon. The Task Scheduler runs a task as the user set on it, but
# hands it *root's* environment: measured on DSM 7.4.1 (S8, 2026-09-17), a
# boot-up task owned by uid 1026 reported HOME=/root, USER=root, LOGNAME=root
# while `id` reported the owner. A deploy dir built from that $HOME is
# /root/.local/share/a2a-goose, the launcher is "not found" at every boot, and
# the goose the payload spawns looks for /root/.config/goose - which is exactly
# the empty recipe directory hard constraint #9's "the owner, not root" is about,
# reached with the right uid and the wrong home.
#
# So ask passwd for the invoking user's home (`id -un` is the owner; only the
# environment lies), and *export* it: the launcher reads $HOME/.config/a2a-goose/env
# on its way to the exec, and the payload and the goose under it inherit this
# environment. A2A_GOOSE_HOME overrides, for a host whose paths differ.
if [ -n "${A2A_GOOSE_HOME:-}" ]; then
  user_home="${A2A_GOOSE_HOME}"
else
  user_home="$(awk -F: -v u="$(id -un)" '$1 == u { print $6; exit }' /etc/passwd 2>/dev/null)"
  [ -n "$user_home" ] || user_home="${HOME:-}"
fi
if [ -n "$user_home" ]; then
  export HOME="$user_home"
fi

# Where this host keeps the launcher and the releases it fetches. The launcher
# is a release asset, not a file in a checkout (S11): the cold start in
# LAUNCHING.md puts it here, and every start it replaces itself with whatever the
# newest release advertises. Overridable so the same script works on a host whose
# paths differ.
A2A_GOOSE_DEPLOY_DIR="${A2A_GOOSE_DEPLOY_DIR:-${HOME}/.local/share/a2a-goose}"
LAUNCHER="${A2A_GOOSE_DEPLOY_DIR}/fetch-launch.sh"

# The hub wiring, made idempotent. An agent only appears in roost mission
# control if its config.yaml carries a `hub:` block and its ENV_FILE carries
# A2A_GOOSE_HUB_TOKEN; the devcontainer path wrote both, the host-process path
# did not, and mac-studio ran for a week registered with LiteLLM but invisible
# in the fleet as a result (2026-09-25). ensure-hub.sh sits beside this file and
# ADDS only what is missing - it never regenerates the hand-authored config or
# env - so running it on every start is safe and self-healing. It is asked for
# by path relative to this file, so it travels with the checkout this wrapper is
# already taken from.
#
# This is the wiring half only. A host also has to be *restarted* to pick up a
# release that contains the hub code: the launcher fetches on every start, so
# the restart below (or the next boot) is what upgrades a stale payload. That is
# why deploy/README.md says restart the host, never just edit the config.
SCRIPT_DIR="$(cd -- "$(dirname -- "$0")" 2>/dev/null && pwd)"
ENSURE_HUB="${A2A_GOOSE_ENSURE_HUB:-${SCRIPT_DIR}/../ensure-hub.sh}"

# Seconds between restarts. Long enough that a payload which dies instantly does
# not spin, short enough that a transient failure is not a night-long outage.
RESTART_DELAY="${RESTART_DELAY:-30}"

log() {
  printf '%s a2a-goose-boot: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
  # DSM's Log Center reads syslog, so a Task Scheduler run leaves a trace a
  # human can find without watching a terminal nobody is looking at.
  command -v logger >/dev/null 2>&1 && logger -t a2a-goose "$*"
}

if [ ! -x "$LAUNCHER" ]; then
  log "no launcher at ${LAUNCHER} - run the cold start in LAUNCHING.md, or set A2A_GOOSE_DEPLOY_DIR"
  exit 1
fi

# Wire the hub before the first start (and on every restart, since this whole
# script is re-run at boot). A failure here is not fatal to booting - the agent
# still serves turns - but it means no fleet membership, so it is logged rather
# than swallowed. ensure-hub.sh is idempotent: on a host already carrying the
# block and the token this is a no-op.
if [ -x "$ENSURE_HUB" ]; then
  "$ENSURE_HUB" ||
    log "ensure-hub.sh failed - starting anyway; a host with no hub block or token will not appear in the fleet"
else
  log "no ensure-hub.sh at ${ENSURE_HUB} - hub wiring not confirmed"
fi

# The launcher sources ENV_FILE ($HOME/.config/a2a-goose/env, mode 0600) itself,
# so nothing host-local is spelled out here: no bearer token, no LITELLM_BASE_URL,
# no bind address. Secrets stay in one file per host.
while :; do
  log "starting ${LAUNCHER}"
  "$LAUNCHER"
  status=$?
  # Under `exec` only a failed start ever reaches here: a clean exit is the
  # payload deciding to stop. Either way the answer is the same - come back.
  log "launcher exited with ${status} - restarting in ${RESTART_DELAY}s"
  sleep "$RESTART_DELAY"
done
