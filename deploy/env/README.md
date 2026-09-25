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

## `A2A_GOOSE_HUB_TOKEN` is the fleet's front door

An agent appears in roost mission control ("the fleet") only by dialling the hub,
and the hub authenticates the WebSocket handshake with `Authorization: Bearer
<token>` — the token from the variable its `config.yaml` names in
`hub.credentialEnv`, here `A2A_GOOSE_HUB_TOKEN`. There is no browser login, so
this one secret is the whole auth boundary between an agent and the fleet, and
the client **refuses to dial** when it is empty rather than connecting
anonymously. A hub enabled with no token is therefore a silent
*non-registration*, not a refused start — the agent comes up, registers with
LiteLLM, and simply never shows in the fleet.

The value lives in **Doppler — project `goose`, config `prd`, key
`A2A_GOOSE_HUB_TOKEN`** — and belongs in `ENV_FILE` beside the bearer token. The
config carries the variable's *name* (`credentialEnv`), never the value, and a
release never carries either. `deploy/ensure-hub.sh` adds the key (as a
`REPLACE_ME` placeholder) and the matching `hub:` block when they are missing; it
cannot add the value, so a host still on the placeholder registers and stays out
of the fleet — exactly mac-studio's state before 2026-09-25.

| Key | Source | Shape |
| --- | ------ | ----- |
| `A2A_GOOSE_HUB_TOKEN` | Doppler `goose/prd`, key `A2A_GOOSE_HUB_TOKEN` | secret string |

## `bind` and `publicUrl` name the same address

`A2A_GOOSE_PUBLIC_URL` is what goes on the card and what LiteLLM dials;
`A2A_GOOSE_BIND` is what the agent listens on. The dialer runs in the LiteLLM
container, so **the bind address has to be the address `publicUrl` names** — the
host's tailnet IP literal on both hosts.

Loopback here is not a hardening measure, it is an outage. Measured on the NAS
(2026-09-17, v0.1.25): with `A2A_GOOSE_BIND="127.0.0.1:10001"` and a
`publicUrl` of `http://100.82.223.13:10001`, `/healthz` answered 200 *on the box*
while the container's dial to `100.82.223.13:10001` was refused — the agent
registered an address nothing listened on, and every call through LiteLLM failed
with `Cannot connect to host 100.82.223.13:10001` (spikes/S15.md §6). Binds are
not forwarded: the tailnet address is a `/32` on `lo`, so a `127.0.0.1` listener
does not answer it even from the same host.

Loopback *is* legitimate in one shape: when something fronts the port (a reverse
proxy that dials `127.0.0.1` and is itself what `publicUrl` names). The agent
cannot tell that case from the mistake, so it **warns at startup** when `bind` is
loopback and `publicUrl` is not, naming both addresses.

## The attribution variables, and why they are here

LiteLLM will not account activity per agent on its own: with one shared
master key every spend-log row is filed under `litellm_proxy_master_key`, which
makes the daily totals useless for "which agent did this". Verified against
`nas:4000` (spikes/S2.md, addendum) — the spend-log row carries
`metadata.user_agent`, populated from the **`User-Agent` header**, and LiteLLM
also auto-adds it to `request_tags`.

So each host names itself:

```sh
LITELLM_CUSTOM_HEADERS='User-Agent: a2a-goose/mac-studio'
```

goose forwards those headers on every provider call (spike S2 proved
`LITELLM_CUSTOM_HEADERS` reaches LiteLLM), and the header is what lets a row be
attributed afterwards by reading `metadata.user_agent` from `/spend/logs/v2`.

**The syntax matters, and the obvious guess is the wrong one.** goose parses
this variable as **`Name: value` lines** — newline-separated when there is more
than one header. Measured on the NAS (v0.1.25, goose 1.50.0), by pointing
`LITELLM_HOST` at a listener that logs every request:

| Value | What happens |
|---|---|
| `User-Agent: a2a-goose/nas` | the header arrives upstream; LiteLLM records `metadata.user_agent = 'a2a-goose/nas'` |
| `User-Agent:a2a-goose/nas` | same — the space after the colon is optional |
| `x-a: 1` newline `x-b: 2` | two headers, both arrive. **A comma does not separate them**: `x-a: 1, x-b: 2` arrives as *one* header named `x-a` with the rest as its value |
| `User-Agent=a2a-goose/nas` | accepted, silently **drops the header** — the turn succeeds and the row comes back with an empty `user_agent` |
| `{"User-Agent":"a2a-goose/nas"}` | **breaks the provider**: goose exits `Error invalid HTTP header name`, and the ACP turn answers `-32603 Internal error ("Error getting agent reply: Provider not set")` |

That last row is worth reading twice: a JSON value here is not a no-op, it is the
difference between a host answering turns and a host that fails every one of them
with a message about a *missing provider*. It cost a real debugging session on
the NAS, where the header was the only variable in the file that was wrong.

The agent now refuses both bad spellings at startup, before it binds a port
(`serve::check_child_env`): a host deployed with either one does not come up at
all, naming the variable and the shape to write instead. That is the same
trade as the goose version gate — a host that advertises skills and fails every
turn is worse than one that does not start.

**Attribution only — no budget is attached to anything, deliberately.** See the
decision in `spikes/S2.md`: goose's and LiteLLM's price tables differ by ~3.5x on
the same call and neither is the provider's invoice, so a proxy-side ceiling
would enforce a guess. The agent bounds its own *loop* instead (`limits` in
`config/config.example.yaml`).

### Verified end to end

The **LiteLLM side is proven** (a direct request with a `User-Agent` came back
with that string in `metadata.user_agent`), the **goose side is proven**
(`LITELLM_CUSTOM_HEADERS` headers arrive at LiteLLM), and as of **2026-09-17 the
two were seen together on the NAS** (v0.1.25, goose 1.50.0, DSM 7.4.1): a real
`SendMessage` turn came back `TASK_STATE_COMPLETED` and the spend-log rows for it
carried

```
2026-09-17T15:01:23  model=deepseek/deepseek-v4-flash
                     ua='a2a-goose/nas'  tags=['User-Agent: a2a-goose', 'User-Agent: a2a-goose/nas']
```

So goose *does* override its own `User-Agent` on the provider call, and the host's
name reaches the row. Route 2 in `spikes/S2.md` (a per-agent virtual key) is not
needed for attribution; the `LITELLM_AGENT_KEY` placeholder below stays commented
out until per-agent *aggregation* from LiteLLM's own endpoints is wanted.

### The other thing the turn taught us: the child goose needs a provider key

`ENV_FILE` is not only the agent's own surface. The agent starts `goose serve`
itself (`goose.acp.serve: own`) and that child **inherits this file's environment
and nothing else** — so any credential goose's provider needs has to be here. On
a host where the human's own goose gets its key from `doppler run` in an
interactive shell, the agent-started goose gets nothing, and the turn fails
*after* the version gate, the bearer and the ACP handshake, with:

```
goose refused the request (-32000): Authentication required
```

measured on the NAS against `http://nas:4000` (a bare `goose.bin run` under an
agent-like environment reproduces it: `401 Unauthorized … No api key passed in`).

The `LITELLM_API_KEY` line in the templates is for exactly this. Master key for
now; a per-agent virtual key with no budget is the better answer (`spikes/S2.md`,
route 2) once attribution is worth splitting by key.
