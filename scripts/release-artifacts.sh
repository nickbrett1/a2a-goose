#!/usr/bin/env bash
#
# Produces the files that get attached to the GitHub Release.
#
# genproj seeds this file once; after that it is yours. `scripts/` is app-owned,
# so regeneration never overwrites it — unlike .buildkite/pipeline.yml, which is
# genproj's and is rewritten on every regeneration.
#
# Contract: write the files to attach into $OUT_DIR (default: release/). The
# release step uploads every file it finds there and nothing else. Producing no
# files is valid: the release then carries notes and no assets.
#
# Called as: bash scripts/release-artifacts.sh <version>
# The version is the tag without its `v` prefix, e.g. "1.2.4" for tag v1.2.4.
set -euo pipefail

VERSION="${1:?usage: release-artifacts.sh <version>}"
OUT_DIR="${OUT_DIR:-release}"

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

# --- the universal payload ---------------------------------------------------
# `dist/` is the language's conventional output directory and it is architecture
# independent (a JS bundle, a wheel, a .pyz), so it is published under the
# universal key rather than under a triple. The release step downloads what the
# build step uploaded before calling this, so `dist/` here is the exact
# directory the tests ran against. Asset names are a contract with whoever
# consumes them: something fetching releases/latest/download/<name> depends on
# the exact string, so treat a name as frozen once anything relies on it.
#
# `-C dist .` puts the CONTENTS of dist/ at the root of the tarball, so dist/ is
# the payload root: a launcher that execs `bin/<name>` needs `dist/bin/<name>` to
# exist before this line runs. Nothing downstream checks it - see LAUNCHING.md.
if [ -d dist ]; then
  tar -czf "$OUT_DIR/a2a-goose-any.tar.gz" -C dist .
  echo "packaged dist/ as a2a-goose-any.tar.gz"
fi

# --- one artifact per declared release target --------------------------------
# `github-release.targets` selects these, and the build step for each one writes
# its payload to build/<target>/ and uploads exactly that path. Targets are Rust
# triples — the names `cargo --target` takes, shared by the pipeline's build
# matrix and by a launcher resolving the manifest. One vocabulary, one string:
# the manifest key, the artifact path and the lookup are the same value, so
# nothing translates between two spellings. Every Linux target is musl, which
# links statically, so a single artifact runs on a musl or a glibc host alike.
#
# An asset name that embeds a version is unlaunchable: version and hash belong
# in the manifest (below), and the asset name carries the target only. That is
# what makes releases/latest/download/manifest.json fetchable with no version
# knowledge.
#
# A target with no build/<target>/ is reported and skipped rather than failing
# the release, so declaring a target before its build produces anything costs
# nothing — the same fail-open reasoning as the artifact download above. The
# list is empty for a project that declares no targets, which makes the whole
# loop a no-op.
#
# This list must stay in step with github-release.targets. The pipeline's half
# is regenerated; this half is not, because scripts/ is app-owned - so a target
# added in the generator has to be added here by hand, or its payload is built,
# uploaded and fetched and then quietly never packed.
for target in aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  if [ -d "build/$target" ]; then
    # The payload has to be executable at the far end, and the bit does not
    # survive the trip here: the build step's `cp` and the release step's
    # `curl -o` both create the file under this machine's umask. A tarball
    # packed without this shipped `-rw-r--r--`, and the launcher on a cold host
    # refused to start it — "nothing executable at
    # .../current/bin/a2a-goose" (mac-studio, 2026-09-16). Pack time is the
    # durable place to normalise it: this file is app-owned and never
    # regenerated, unlike the generated step that produced the file.
    if [ -d "build/$target/bin" ]; then
      find "build/$target/bin" -type f -exec chmod 0755 {} +
    fi
    tar -czf "$OUT_DIR/a2a-goose-$target.tar.gz" -C "build/$target" .
    echo "packaged build/$target/ as a2a-goose-$target.tar.gz"
  else
    echo "No build/$target/ directory - nothing to attach for $target." >&2
  fi
done

# --- the launcher ------------------------------------------------------------
# The launcher is what launchd and DSM supervise, so a fix to *it* has to be
# shippable the same way a fix to the payload is - otherwise the code that
# supervises everything else only changes when a human visits the box. That is
# not hypothetical here: mac-studio ran a launcher four commits stale while its
# payload self-updated underneath (S11).
#
# Published verbatim rather than packed - it is one script, not a payload tree -
# and advertised in the manifest's "launcher" entry, which the running launcher
# reads to replace itself before it execs. `scripts/fetch-launch.sh` is
# app-owned, so this is a guard and not a promise: if it is ever removed, the
# release simply carries no launcher and the manifest no "launcher" key, which a
# running launcher reads as "keep running me".
if [ -f scripts/fetch-launch.sh ]; then
  cp scripts/fetch-launch.sh "$OUT_DIR/fetch-launch.sh"
  chmod +x "$OUT_DIR/fetch-launch.sh"
  echo "published scripts/fetch-launch.sh as fetch-launch.sh"
fi

# Nothing at all was produced. Say so here, once, rather than attaching an empty
# release with no explanation.
if [ -z "$(ls -A "$OUT_DIR" 2>/dev/null)" ]; then
  echo "No payloads found, so this release carries notes and no assets." >&2
  echo "Edit scripts/release-artifacts.sh once this project builds something to ship." >&2
fi

# --- manifest (a launcher's only stable URL) ---------------------------------
# releases/latest/download/manifest.json is what something fetching without
# knowing the version reads. It is written last, after every asset exists, so
# a manifest never advertises a file that is not there. The key per asset is
# the target: a Rust triple (aarch64-apple-darwin, x86_64-unknown-linux-musl,
# ...) or any for an architecture-independent payload (a JS
# bundle, a pure-python .pyz). A launcher intersects its own uname-derived
# candidate list with these keys and never constructs one from uname, so a
# label change on one side cannot silently 404 the other.
# sha256 is computed here, once, next to the packing that produced the file.
if [ -n "$(ls -A "$OUT_DIR" 2>/dev/null)" ]; then
  {
    printf '{\n'
    printf '  "name": "%s",\n' "a2a-goose"
    printf '  "version": "%s",\n' "$VERSION"
    printf '  "tag": "v%s",\n' "$VERSION"
    printf '  "commit": "%s",\n' "${BUILDKITE_COMMIT:-}"
    printf '  "assets": {'
    first=1
    for file in "$OUT_DIR"/*.tar.gz; do
      [ -e "$file" ] || continue
      base="$(basename "$file")"
      target="${base#"a2a-goose"-}"
      target="${target%.tar.gz}"
      sha="$(sha256sum "$file" | cut -d' ' -f1)"
      [ "$first" = 1 ] || printf ','
      first=0
      printf '\n    "%s": { "file": "%s", "sha256": "%s" }' "$target" "$base" "$sha"
    done
    printf '\n  }'
    # The launcher, beside `assets` rather than inside it: `assets` is keyed by
    # *target* and consumed by the launcher's candidate lookup, and the launcher
    # is not a target - a candidate list must never resolve to it. Same one-line
    # shape, so the launcher reads both with the same grep+sed and needs no JSON
    # parser on a host that may not have one.
    if [ -f "$OUT_DIR/fetch-launch.sh" ]; then
      launcher_sha="$(sha256sum "$OUT_DIR/fetch-launch.sh" | cut -d' ' -f1)"
      printf ',\n  "launcher": { "file": "fetch-launch.sh", "sha256": "%s" }' "$launcher_sha"
    fi
    printf '\n}\n'
  } > "$OUT_DIR/manifest.json"
  echo "wrote $OUT_DIR/manifest.json"
fi
