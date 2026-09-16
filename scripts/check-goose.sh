#!/usr/bin/env bash
#
# Verify this host's goose. Verify, never install (hard constraint #8): every
# host already has goose with its own MCP config and recipes, and installing a
# second copy would fork the configuration this project depends on.
#
# Run it from a deploy unit before the agent starts, or by hand. Exit 0 means a
# usable goose was found. Any other exit means the agent must not start.
#
# The same policy is enforced in the binary's first act (see src/goose.rs),
# because the release payload is unpacked away from this repository and cannot
# call this script. Keep MIN_GOOSE_VERSION in step with that file.
#
# Portable on purpose: bash, awk and grep only - no `sort -V`, no `readlink -f`,
# no GNU-only flags. The macOS host and DSM both have to run it.
set -uo pipefail

MIN_GOOSE_VERSION="${MIN_GOOSE_VERSION:-1.50.0}"

log() { printf '%s check-goose: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; }
die() { log "$*"; exit 1; }

# `goose` has to be an executable file: a shell function or an alias is not
# something launchd or DSM's Task Scheduler can exec, and the agent has to be
# able to spawn it too.
find_goose() {
  if [ -n "${GOOSE_BIN:-}" ]; then
    [ -x "${GOOSE_BIN}" ] || die "GOOSE_BIN=${GOOSE_BIN} is not an executable file"
    printf '%s\n' "${GOOSE_BIN}"
    return 0
  fi
  command -v goose 2>/dev/null
}

# version_ge A B -> true when A >= B. awk rather than `sort -V`: busybox sort
# has no -V, and comparing dotted versions as strings gets 1.9 > 1.50 wrong.
version_ge() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, x, "."); m = split(b, y, ".")
    for (i = 1; i <= (n > m ? n : m); i++) {
      xi = (i <= n ? x[i] + 0 : 0); yi = (i <= m ? y[i] + 0 : 0)
      if (xi > yi) exit 0
      if (xi < yi) exit 1
    }
    exit 0
  }'
}

path="$(find_goose)"
[ -n "$path" ] || die "goose is not on PATH. Install goose for this host user - this project verifies goose, it never installs it - or set GOOSE_BIN to an absolute path."

raw="$("$path" --version 2>&1)" || die "\`$path --version\` failed: $raw"
# goose prints a bare version today, but a future release may print
# "goose 1.50.0", so scan for the version token rather than taking a field.
version="$(printf '%s\n' "$raw" | tr -c '0-9.' '\n' | grep -E '^[0-9]+(\.[0-9]+)+$' | head -1)"
[ -n "$version" ] || die "could not read a version out of \`$path --version\` (it printed: $raw)"

version_ge "$version" "$MIN_GOOSE_VERSION" ||
  die "goose $version at $path is older than the required $MIN_GOOSE_VERSION - upgrade goose on this host (this project will not do it for you)."

# The ACP server is what this project actually runs on: a goose without `serve`
# is the wrong goose, and finding that out at startup beats finding it out at the
# first A2A call.
"$path" serve --help >/dev/null 2>&1 ||
  die "goose $version at $path has no \`serve\` subcommand (the ACP server) - too old, or not a full goose installation."

log "goose $version at $path (minimum $MIN_GOOSE_VERSION)"
exit 0
