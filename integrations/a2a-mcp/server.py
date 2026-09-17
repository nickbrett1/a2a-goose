"""The registered A2A agents, as MCP tools any MCP client can call.

## Why this exists next to `../openwebui/`

The bridge in `../openwebui/` turns each registered agent into an Open WebUI
*model*, which is what you want when the agent **is** the thing you are talking
to. It does nothing for the other direction: a conversation with a *different*
model — an Open WebUI chat on `core`, or a human goose session — that wants to
**hand work** to an agent ("please follow up with the nas agent and finish the
task"). A model cannot discover an agent it has no tool for, so this is that
tool, and MCP is the surface both callers already speak: goose as an extension,
Open WebUI 0.11+ as a Tool Server.

    list_agents()   the roster, read live from the proxy, on every call
    ask_agent()     one A2A `SendMessage`, answered

Two tools rather than a fixed single-agent binding, for the same reason the
bridge became a manifold: a hand-maintained roster is invisible when it is
wrong. An agent that comes up is askable immediately; one that is cleared is
not offered.

## What it does not expect the protocol to do

Both measurements this rests on were taken in `spikes/S15.md`:

  * **A 1.0 request is built here.** LiteLLM's `/a2a/{agent_id}` route forwards
    a JSON-RPC method verbatim, so a hand-built `SendMessage`/`ROLE_USER`
    reaches an `a2a-lf` agent. LiteLLM's own model paths (`a2a/<name>`) build
    the body themselves in the 0.3 dialect and the agent answers
    `method not found: message/send`.
  * **A credential is carried here.** The agent's bearer lives in its LiteLLM
    row's `static_headers`, which the route honours. What this process holds is
    a *proxy* virtual key — enough for `GET /v1/agents` and the route, and not
    enough to be an agent's identity.

It is a deliberate copy of the bridge's registry-and-answer handling rather than
an import of it: the bridge has to stay a single file that installs into Open
WebUI through its own UI, and a shared package would break exactly that. The
duplication is two small functions, and `README.md` says so out loud.

## A call has a deadline; the agent does not

`TIMEOUT_SECONDS` is where *this* process gives up, and every caller in front of
it — mcphub, Open WebUI, a goose session — usually gives up sooner. Giving up
does not stop the turn: the agent keeps working with nobody listening, which is
how a caller learns that "the tool call failed" and "the agent failed" are
different claims (`RUNBOOK.md`). So both tools say the number out loud and say
what to do about it, which is the advertising half of the timeout item and not
a change in behaviour.

What is advertised is deliberately about **what to send**, because the deadline
is a property of the task and not of the agent waking up: measured on
mac-studio, a new conversation on a running agent answers in ~2 s, the first
turn after a restart in ~2 s, and three simultaneous turns in ~3 s
(`spikes/S18.md`). There is no warm-up to budget for — there is work, and work
can outlive the call.
"""

from __future__ import annotations

import os
import uuid
from typing import Annotated, Any

import httpx
from mcp.server.fastmcp import FastMCP
from mcp.server.transport_security import TransportSecuritySettings
from pydantic import Field

# Everything deployment-specific is an environment variable, so the compose file
# next to this one is the only place a host is described.
A2A_ROUTE = (os.environ.get("A2A_ROUTE") or "http://litellm:4000/a2a").rstrip("/")
LITELLM_API_KEY = os.environ.get("LITELLM_API_KEY") or ""

# A turn here is an agent loop, not a completion: minutes, not seconds. The
# caller's own timeout is the real ceiling — mcphub and Open WebUI each impose
# one — so this is deliberately the more patient of the two. Where it lands
# *and what to do about it* is in the module docstring; what matters here is
# that the number is read once and then advertised verbatim, so the tool
# description cannot drift from the behaviour.
TIMEOUT_SECONDS = float(os.environ.get("TIMEOUT_SECONDS") or 300)
REGISTRY_TIMEOUT_SECONDS = float(os.environ.get("REGISTRY_TIMEOUT_SECONDS") or 15)
# The deadline is asked of the agent itself, and an agent that is down must not
# hold up a listing. Five seconds is a card fetch on a tailnet, not a turn.
CARD_TIMEOUT_SECONDS = float(os.environ.get("CARD_TIMEOUT_SECONDS") or 5)

# The paragraph both callers read. It is a module constant rather than a
# docstring because it carries `TIMEOUT_SECONDS` and a docstring cannot be
# formatted at import without a decorator that exists only to do that.
DEADLINE = (
    f"One call waits up to {TIMEOUT_SECONDS:g} s for an answer, and the client's "
    "own tool-call timeout is often lower still. Agent-to-agent calls are meant "
    "for relatively short-lived work: a turn is a whole agent loop, so send a "
    "task that finishes inside that window. If the work will outlive the call, "
    "say so in the message and ask the agent to write its result down as it goes "
    "— a memo, a file — because a call that gives up does not stop the turn, and "
    "the result is only lost if nobody wrote it down. Reuse the same context_id "
    "on a later call to collect it in the same conversation."
)

MCP_HOST = os.environ.get("MCP_HOST") or "0.0.0.0"
MCP_PORT = int(os.environ.get("MCP_PORT") or 8090)

# LiteLLM's route answers this for a good turn; anything else is reported to the
# caller rather than being mistaken for an empty answer.
COMPLETED = "TASK_STATE_COMPLETED"

mcp = FastMCP(
    "a2a-agents",
    instructions=(
        "Hand work to the user's own A2A agents. They are separate agent "
        "processes on the user's machines (a NAS, a Mac Studio), reachable "
        "through the LiteLLM proxy, and they can take minutes to answer. Call "
        "list_agents to see who is registered before sending anything. These "
        f"calls are for relatively short-lived work: one waits up to "
        f"{TIMEOUT_SECONDS:g} s before giving up, the turn carries on without "
        "you if it does, and a long task is better asked to write its result "
        "down somewhere you can read it."
    ),
    host=MCP_HOST,
    port=MCP_PORT,
    # No MCP session state: each call is independent, which is what a hub
    # in front of it (mcphub) and a per-request client (Open WebUI) both want,
    # and nothing here is worth resuming across a reconnect.
    stateless_http=True,
    # The SDK's DNS-rebinding guard is a host allowlist, and an allowlist here
    # would have to name every container name and proxy hostname that ever
    # reaches this port — the same maintenance trap as two URLs that must agree.
    # The bind is the boundary instead: this is an internal Docker network, and
    # a Tailnet behind it.
    transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False),
)


class UnknownAgent(Exception):
    """No agent matched, or more than one did. The message carries the roster."""


def flatten_docstring(fn):
    """Keep the source's indentation out of the model's prompt.

    FastMCP hands a tool's `__doc__` to the client verbatim, so a docstring
    written inside a function arrives with four spaces on every continuation
    line. Paragraphs are kept; the indent is not. Decorate *below* `@mcp.tool()`
    so the registration reads the flattened text.
    """
    paragraphs = (fn.__doc__ or "").split("\n\n")
    fn.__doc__ = "\n\n".join(" ".join(p.split()) for p in paragraphs if p.strip())
    return fn


# --------------------------------------------------------------------------- #
# The proxy's two endpoints
# --------------------------------------------------------------------------- #


def registry_url(route: str) -> str:
    """`…/a2a` → `…/v1/agents`: the registry is the route's sibling.

    Derived rather than configured, so that one address describes the proxy.
    Two variables that must agree is the loopback-bind shape again: one edited,
    one not, and the failure is a DNS error at call time.
    """
    base = (route or "").rstrip("/")
    for suffix in ("/a2a", "/a2a/"):
        if base.endswith(suffix):
            base = base[: -len(suffix)]
            break
    return f"{base.rstrip('/')}/v1/agents"


def headers() -> dict[str, str]:
    """A virtual key on every call; the agent's own bearer is never held here."""
    result = {"Content-Type": "application/json"}
    if LITELLM_API_KEY:
        result["Authorization"] = f"Bearer {LITELLM_API_KEY}"
    return result


async def registry() -> list[dict[str, Any]]:
    """The live roster. Read per call: this is the discovery half of the tool."""
    async with httpx.AsyncClient(timeout=REGISTRY_TIMEOUT_SECONDS) as client:
        response = await client.get(registry_url(A2A_ROUTE), headers=headers())
    response.raise_for_status()

    payload = response.json()
    # `{"agents": [...]}` and a bare list have both been seen; read the roster
    # either way rather than guessing.
    agents = payload.get("agents", payload) if isinstance(payload, dict) else payload
    return [a for a in agents if isinstance(a, dict)]


async def send(agent_id: str, message: str, context_id: str | None = None) -> str:
    """One `SendMessage`, one answer.

    `contextId` is the agent's own session key: pass the same one again and the
    turn lands in the conversation it belongs to, because the agent retains a
    session per context and does not need the history re-sent (`spikes/S15.md`
    §7). Without one, this is a fresh conversation — which is the honest
    default for a tool call, where nothing identifies the caller's chat.
    """
    request = {
        "jsonrpc": "2.0",
        "id": str(uuid.uuid4()),
        "method": "SendMessage",
        "params": {
            "message": {
                "messageId": str(uuid.uuid4()),
                "role": "ROLE_USER",
                "parts": [{"text": message}],
                "contextId": context_id or str(uuid.uuid4()),
            }
        },
    }

    url = f"{A2A_ROUTE}/{agent_id}"
    async with httpx.AsyncClient(timeout=TIMEOUT_SECONDS) as client:
        response = await client.post(url, json=request, headers=headers())

    # LiteLLM relays the agent's own errors through this route, so a non-2xx is
    # usually the agent (an unauthenticated route, a bad method) rather than the
    # proxy. The body says which, and it is worth keeping.
    if response.status_code >= 400:
        raise RuntimeError(f"{response.status_code} from {url}: {response.text[:400]}")

    return answer(response.json())


def answer(payload: dict[str, Any]) -> str:
    """The turn's text, or the reason there isn't any.

    Two shapes arrive: a JSON-RPC result (`{"result": {"task": …}}`) and
    whatever LiteLLM wrapped it in on the way out. Both are walked rather than
    assumed. The measured response also carries a *second*, empty `answer`
    artifact (see `spikes/S15.md` §7), so the text is collected and filtered
    rather than read at a fixed index.
    """
    if payload.get("error"):
        error = payload["error"]
        message = error.get("message") if isinstance(error, dict) else error
        raise RuntimeError(f"A2A error: {message}")

    result = payload.get("result", payload)
    task = result.get("task", result) if isinstance(result, dict) else {}
    status = task.get("status") or {}
    state = status.get("state")

    texts = [
        part["text"]
        for artifact in task.get("artifacts") or []
        for part in artifact.get("parts") or []
        if part.get("text")
    ]
    if not texts:
        # A turn that answered in a status message rather than an artifact.
        texts = [
            part["text"]
            for part in (status.get("message") or {}).get("parts") or []
            if part.get("text")
        ]

    if state and state != COMPLETED and not texts:
        # Naming the state is the point: `TASK_STATE_INPUT_REQUIRED` is a
        # different problem from `TASK_STATE_FAILED`.
        reason = status.get("error") or status.get("message") or state
        raise RuntimeError(f"the turn ended {state}: {reason}")

    return "\n".join(texts) if texts else ""


# --------------------------------------------------------------------------- #
# Naming: what people say vs what the registry spells
# --------------------------------------------------------------------------- #


def canonical(name: str) -> str:
    """`the nas agent` → `nas`.

    The request that motivated this tool is "please follow up with the nas
    agent", so the tool has to survive a model that passes the phrase rather
    than the registry's `nas-goose`. Articles and the word "agent" are how
    people say it and are never how an agent is named.
    """
    text = (name or "").strip().strip("\"'").lower()
    for article in ("the ", "a ", "an "):
        if text.startswith(article):
            text = text[len(article) :]
            break
    for suffix in (" agents", " agent", " host", " machine", " server"):
        if text.endswith(suffix):
            text = text[: -len(suffix)]
            break
    return text.strip()


def name_of(agent: dict[str, Any]) -> str:
    return str(agent.get("agent_name") or agent.get("agent_id") or "").strip()


def turn_ceiling(card: dict[str, Any]) -> float | None:
    """The agent's *own* turn ceiling, if its card advertises one.

    This process's deadline and the agent's are different numbers, and a caller
    that can only see one of them is guessing. An a2a-goose agent publishes its
    `promptSecs` as a `capabilities.extensions` entry (`docs/turn-deadline.md`)
    and LiteLLM carries the card into the row, so reading it back is what makes
    the roster useful for budgeting. An agent that advertises nothing — or a row
    whose card was rewritten — reads as `None` rather than as a guess.

    Read the row's copy first and stop there when it has one. LiteLLM stores a
    *normalised* card and drops `capabilities.extensions` (measured 2026-09-17:
    a registered a2a-goose row comes back with `capabilities` = `{"streaming":
    true}` while the agent's own card carries the extension), so today the row
    never has one — `own_turn_ceiling` is what actually finds it.
    """
    capabilities = card.get("capabilities") or {}
    if not isinstance(capabilities, dict):
        return None
    for extension in capabilities.get("extensions") or []:
        if not isinstance(extension, dict):
            continue
        params = extension.get("params") or {}
        seconds = params.get("promptSecs")
        if isinstance(seconds, (int, float)) and not isinstance(seconds, bool):
            return float(seconds)
    return None


async def own_turn_ceiling(card: dict[str, Any]) -> float | None:
    """The ceiling an agent advertises, read from the agent when the row has none.

    The proxy's copy cannot be trusted to carry it — see `turn_ceiling` — so the
    fallback is the agent's own card endpoint, which is the same card the proxy
    was handed and is served by the process that will actually run the turn.
    Fail-open in every direction: no url, a refused connection, a timeout, a
    body that is not JSON, or a card with no extension all read as "advertises
    nothing", which is what a caller assumed before any of this existed. One
    agent being down must not cost the roster its other entries.
    """
    documented = turn_ceiling(card)
    if documented is not None:
        return documented
    url = str(card.get("url") or "").strip()
    if not url:
        return None
    try:
        async with httpx.AsyncClient(timeout=CARD_TIMEOUT_SECONDS) as client:
            response = await client.get(f"{url.rstrip('/')}/.well-known/agent-card.json")
        response.raise_for_status()
        published = response.json()
    except (httpx.HTTPError, httpx.InvalidURL, ValueError):
        return None
    return turn_ceiling(published if isinstance(published, dict) else {})


def roster(agents: list[dict[str, Any]]) -> str:
    """Who there is, for an error the model can act on.

    A tool that only says "not found" makes the model guess again; naming the
    candidates lets it correct itself in one step.
    """
    names = [name_of(agent) for agent in agents if name_of(agent)]
    return ", ".join(repr(name) for name in names) if names else "(no agents registered)"


def resolve(agents: list[dict[str, Any]], wanted: str) -> dict[str, Any]:
    """The agent the caller meant, or an error that lists who there is.

    Exact first, then a unique substring either way round, because the model
    that says `nas` means the only agent whose name contains it. Ambiguity is an
    error rather than a coin toss.
    """
    needle = canonical(wanted)
    if not needle:
        raise UnknownAgent(
            f"no agent was named. Registered agents: {roster(agents)}. "
            "Pass one of these names."
        )

    for agent in agents:
        if needle in (name_of(agent).lower(), str(agent.get("agent_id") or "").lower()):
            return agent

    hits = [
        agent
        for agent in agents
        if needle in name_of(agent).lower() or name_of(agent).lower() in needle
    ]
    if len(hits) == 1:
        return hits[0]

    if hits:
        raise UnknownAgent(
            f"{wanted!r} matches more than one agent: "
            f"{roster(hits)}. Use one of these names exactly."
        )
    raise UnknownAgent(
        f"no agent matches {wanted!r}. Registered agents: {roster(agents)}."
    )


# --------------------------------------------------------------------------- #
# The tools
# --------------------------------------------------------------------------- #


@mcp.tool()
@flatten_docstring
async def list_agents() -> str:
    """List the A2A agents that are registered and reachable right now.

    Call this first: it is the only way to know which agents exist. The roster
    is read live from the proxy on every call, so an agent that has just been
    started appears here immediately and one that has been shut down does not —
    do not assume an agent from earlier in the conversation is still there.
    """
    agents = await registry()
    if not agents:
        return "No agents are registered on the proxy right now."

    lines = [f"{len(agents)} agent(s) registered on the proxy:", ""]
    for agent in agents:
        card = agent.get("agent_card_params") or {}
        lines.append(f"- {name_of(agent)} (id {agent.get('agent_id')})")
        if card.get("url"):
            lines.append(f"    url: {card['url']}")
        if card.get("protocolVersion"):
            lines.append(f"    protocol: {card['protocolVersion']}")
        if card.get("description"):
            lines.append(f"    description: {card['description']}")
        ceiling = await own_turn_ceiling(card)
        if ceiling is not None:
            lines.append(
                f"    its own turn ceiling: {ceiling:g} s "
                "(what it allows one turn to take, not what it usually takes)"
            )
        skills = [
            str(skill.get("name") or skill.get("id") or "?")
            for skill in card.get("skills") or []
            if isinstance(skill, dict)
        ]
        if skills:
            lines.append(f"    skills: {', '.join(skills)}")

    lines += [
        "",
        "Hand one of them work with ask_agent(agent=..., message=...). "
        f"{DEADLINE}",
    ]
    return "\n".join(lines)


@mcp.tool(
    description=(
        "Send one message to a registered A2A agent and return its answer.\n\n"
        "The agent is a separate process with its own conversation: it cannot "
        "see this chat, so give it everything it needs and send work it can "
        "carry out on its own.\n\n"
        f"{DEADLINE}\n\n"
        "Its answer is returned verbatim — quote or summarise it rather than "
        "inventing what it would say."
    )
)
async def ask_agent(
    agent: Annotated[
        str,
        Field(
            description=(
                "The agent to send to, by the name `list_agents` reported "
                "(e.g. 'nas-goose'). A distinctive part of the name works too."
            )
        ),
    ],
    message: Annotated[
        str,
        Field(
            description=(
                "What to ask or tell the agent, phrased so it stands on its "
                "own: it is a separate agent with its own conversation, and it "
                "cannot see this chat. Include anything it needs to know."
            )
        ),
    ],
    context_id: Annotated[
        str | None,
        Field(
            description=(
                "Optional. Pass the same value on a later call to continue the "
                "agent's previous conversation; omit it to start a fresh one. "
                "This is also how a result that outlived an earlier call is "
                "collected."
            )
        ),
    ] = None,
) -> str:
    """Send one message to a registered A2A agent and return its answer.

    The description shown to the model is built above (it carries the deadline);
    this docstring is for people reading the source.
    """
    agents = await registry()
    chosen = resolve(agents, agent)  # raises UnknownAgent, which carries the roster

    try:
        text = await send(str(chosen.get("agent_id")), message, context_id)
    except httpx.TimeoutException:
        # A deadline is not a failure of the agent. Saying so — and saying what
        # it would take to keep the result — is the difference between a model
        # that retries the same doomed call and one that changes the ask.
        return (
            f"Gave up waiting for {name_of(chosen)} after {TIMEOUT_SECONDS:g} s. "
            "The turn was not cancelled by this: the agent is most likely still "
            "working, with nobody listening. If the task is a long one, send it "
            "again (same context_id to stay in the same conversation) and ask it "
            "to write its result down where you can read it, then collect that."
        )
    if not text.strip():
        return f"{name_of(chosen)} finished the turn without an answer."
    return text


if __name__ == "__main__":
    mcp.run(transport="streamable-http")
