#!/usr/bin/env bash
#
# fetch-launch: install the newest release for THIS host and exec it.
#
# Run once per host start: a systemd unit, a launchd agent, or by hand. It is
# deliberately not a daemon - no polling, no timer, no rollback, no blue-green.
# One fetch, one symlink flip, one exec.
#
#   read the release manifest at a URL that never contains a version
#   -> already current: exec what is installed
#   -> otherwise: download the asset for this host's target, verify its sha256,
#      unpack it into releases/<version>/, flip the `current` symlink
#   -> exec current/bin/<name>
#
# FAIL OPEN IS THE FIRST RULE. A host must never fail to boot because GitHub was
# unreachable, the manifest was odd, or this host's target was missing from it.
# Every failure below logs and execs `current` exactly as it is found. The one
# case that cannot fail open is a cold host with nothing installed yet: there is
# nothing to exec, so it exits non-zero and says so.
#
# The escape hatch is NO_FETCH=1, which skips the fetch entirely (useful when a
# host must stay on the version it has).
#
# Layout, all under DEPLOY_DIR (default: $HOME/.local/share/a2a-goose):
#
#   releases/<version>/     an unpacked payload; the entry point is bin/<name>
#   current -> releases/<version>
#
# ENV_FILE (default: $HOME/.config/a2a-goose/env) is sourced, with `set -a`,
# immediately before the exec, so host-local configuration reaches an app
# fetched from GitHub. It is optional and it is not part of a release: keep the
# host's secrets there, never in the payload.
#
# genproj seeds this file once; after that it is yours. `scripts/` is app-owned,
# so regeneration never overwrites it - unlike .buildkite/pipeline.yml, which is
# genproj's and is rewritten on every regeneration.
#
# Flags are forwarded to the payload, so `fetch-launch --help` is the payload's
# `--help`.
set -uo pipefail # deliberately no `-e`: a failure must fall through to exec

LAUNCHER_NAME="${LAUNCHER_NAME:-a2a-goose}"
DEPLOY_DIR="${DEPLOY_DIR:-${HOME}/.local/share/a2a-goose}"
ENV_FILE="${ENV_FILE:-$HOME/.config/a2a-goose/env}"
# Manifest and assets are fetched from the same directory, so only the manifest
# URL is configured. It points at `latest`, which is exactly what makes it
# stable: no version appears in the URL, so a launcher never has to know one.
MANIFEST_URL="${MANIFEST_URL:-https://github.com/nickbrett1/a2a-goose/releases/latest/download/manifest.json}"
TIMEOUT="${TIMEOUT:-10}"

# This file's own path, so it can replace itself (see self_update). launchd and
# DSM run an absolute path, which is the case that matters; a relative `$0` is
# resolved against the working directory the caller used.
SELF=""
case "$0" in
  */*) SELF="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")" ;;
  *) SELF="" ;;
esac

CURRENT="${DEPLOY_DIR}/current"
RELEASES_DIR="${DEPLOY_DIR}/releases"
TMP_DIR="${DEPLOY_DIR}/.fetch-launch-tmp"

log() {
  printf '%s fetch-launch: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
}

# Host-local configuration, sourced (not parsed) and exported to the payload.
# Optional: most hosts never create this file, so its absence is not an event.
# Two guards, both fail-open - the file is the host's script, not ours:
#
#   - `bash -n` first, because bash *exits* a non-interactive shell on a syntax
#     error in a sourced file, `set +e` or not. A typo in the host's own env
#     file must not stop the host from starting.
#   - `set +u` for the duration, because an unset variable in it would
#     otherwise be fatal under the `set -u` above.
source_env_file() {
  [ -n "${ENV_FILE}" ] && [ -f "${ENV_FILE}" ] || return 0
  if ! bash -n "${ENV_FILE}" 2>/dev/null; then
    log "env file ${ENV_FILE} has a syntax error - starting without it"
    return 0
  fi
  set -a
  set +u
  # shellcheck source=/dev/null
  . "${ENV_FILE}"
  set -u
  set +a
}

exec_current() {
  # The env file belongs to the host, not to the release: it is where *this*
  # machine's database URL, token or port comes from, and it is read on the way
  # out so the payload inherits it. Every exit path is an exec, so this is the
  # one call site.
  source_env_file
  if [ -x "${CURRENT}/bin/${LAUNCHER_NAME}" ]; then
    exec "${CURRENT}/bin/${LAUNCHER_NAME}" "$@"
  fi
  log "nothing executable at ${CURRENT}/bin/${LAUNCHER_NAME}"
  exit 1
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo ""
  fi
}

# Replace this file with the one the manifest advertises, before the payload is
# fetched. This is what makes the launcher a release asset: without it the code
# that supervises everything else only changes when a human visits the box (a
# real host ran a launcher four commits stale while its payload self-updated
# happily underneath).
#
# THE FROZEN FLOOR IS THIS FUNCTION: read the manifest, verify a sha256, rename
# over the path, exec, fail open. Everything above that floor changes with any
# release, because the copy that runs it is the copy being replaced - so the
# floor has to be small enough to be obviously right. Two rules keep it safe:
#
#   - Verify before swapping, and fail open. A truncated download, a GitHub
#     error page, a checksum mismatch, or a file that does not parse leaves the
#     working launcher exactly where it is and the host still boots. The sha256
#     comes from the same manifest as the asset, so it guards against corruption
#     and not against a hostile release - the same trust the payload already has.
#   - Rename over the path; never write through it. bash reads a script lazily,
#     off its inode, as it runs, so rewriting this file in place would corrupt
#     the running process mid-execution. `mv` of a new file onto the name gives
#     the new inode a fresh name and leaves this process reading the inode it
#     already has open. (Same reasoning as the `current` flip below: the bug
#     there was also a `mv` doing something other than the obvious.)
#
# A manifest with no "launcher" entry - an older release, or a project with no
# release pipeline - is a no-op, not a failure.
self_update() {
  [ -n "${SELF:-}" ] || return 0
  [ -f "${SELF}" ] || return 0
  local entry file expected url new actual
  entry="$(grep '"launcher"[[:space:]]*:' "$manifest" | head -1)"
  [ -n "$entry" ] || return 0
  file="$(printf '%s\n' "$entry" | sed -n 's/.*"file"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
  expected="$(printf '%s\n' "$entry" | sed -n 's/.*"sha256"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
  if [ -z "$file" ] || [ -z "$expected" ]; then
    log "the manifest's launcher entry has no file/sha256 - keeping the launcher that is running"
    return 0
  fi
  # Downloaded beside SELF, not into TMP_DIR: the swap below has to be a rename,
  # and a cross-device `mv` would copy onto the destination instead - which is
  # the write-through this function exists to avoid.
  new="${SELF}.new.$$"
  url="$(dirname "$MANIFEST_URL")/${file}"
  if ! curl -fsSL --max-time "$TIMEOUT" -o "$new" "$url"; then
    log "could not download the launcher ${url} - keeping the launcher that is running"
    rm -f "$new"
    return 0
  fi
  actual="$(sha256_of "$new")"
  if [ "$actual" != "$expected" ]; then
    log "sha256 mismatch for the launcher ${file}: expected ${expected}, got ${actual:-<no sha256 tool>}"
    rm -f "$new"
    return 0
  fi
  if ! bash -n "$new" 2>/dev/null; then
    log "the launcher ${file} does not parse - keeping the launcher that is running"
    rm -f "$new"
    return 0
  fi
  # Already this launcher: nothing to do, and no reason to churn the file (or
  # its mtime) on every boot.
  if cmp -s "$new" "$SELF"; then
    rm -f "$new"
    return 0
  fi
  chmod +x "$new" 2>/dev/null || true
  if mv -f "$new" "$SELF"; then
    # Takes effect next start: this process is already reading the old inode,
    # and it execs the payload in a moment anyway.
    log "updated the launcher to ${version} (${file}) - it takes effect next start"
  else
    log "could not replace the launcher at ${SELF} - keeping the launcher that is running"
    rm -f "$new"
  fi
  return 0
}

# The candidate labels for this host, most preferred first. The label is LOOKED
# UP from this table, never constructed from `uname`: a concatenated triple is
# where producer/consumer drift would live, and the launcher has no business
# spelling a Rust target at all. Every host's list ends with the universal key,
# so a host with no triple of its own still finds an architecture-independent
# payload (a JS bundle, a pure-python .pyz). This table is generated from
# src/generator/target-labels.js in genproj - the same table the pipeline builds
# its targets from.
candidates() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) echo "aarch64-apple-darwin any" ;;
    Linux/x86_64) echo "x86_64-unknown-linux-musl any" ;;
    Linux/aarch64) echo "aarch64-unknown-linux-musl any" ;;
    *) echo "any" ;;
  esac
}

if [ -n "${NO_FETCH:-}" ]; then
  log "NO_FETCH is set - not fetching"
  exec_current "$@"
fi

mkdir -p "$RELEASES_DIR" "$TMP_DIR" 2>/dev/null || true
# Cleared rather than trap-cleaned: `exec` replaces this process, so an EXIT
# trap would never run on the success path.
rm -rf "${TMP_DIR:?}"/* 2>/dev/null || true

manifest="${TMP_DIR}/manifest.json"
if ! curl -fsSL --max-time "$TIMEOUT" -o "$manifest" "$MANIFEST_URL"; then
  log "could not fetch ${MANIFEST_URL} - starting what is installed"
  exec_current "$@"
fi

version="$(
  sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" |
    head -1
)"
if [ -z "$version" ]; then
  log "manifest at ${MANIFEST_URL} has no version - starting what is installed"
  exec_current "$@"
fi

# The launcher is a release asset too. Do this before the "already at" return
# below on purpose: a host whose payload is current can still be running a stale
# launcher, and that is the case this exists to end. It fails open, so every path
# through it ends in continuing as before.
self_update

label=""
for candidate in $(candidates); do
  if grep -q "\"${candidate}\"[[:space:]]*:" "$manifest"; then
    label="$candidate"
    break
  fi
done
if [ -z "$label" ]; then
  log "the manifest publishes no target for this host - starting what is installed"
  exec_current "$@"
fi

current_version=""
if [ -L "$CURRENT" ]; then
  current_version="$(basename "$(readlink "$CURRENT")")"
fi
if [ "$version" = "$current_version" ]; then
  log "already at ${version}"
  exec_current "$@"
fi

# `scripts/release-artifacts.sh` writes one asset per line, which is what makes
# this readable without a JSON parser the host may not have. The asset name comes
# from the manifest rather than from a naming convention re-implemented here: the
# only string both sides must agree on is the target key.
entry="$(grep "\"${label}\"[[:space:]]*:" "$manifest" | head -1)"
file="$(printf '%s\n' "$entry" | sed -n 's/.*"file"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
expected="$(printf '%s\n' "$entry" | sed -n 's/.*"sha256"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
if [ -z "$file" ] || [ -z "$expected" ]; then
  log "the manifest entry for ${label} has no file/sha256 - starting what is installed"
  exec_current "$@"
fi

asset="${TMP_DIR}/${file}"
asset_url="$(dirname "$MANIFEST_URL")/${file}"
if ! curl -fsSL --max-time "$TIMEOUT" -o "$asset" "$asset_url"; then
  log "could not download ${asset_url} - starting what is installed"
  exec_current "$@"
fi

actual="$(sha256_of "$asset")"
if [ "$actual" != "$expected" ]; then
  log "sha256 mismatch for ${file}: expected ${expected}, got ${actual:-<no sha256 tool>}"
  exec_current "$@"
fi

# Unpack beside the target directory, then move it into place: a partial unpack
# must never become `current`.
staging="${RELEASES_DIR}/.staging.$$"
rm -rf "$staging"
mkdir -p "$staging"
if ! tar -xzf "$asset" -C "$staging"; then
  log "could not unpack ${file} - starting what is installed"
  rm -rf "$staging"
  exec_current "$@"
fi
rm -rf "${RELEASES_DIR:?}/${version}"
mv "$staging" "${RELEASES_DIR}/${version}"

# Point `current` at the release just unpacked.
#
# The obvious spelling - link to a temporary name, then `mv -f` it over
# `current` - does not work, and does not fail either. `current` is a symlink to
# a directory, `mv` follows it, decides the destination is a directory and moves
# the new link *inside* the old release: `current` is left untouched and a stray
# `.current.<pid>` appears under the previous version. The launcher then logs
# "installed <version>" on every boot while still executing the version it first
# installed, which is the one failure a self-updating host must not have
# (mac-studio, v0.1.14 -> v0.1.15). `mv -T` is the fix on GNU and does not exist
# on macOS.
#
# So the old link is removed first, and the new link is renamed into the name
# that frees. With no destination to inspect, every `mv` gets this right. The
# gap between the two is one rename wide and nothing execs from `current` while
# the launcher is flipping it. `previous` is kept so a failed rename can put the
# old link back rather than leave the host with no `current` at all - fail open
# applies here too - and the result is read back, because this bug's whole
# character is that it reported success.
previous="$(readlink "$CURRENT" 2>/dev/null || true)"
flip="${DEPLOY_DIR}/.current.$$"
if ln -sn "${RELEASES_DIR}/${version}" "$flip" && rm -f "$CURRENT" && mv "$flip" "$CURRENT"; then
  if [ "$(readlink "$CURRENT" 2>/dev/null || true)" != "${RELEASES_DIR}/${version}" ]; then
    log "installed ${version} but ${CURRENT} does not point at it - starting what is installed"
    exec_current "$@"
  fi
else
  log "could not point ${CURRENT} at ${version} - starting what is installed"
  rm -f "$flip"
  if [ -n "$previous" ]; then
    ln -sn "$previous" "$CURRENT"
  fi
  exec_current "$@"
fi

log "installed ${version} (${label})"
exec_current "$@"
