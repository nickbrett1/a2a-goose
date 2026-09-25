#!/usr/bin/env bash
#
# ensure-hub.sh - make this host a member of the roost fleet, idempotently.
#
# WHY THIS EXISTS (the outage it ends). An agent appears in roost mission
# control ("the fleet") only by dialling the hub at ws://nas:3008/agent/ws,
# which needs two things the devcontainer path already wrote and the
# host-process path did not:
#
#   1. a `hub:` block in the agent's config.yaml, and
#   2. A2A_GOOSE_HUB_TOKEN in ENV_FILE - the credential that block names.
#
# mac-studio, 2026-09-25: running a2a-goose from launchd since 2026-09-17 with
# neither. It registered with LiteLLM on every start (so it looked healthy) but
# never dialled roost, and the installed binary (0.1.41) predated the hub code
# entirely - `strings <bin> | grep -c 'roost tunnel'` was 0. It joined the fleet
# only after the block and the token were added AND the launchd job was
# restarted; the restart, not the edit, is what pulled a current release. So:
# this script wires the files, and deploy/README.md insists a host is restarted
# afterwards, never merely edited.
#
# These two files are HAND-AUTHORED on a host, not generated, so this script
# NEVER regenerates them and never rewrites a value that is already there. It
# only ADDS what is missing, appends nothing when the thing is present, and
# copies the file aside before it touches it. Run it as the user that owns
# goose; re-run it freely, because the second run is a no-op:
#
#   deploy/ensure-hub.sh
#
# Overridable:
#   A2A_GOOSE_CONFIG       default: $HOME/.config/a2a-goose/config.yaml
#   ENV_FILE               default: $HOME/.config/a2a-goose/env
#   A2A_GOOSE_HUB_URL      default: ws://nas:3008/agent/ws
#   A2A_GOOSE_HUB_ENABLED  set to "false" to opt this host out entirely
#
# Exit status: 0 when the host is wired (or already was); non-zero when the
# config it must edit is absent or cannot be backed up - a host with no agent
# config cannot be an agent at all, so silence there would be the wrong answer.
set -uo pipefail

HUB_URL="${A2A_GOOSE_HUB_URL:-ws://nas:3008/agent/ws}"
HUB_ENABLED="${A2A_GOOSE_HUB_ENABLED:-true}"
HUB_CREDENTIAL_ENV="A2A_GOOSE_HUB_TOKEN"

CONFIG_FILE="${A2A_GOOSE_CONFIG:-${HOME}/.config/a2a-goose/config.yaml}"
ENV_FILE="${ENV_FILE:-${HOME}/.config/a2a-goose/env}"

log() {
  printf '%s ensure-hub: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
}

# A visible, multi-line warning. Reserved for the things a human has to read.
loud() {
  printf '\n============================================================\n' >&2
  for line in "$@"; do printf '  %s\n' "$line" >&2; done
  printf '============================================================\n\n' >&2
}

# Copy before edit, with a UTC stamp and the pid so two runs in the same second
# do not fight over one name. Refusing to edit when the copy fails is the whole
# point: an unbacked edit to a hand-authored file is the thing we must not do.
backup() {
  local file="$1" dest
  dest="${file}.bak-$(date -u '+%Y%m%dT%H%M%SZ')-$$"
  if cp -p "${file}" "${dest}" 2>/dev/null; then
    log "backed up ${file} -> ${dest}"
    return 0
  fi
  log "could not back up ${file} - refusing to edit it"
  return 1
}

# True when the file is non-empty and its last byte is a newline. Used so an
# append never welds itself onto the previous line.
ends_with_newline() {
  [ -s "$1" ] || return 1
  [ "$(tail -c 1 "$1" | wc -l)" -gt 0 ]
}

if [ "${HUB_ENABLED}" = "false" ]; then
  log "A2A_GOOSE_HUB_ENABLED=false - not wiring the hub on this run"
  exit 0
fi

# --- config.yaml: the hub block ------------------------------------------------
if [ ! -f "${CONFIG_FILE}" ]; then
  loud "No config at ${CONFIG_FILE} - this host is not deployed yet." \
    "ensure-hub.sh deliberately does not create an agent config: config.yaml is" \
    "hand-authored on a host (deploy/README.md), and a generated one would put" \
    "the wrong bind address and card on the wire. Deploy the agent first, then" \
    "re-run this."
  exit 1
fi

if grep -qE '^hub[[:space:]]*:' "${CONFIG_FILE}"; then
  log "hub block already present in ${CONFIG_FILE} - leaving it alone"
else
  backup "${CONFIG_FILE}" || exit 1
  {
    ends_with_newline "${CONFIG_FILE}" || printf '\n'
    printf '\n'
    cat <<YAML
# Added by deploy/ensure-hub.sh: dial the roost hub so this host appears in
# mission control. Field names are case-sensitive (the config is camelCase).
hub:
  enabled: true
  url: "${HUB_URL}"
  credentialEnv: "${HUB_CREDENTIAL_ENV}"
  kind: "a2a-goose"
  connectTimeoutSecs: 10
  idleTimeoutSecs: 90
YAML
  } >>"${CONFIG_FILE}"
  log "appended the hub block to ${CONFIG_FILE}"
fi

# --- ENV_FILE: the credential the block names ----------------------------------
# A hub credential is the entire auth boundary for a fleet agent (there is no
# browser login), and the client refuses to dial at all when credentialEnv is
# empty - so an enabled hub with no token is a silent non-registration rather
# than a refused start. The value is a SECRET: this script only guarantees the
# KEY is present; the operator supplies the value.
if [ -f "${ENV_FILE}" ] &&
  grep -qE '^[[:space:]]*(export[[:space:]]+)?'"${HUB_CREDENTIAL_ENV}"'=' "${ENV_FILE}"; then
  log "${HUB_CREDENTIAL_ENV} already present in ${ENV_FILE} - leaving it alone"
else
  mkdir -p "$(dirname "${ENV_FILE}")" 2>/dev/null || true
  [ -f "${ENV_FILE}" ] && { backup "${ENV_FILE}" || exit 1; }
  {
    if [ -f "${ENV_FILE}" ]; then
      ends_with_newline "${ENV_FILE}" || printf '\n'
      printf '\n'
    fi
    cat <<ENV
# Added by deploy/ensure-hub.sh: the credential the hub block names, sent as
# \`Authorization: Bearer ...\` on the WebSocket handshake. The real value is a
# secret and lives in Doppler - project "goose", config "prd", key
# ${HUB_CREDENTIAL_ENV}. Replace the placeholder below with it (or paste the
# value this host already keeps in its secret store).
export ${HUB_CREDENTIAL_ENV}="REPLACE_ME"
ENV
  } >>"${ENV_FILE}"
  chmod 600 "${ENV_FILE}" 2>/dev/null || true
  log "added ${HUB_CREDENTIAL_ENV} to ${ENV_FILE} (mode 0600)"
fi

# Say out loud the one state that looks fine and is not: a placeholder token.
# The agent will still start and register with LiteLLM, so nothing downstream
# reports a problem - only the fleet view is empty.
token_value="$(sed -nE "s/^[[:space:]]*(export[[:space:]]+)?${HUB_CREDENTIAL_ENV}=[\"']?([^\"']*)[\"']?.*/\2/p" "${ENV_FILE}" | tail -1)"
if [ -z "${token_value}" ] || [ "${token_value}" = "REPLACE_ME" ]; then
  loud "The hub credential in ${ENV_FILE} is still a placeholder." \
    "The agent will start and register with LiteLLM, but the hub client refuses" \
    "to dial with no credential - so it will NOT appear in the fleet." \
    "Fill ${HUB_CREDENTIAL_ENV} from Doppler (project goose, config prd), then" \
    "restart the host so the launcher fetches a current release."
else
  log "${HUB_CREDENTIAL_ENV} is set in ${ENV_FILE}"
fi

log "done - restart the host (not just a config edit) so the launcher fetches a current release"
