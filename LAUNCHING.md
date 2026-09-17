# Launching

`scripts/fetch-launch.sh` installs the newest release built for **this host** and
`exec`s it. Run it once per host start — a systemd unit, a launchd agent, or by
hand:

```bash
scripts/fetch-launch.sh --help
```

Flags are the payload's: the script forwards its arguments, so it never
interprets them.

## Cold start

A host that has never run the launcher is the one case the launcher cannot fix
for itself — something has to put the first copy down. Fetch it from the newest
release, at the path the init unit will run:

```bash
dir="$HOME/.local/share/a2a-goose"; mkdir -p "$dir"
curl -fsSL https://github.com/nickbrett1/a2a-goose/releases/latest/download/fetch-launch.sh \
  -o "$dir/fetch-launch.sh" && chmod +x "$dir/fetch-launch.sh"
```

Point the unit at `$dir/fetch-launch.sh` — not at a checkout of this repository.
Every start after that maintains that one file itself, so the path never changes
again. `deploy/launchd/` and `deploy/dsm/` are already wired to it.

## What it does

1. Fetches the release manifest from a URL that never contains a version.
2. Replaces **itself** with the launcher that manifest advertises, if it
   advertises one (see *The launcher updates itself*).
3. Looks up its **target** in that manifest.
4. If the version is newer, downloads that target's tarball, verifies its
   `sha256`, unpacks it into `releases/<version>/`, and flips the `current`
   symlink.
5. `exec`s `current/bin/a2a-goose`.

```
$DEPLOY_DIR/releases/<version>/     an unpacked payload; entry point bin/a2a-goose
$DEPLOY_DIR/current -> releases/<version>
```

`DEPLOY_DIR` defaults to `$HOME/.local/share/a2a-goose`.

That `exec` is the whole of a host's startup. The payload it lands on is the
**agent**, and the agent starts and supervises its own `goose serve`
(`goose.acp.serve: own`) — so a host has one thing to start and one thing to
supervise, and the agent refuses to start if something is already answering on
the ACP address rather than adopting a goose it did not start. `deploy/` has the
per-host units; [S14](spikes/S14.md) is the evidence.

## The manifest is the only stable URL

`https://github.com/nickbrett1/a2a-goose/releases/latest/download/manifest.json`

Nothing in that path carries a version, so a launcher never has to know one
before it can fetch. The tarball names it downloads are read **out of** the
manifest (`assets[<target>].file`) rather than assembled from a naming
convention, so the only string that has to match between the pipeline and the
launcher is the target key — and both read that from one shared table.

## The launcher updates itself

The launcher is a release asset too, so a fix to *it* reaches a host the same way
a fix to the payload does. The manifest advertises it in a top-level `launcher`
entry beside `assets` — a separate key, because `assets` is keyed by *target* and
the launcher is not one (a candidate list must never resolve to it):

```json
"launcher": { "file": "fetch-launch.sh", "sha256": "…" }
```

On every start the launcher downloads that file, verifies its `sha256`, checks
that it parses (`bash -n`) and — only if it differs from the running copy —
renames it over its own path. It then carries on with the payload, so the new
launcher takes effect on the **next** start.

Two properties are what make replacing yourself safe rather than exciting:

- **It fails open.** A truncated download, an error page, a checksum mismatch, a
  file that does not parse, or a directory the launcher cannot write to all log
  and leave the running launcher exactly where it is. The host still starts.
- **It replaces the name, not the file.** The swap is a `mv` onto the path, never
  a write through it: bash reads a script lazily, off its inode, so rewriting the
  file in place would corrupt the process doing the rewriting.

A manifest with no `launcher` entry — an older release, or a release cut before
this existed — is a no-op: the host keeps running the launcher it has.
`NO_FETCH=1` skips this along with the rest of the fetch. The `sha256` comes from
the same manifest as the asset, so it guards against a corrupt download rather
than a hostile release — the trust the payload already has.

That self-update is also why the launcher must live at a path the host owns and
not inside a checkout: replacing the file is the mechanism, so nothing may be
sharing it.

## Which target a host resolves

The launcher does not derive a target from `uname`. It looks up its **candidate
list** for `uname -s`/`uname -m` and takes the first entry the manifest actually
publishes. Assembling a triple by string concatenation is where producer and
consumer would drift apart, so it never happens: the table is generated from the
same module the pipeline builds its targets from.

Every list ends with `any`, so a host with no triple of its own — an
architecture this project does not build for, or a release whose payload is
architecture-independent — resolves it through the same lookup rather than a
special case.

A host with no candidate in the manifest is a **supported** condition: it logs,
starts what is installed, and carries on.

## Fail open

Every failure — no network, a malformed manifest, no target for this host, a
checksum mismatch, a failed unpack — logs and `exec`s `current` unchanged. A host
must never fail to boot because GitHub was unreachable.

Two consequences worth knowing:

- A crash-restart or a power blip can silently upgrade the host.
- The only hard failure is a **cold** host with nothing installed: there is
  nothing to fall back to, so it exits non-zero and says so.

## Environment

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `NO_FETCH` | unset | Set to anything (`NO_FETCH=1`) to skip the fetch and start what is installed |
| `MANIFEST_URL` | the URL above | Override the manifest location |
| `DEPLOY_DIR` | `$HOME/.local/share/a2a-goose` | Where `releases/` and `current` live |
| `ENV_FILE` | `$HOME/.config/a2a-goose/env` | Env file sourced (not parsed) before the payload starts |
| `LAUNCHER_NAME` | `a2a-goose` | Entry point under `current/bin/` |
| `TIMEOUT` | `10` | Seconds per HTTP request |

`ENV_FILE` is sourced with `set -a`, so every variable it defines is exported to
the payload. That is how a host's own configuration reaches an app that was
fetched from GitHub: keep secrets in that file on the host, never in the release.

The file is **optional**, and most hosts never create one — a missing file is not
an event, and neither is a bad one, because the launcher is fail-open here too: a
file that does not parse is logged and skipped, and a host with no file starts
with the environment it already had.

## The payload contract

The tarball for a target unpacks to the payload root, and its entry point is
`bin/a2a-goose`. That layout is what the build produces: the
`github-release` build step writes its per-target payload into `build/<target>/`,
and `scripts/release-artifacts.sh` packs the contents of that directory as
`<project>-<target>.tar.gz`.

A **runnable bundle** and a **release artifact** are not the same thing. For a
python or node project, `dist/` (a wheel, a bundle) is the right thing to publish
and the wrong thing to launch: it has no `bin/`. The packed directory *is* the
payload root — `scripts/release-artifacts.sh` packs `dist/` with `tar -C dist .`,
so its contents land at the top of the tarball rather than under `dist/` — which
means the entry point has to be at `dist/bin/a2a-goose` before
the packing runs. For a pure-python project that is a `python -m zipapp` bundle
plus a small `bin/a2a-goose` shim that execs it.

A rust project gets this for free: the generated build step copies the binary to
`build/<target>/bin/`, so a per-target tarball unpacks with `bin/` already in it.

Nothing upstream of the exec can catch a payload that is missing this. The
manifest key resolves, the sha256 matches, the unpack succeeds, `current` flips —
and the process then exits without starting, after one log line: `nothing
executable at …`. There is no earlier stage to fail, which is why the layout is
called out here rather than left to the packaging.
