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
for target in aarch64-apple-darwin x86_64-unknown-linux-musl; do
  if [ -d "build/$target" ]; then
    tar -czf "$OUT_DIR/a2a-goose-$target.tar.gz" -C "build/$target" .
    echo "packaged build/$target/ as a2a-goose-$target.tar.gz"
  else
    echo "No build/$target/ directory - nothing to attach for $target." >&2
  fi
done

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
    printf '\n  }\n}\n'
  } > "$OUT_DIR/manifest.json"
  echo "wrote $OUT_DIR/manifest.json"
fi
