# deploy/env

Per-host copies of `ENV_FILE` (`$HOME/.config/a2a-goose/env`, mode `0600`). The
launcher sources this file with `set -a` immediately before `exec`, so these are
the *agent's* environment — and the only place a host's identity is written down.

Why per host, and not in config: the agent is one-per-host and `cwd` is the
namespace, so all the host-local facts (which machine is this, what does it call
itself) belong here where they are set once at deploy, not in code or in a
config file that is shared or regenerated.

`*.example` files are templates. Copy, fill in, `chmod 0600`. They are committed
so a new host has something to start from; they must never contain a real secret,
which is why every value that is a secret is a *placeholder* here and the real
one comes from wherever that host already keeps secrets.

## The attribution variables, and why they are here

LiteLLM will not account activity per agent on its own: with one shared
master key every spend-log row is filed under `litellm_proxy_master_key`, which
makes the daily totals useless for "which agent did this". Verified against
`nas:4000` (spikes/S2.md, addendum) — the spend-log row carries
`metadata.user_agent`, populated from the **`User-Agent` header**, and LiteLLM
also auto-adds it to `request_tags`.

So each host names itself:

```sh
LITELLM_CUSTOM_HEADERS='{"User-Agent":"a2a-goose/mac-studio"}'
```

goose forwards those headers on every provider call (spike S2 proved
`LITELLM_CUSTOM_HEADERS` reaches LiteLLM), and the header is what lets a row be
attributed afterwards by reading `metadata.user_agent` from `/spend/logs/v2`.

**Attribution only — no budget is attached to anything, deliberately.** See the
decision in `spikes/S2.md`: goose's and LiteLLM's price tables differ by ~3.5x on
the same call and neither is the provider's invoice, so a proxy-side ceiling
would enforce a guess. The agent bounds its own *loop* instead (`limits` in
`config/config.example.yaml`).

### Not verified end to end

The **LiteLLM side is proven** (a direct request with a `User-Agent` came back
with that string in `metadata.user_agent`) and the **goose side is proven**
(`LITELLM_CUSTOM_HEADERS` headers arrive at LiteLLM). What is *not* yet proven is
the two together — goose overriding its own `User-Agent` on the provider call.
That is a five-minute check on a host that already has a working goose, and it
belongs with **S8/S12** which need the hosts anyway.

If it turns out goose will not override `User-Agent`, the fallback is route 2 in
`spikes/S2.md`: a per-agent virtual key with **no budget attached**, which puts
the host's traffic under its own `api_key` column and makes LiteLLM's own
aggregate endpoints split per agent. The `LITELLM_AGENT_KEY` placeholder below is
for exactly that, and is commented out until it is needed.
