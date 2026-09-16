# Buildkite

`pipeline.yml` in this directory is the source of truth for this project's CI.
The pipeline object in Buildkite carries a one-line wrapper that uploads this
file, so edits take effect on the next push — there is nothing to change in the
Buildkite UI.

## How a build runs

- **Queue:** `mac-studio-linux` — the pipeline is dispatched to that
  self-hosted agent, not to a cloud fleet.
- **Image:** `rust:1-slim` for the build, lint and test steps (public;
  pick an arm64 image, the fleet is Apple silicon).
- **Steps:** install, then build, lint and test — in a **single job on purpose**.
  The install is the dominant fixed cost, so it is paid once rather than once
  per step.

The step set is _capability-driven_, exactly as the CircleCI config is: a
deployment capability adds its deploy step, `gitguardian` adds the secret scan,
`lighthouse-ci` adds the performance gate. A project that selects none of them
gets build and test and nothing else.

## What the agent has to provide

None of this lives in the repository, and the pipeline fails in confusing ways
without it:

1. **`plugins-path`** in `buildkite-agent.cfg`. Agent v4 has no usable default,
   and _every_ step here uses a plugin — omitting it fails the checkout.
2. **A unique agent `name`** when a machine runs more than one agent
   (`name="<machine>-%spawn"`). Otherwise agents race over the plugin directory.
3. **Docker.** Almost every step runs in a container. The exception is a
   `github-release` release target ending in `-apple-darwin`: a macOS binary
   cannot be linked inside a Linux container, so that step is dispatched with no
   docker plugin and runs **on the host**. Its toolchain therefore has to be on
   the machine and not only in the image — for a rust target that means `rustup`,
   `cargo` and the Xcode command line tools, reachable from the agent's own
   `PATH` (a job inherits the agent process's environment, not your shell's).
   That step also ignores `buildkite.queue`: it is dispatched to the macOS queue
   (`mac-studio-linux`, which names the queue's containers rather than its
   hosts), because moving `buildkite.queue` to a Linux queue must not move a
   build that cannot run there.
4. **Secrets, delivered to the job environment** by the agent's `environment`
   hook. A step-level `env:` value does **not** reach the container — only names
   listed in the docker plugin's `environment:` list do. This is the most common
   way a working step mysteriously loses its credentials.

## Secrets each contributed step needs

Provided by the agent's `environment` hook, by name:

- **Secret scan** (`gitguardian`) — `GITGUARDIAN_API_KEY`. Scans a path, so it
  needs no git history.
- **Deploy** (`cloudflare-wrangler`) — `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID`, plus `DOPPLER_TOKEN` when the `doppler` capability is
  selected (for `scripts/setup-wrangler-config.sh` and the secret sync).
- **Image publish** (`docker-container`) — `GHCR_USERNAME` and `GHCR_TOKEN`.
  Runs on the agent, which has the Docker daemon it needs.

This list is a bullet list rather than a table on purpose: a generated file has
to survive `prettier --check` in this project's own lint step, and prettier
re-pads markdown tables to its own column widths.

## Worth knowing

- If a build is queued but never starts, the queue name is wrong (above).
- A pipeline can exist, look correctly connected, and receive **nothing** on
  push. That is a missing GitHub webhook, and it is worth checking before
  debugging anything else.
- The Lighthouse step runs on `main` only by default, and passes its config
  explicitly (`--config .lighthouse.cjs`) because that filename is not one lhci
  discovers on its own.
- A rust target builds and tests with `--locked`, so the repository has to carry
  a `Cargo.lock` that matches `Cargo.toml`. A missing or stale lockfile fails
  those commands rather than silently resolving new versions — which is the
  point of the flag, and the reason to commit the lockfile with the manifest.
