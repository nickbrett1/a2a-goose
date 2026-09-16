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
#   Script      /volume1/homes/nick/a2a-goose/deploy/dsm/a2a-goose-boot.sh
#
# Verify with S8 (does the agent actually come back after a DSM reboot, and does
# it survive a DSM update) before trusting any of this. S8 has not been run yet.
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

# Where this host keeps its checkout of the repository. Overridable so the same
# script works on a host whose paths differ; the launcher is a repository file,
# not a release asset.
A2A_GOOSE_REPO="${A2A_GOOSE_REPO:-/volume1/homes/nick/a2a-goose}"
LAUNCHER="${A2A_GOOSE_REPO}/scripts/fetch-launch.sh"

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
  log "no launcher at ${LAUNCHER} - check A2A_GOOSE_REPO"
  exit 1
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
