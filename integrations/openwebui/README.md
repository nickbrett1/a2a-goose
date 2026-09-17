# Open WebUI → a2a-goose, through LiteLLM

Open WebUI can only talk to OpenAI-shaped endpoints: every request it makes lands
on `<base>/chat/completions` or `/responses`. LiteLLM's A2A **model** paths
(`a2a/<agent_name>`) build their JSON-RPC body in the **A2A 0.3** dialect while
`a2a-lf` speaks **1.0** — so those paths answer `method not found: message/send`
and cannot be fixed from here ([`spikes/S15.md`](../../spikes/S15.md) §2).

LiteLLM's **route** — `POST /a2a/{agent_id}` — forwards the caller's method
verbatim and is the one path that reaches a 1.0 agent today. Its caller is a
JSON-RPC client, and Open WebUI is not one. This directory is the one small
function that makes it one.

```
Open WebUI  ──OpenAI──▶  /api/chat/completions
                            │  (this Pipe)
                            ▼
              POST /a2a/{agent_id}  {"method": "SendMessage", …}
                            │  LiteLLM: forwards method, attaches static_headers
                            ▼
                        a2a-goose agent  ──ACP──▶  goose
```

The agent's own bearer never leaves the LiteLLM agent row (`static_headers`); what
the function holds is a LiteLLM **virtual key**, which is why the caller's
credential can be rotated without touching the agent.

## Files

| file | what it is |
| --- | --- |
| `a2a_bridge.py` | the Open WebUI **Pipe** function. Self-contained; paste or POST it. |
| `test_a2a_bridge.py` | parsing tests against payloads taken off the live wire. Run it where the bridge runs. |

## Install

The function's valves are admin configuration, so install it as an admin. Both
paths end the same way: one entry in **Admin → Functions**, switched **on**, with
`AGENT_ID` and `LITELLM_API_KEY` set.

### By hand (the UI)

1. **Admin → Functions → +**, paste `a2a_bridge.py`, save.
2. Open it and set the valves (the cog), then flip the toggle on.

### By API (what was actually measured here)

```bash
# The agent's id: agents never appear in GET /v1/models, so this is how you find it.
curl -s -H "Authorization: Bearer $LITELLM_MASTER_KEY" http://litellm:4000/v1/agents \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);[print(a["agent_id"],a["agent_name"]) for a in (d.get("agents",d) if isinstance(d,dict) else d)]'

# Create it. `id` must be a Python identifier; it is lowercased and becomes the
# model id Open WebUI offers. The frontmatter in the file supplies `meta.manifest`.
python3 - <<'PY'
import json, urllib.request
body = {"id": "a2a_goose", "name": "A2A Goose",
        "content": open("a2a_bridge.py").read(),
        "meta": {"description": "An a2a-goose agent through LiteLLM's A2A route.", "manifest": {}}}
req = urllib.request.Request("http://owui:8080/api/v1/functions/create",
    data=json.dumps(body).encode(), method="POST",
    headers={"Authorization": "Bearer $OWUI_ADMIN_KEY", "Content-Type": "application/json"})
print(urllib.request.urlopen(req).status)
PY

# Valves. Note the shape: the body **is** the valves dict — not {"valves": {…}},
# which validates to nothing and returns 200 with an empty object.
curl -s -X POST http://owui:8080/api/v1/functions/id/a2a_goose/valves/update \
  -H "Authorization: Bearer $OWUI_ADMIN_KEY" -H 'Content-Type: application/json' \
  -d '{"AGENT_ID": "<agent_id>", "LITELLM_API_KEY": "sk-…"}'

# A new function is created **inactive**, and `/toggle` is a toggle, not a set.
curl -s -X POST http://owui:8080/api/v1/functions/id/a2a_goose/toggle \
  -H "Authorization: Bearer $OWUI_ADMIN_KEY" -H 'Content-Type: application/json' -d '{}'
```

The virtual key can be the one already in the container's `OPENAI_API_KEY`, which
is what Open WebUI uses for its LiteLLM connection — it is a LiteLLM key either
way, and `GET /v1/agents` needs the master key rather than a virtual one.

### Valves

| valve | default | notes |
| --- | --- | --- |
| `A2A_ROUTE` | `http://litellm:4000/a2a` | no trailing agent id. Must resolve **from the Open WebUI container** — the container name on the shared network, not the NAS host name ([S12](../../spikes/S12.md): a musl payload could not resolve DSM's uppercase host name; the same class of mistake, one container over). |
| `AGENT_ID` | — | from `GET /v1/agents`. |
| `LITELLM_API_KEY` | — | a virtual key. Not the agent's bearer. |
| `TIMEOUT_SECONDS` | `180` | a goose turn is an agent loop, not a completion. |

## Measured, 2026-09-17, on the NAS

Through Open WebUI's own API (`POST /api/chat/completions`, `model: a2a_goose`) —
the same call the browser makes:

| turn | sent | Open WebUI returned |
| --- | --- | --- |
| 1 | *"Reply with the single word: zeta"* | `zeta` |
| 2 | *"What single word did I ask you to reply with? One word."* — **same chat** | `zeta` |
| 3 | the same question — **different chat** | *"I don't see any earlier request like that — this is the start of our conversation…"* |

Turn 2 is the point: the bridge keys `contextId` on the Open WebUI chat id, so a
chat is one agent session and the history is *not* re-sent every turn — the
agent's own M3 session retention carries it. Turn 3 is the control that makes
turn 2 mean something.

The container log for the same turns, from inside Open WebUI:

```
httpx HTTP Request: POST http://litellm:4000/a2a/0a2d93c6-2b0e-471b-9507-a055b5cfe97d "HTTP/1.1 200 OK"
```

and parsing was checked against the **live** response body, not a fixture
invented here:

```console
$ docker exec open-webui python /tmp/bridge/test_a2a_bridge.py
Ran 11 tests in 0.002s
OK
```

## What a reader of this should know

- **The agent is a model in the dropdown, and is not in `/v1/models`.** Open WebUI
  builds a model entry from an active Pipe function, which is what puts "A2A
  Goose" in the list; LiteLLM itself will never advertise an agent as a model.
  A fresh Open WebUI pointed at LiteLLM alone shows seven models and none of them
  is an agent.
- **A turn is one JSON-RPC call and it is not streamed to the *user*.** The route
  is a unary `SendMessage`: the answer arrives whole, and a long turn shows a
  status line and then the text. `SendStreamingMessage` exists on the agent and
  on the wire ([`tests/skeleton.rs`](../../tests/skeleton.rs) pins the SSE
  frames); relaying it through this route is unmeasured and not claimed here.
- **The completed task carries two `answer` artifacts, one of them empty.** Not
  ours to explain yet ([S15](../../spikes/S15.md) §7); the parser collects texts
  and filters empties rather than reading `artifacts[0]`, so it does not matter —
  but a caller indexing by position would answer `""` about half the time.
- **The status emitter is optional.** It is absent for API callers and background
  tasks, and a status line is a courtesy — it never fails a turn.
- **Nothing here is a dependency of the agent.** If this function is deleted the
  agent is unaffected; the route it calls is LiteLLM's, and the token it needs is
  on LiteLLM's agent row.
