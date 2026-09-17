"""
title: A2A Goose
author: a2a-goose
version: 0.1.0
required_open_webui_version: 0.11.0
license: MIT
description: >-
  Every agent in LiteLLM's registry, as an Open WebUI model. Install as an Open
  WebUI **Pipe** function and it offers one model per registered agent, named
  from the registry, appearing and disappearing as agents are registered and
  cleared — a manifold rather than a fixed binding. It reaches each one through
  LiteLLM's `/a2a/{agent_id}` JSON-RPC route, which is the only LiteLLM path
  that can talk to an A2A 1.0 agent today. A turn here is a whole agent loop, so
  this function waits `TIMEOUT_SECONDS` (180 s by default) for it: treat these
  models as **relatively short-lived work**, and if a task will run longer, ask
  the agent to write its result down and pick it up in a later message. See
  `integrations/openwebui/README.md` (and `spikes/S15.md` for why this is a
  function and not a model id).
"""

import json
import uuid

import httpx
from pydantic import BaseModel, Field

# What LiteLLM's route will not do for us, so this function does:
#
#   * build a 1.0 request. LiteLLM's *model* paths (`a2a/<name>`) construct the
#     JSON-RPC body themselves in the **0.3** dialect (`message/send`, role
#     `"user"`) and an `a2a-lf` agent answers `method not found: message/send`.
#     The *route* forwards the caller's method verbatim, so sending 1.0 by hand
#     is the whole trick (spikes/S15.md §2).
#   * carry a credential. LiteLLM sends no `Authorization` on any A2A path by
#     default and does not forward `litellm_params.api_key`; the agent's bearer
#     lives in the agent row's `static_headers`, which the route honours and the
#     model paths do not (spikes/S15.md §4). So the caller authenticates to the
#     *proxy* with a virtual key and never holds the agent's token.
#   * survive the response shape. A completed turn is a `task` whose text is in
#     `artifacts[*].parts[*].text` — and the measured response carries a second,
#     empty `answer` artifact, so the text is collected and filtered rather than
#     read at a fixed index.

# LiteLLM's route answers `TASK_STATE_COMPLETED` for a good turn; anything else
# is reported to the user instead of being mistaken for an empty answer.
COMPLETED = "TASK_STATE_COMPLETED"

# Discovery, and why it is a *manifold*.
#
# Open WebUI builds one model per entry in a pipe's `pipes` attribute, and it
# evaluates that attribute every time it builds the model list — so a `pipes`
# that reads the proxy's registry gives a dropdown that is the registry:
# register an agent and it appears, clear one and it goes. The alternative (one
# function per host, with the agent id in a valve) is a roster maintained by
# hand, and a stale valve is invisible: the model is still in the dropdown and
# every turn fails with `Agent '<id>' not found`.
#
# The sub-model ids are the **agent ids**, and the names are the agents' own
# `agent_name`s: no slug, no mapping table, and nothing to rename when an agent
# is re-registered. `GET /v1/agents` answers to the same virtual key the route
# needs (measured), so this costs no extra credential.


class Pipe:
    """An a2a-goose agent as an Open WebUI model."""

    class Valves(BaseModel):
        """Admin-configured, in the Open WebUI UI: Admin → Functions → this one.

        Nothing here belongs in the repository or in a chat: the agent's own
        bearer is on the LiteLLM agent row, and what this function holds is a
        *virtual key* for the proxy.
        """

        A2A_ROUTE: str = Field(
            default="http://litellm:4000/a2a",
            description=(
                "LiteLLM's A2A route, without the trailing agent id. Use the "
                "address the Open WebUI **container** can resolve — `litellm` "
                "by container name on the shared network, not the NAS host name."
            ),
        )
        AGENT_ID: str = Field(
            default="",
            description=(
                "The agent's `agent_id` from `GET /v1/agents`. Used as a "
                "**fallback** only: normally the agent comes from the model "
                "picked in the dropdown, and from the registry when the "
                "dropdown is built. Set it to keep one agent offered even if "
                "the registry cannot be read."
            ),
        )
        LITELLM_API_KEY: str = Field(
            default="",
            description=(
                "A LiteLLM virtual key. The agent's bearer is *not* used here: "
                "the proxy attaches it from the agent row's static_headers."
            ),
        )
        TIMEOUT_SECONDS: int = Field(
            default=180,
            description=(
                "A goose turn is an agent loop, not a completion. Seconds to "
                "wait for one; the chat shows a timeout if it is exceeded, and "
                "the agent keeps working. Raise it for heavy work, or ask the "
                "agent to write its result down and read it later."
            ),
        )

    def __init__(self):
        self.type = "pipe"
        self.id = "a2a_goose"
        # Deliberately no `self.name`: Open WebUI *prefixes* a manifold's
        # sub-model names with it, and the sub-model names are the agents'.
        self.valves = self.Valves()

    async def pipes(self):
        """One model per registered agent, read when Open WebUI lists models.

        This is what makes the dropdown the registry. A registry that cannot be
        read is not allowed to empty the dropdown: the valve's agent is offered
        as a fallback, so a working deployment keeps working when the proxy is
        being restarted.
        """
        try:
            agents = await self._registry()
        except Exception as e:
            print(f"a2a bridge: could not read the agent registry: {e}")
            agents = []

        entries = [
            {"id": a["agent_id"], "name": agent_name}
            for a in agents
            if (agent_name := (a.get("agent_name") or a.get("agent_id")))
            and a.get("agent_id")
        ]
        if not entries and self.valves.AGENT_ID:
            entries = [{"id": self.valves.AGENT_ID, "name": "A2A Goose"}]
        return entries

    async def _registry(self) -> list:
        url = self._registry_url(self.valves.A2A_ROUTE)
        headers = {}
        if self.valves.LITELLM_API_KEY:
            headers["Authorization"] = f"Bearer {self.valves.LITELLM_API_KEY}"

        async with httpx.AsyncClient(timeout=15) as client:
            response = await client.get(url, headers=headers)
        response.raise_for_status()

        payload = response.json()
        # `{"agents": [...]}` and a bare list have both been seen; the roster is
        # read either way rather than guessed at.
        agents = payload.get("agents", payload) if isinstance(payload, dict) else payload
        return [a for a in agents if isinstance(a, dict)]

    @staticmethod
    def _registry_url(route: str) -> str:
        """`…/a2a` → `…/v1/agents`: the registry is the route's sibling.

        Derived rather than configured, because two URL valves that must agree
        is the loopback-bind shape again — one edited, one not, and the failure
        is `getaddrinfo` at list time.
        """
        base = (route or "").rstrip("/")
        for suffix in ("/a2a", "/a2a/"):
            if base.endswith(suffix):
                base = base[: -len(suffix)]
                break
        return f"{base.rstrip('/')}/v1/agents"

    def _agent_id(self, body) -> str:
        """The agent the dropdown chose, or the valve's fallback.

        Open WebUI passes a manifold's model id as `<function id>.<sub id>`, and
        the sub id here is the agent id — so the choice travels in the request
        and nothing has to be looked up again.
        """
        model = body.get("model") or ""
        if "." in model:
            sub = model.split(".", 1)[1].strip()
            if sub:
                return sub
        return self.valves.AGENT_ID

    async def pipe(self, body, __metadata__=None, __event_emitter__=None):
        """One Open WebUI turn → one A2A `SendMessage` → one answer.

        `body` is the OpenAI-shaped request Open WebUI would have sent to a
        model; `__metadata__` carries the chat id, which is what keeps a
        conversation's memory on the agent side (the agent retains a session per
        `contextId`, so a multi-turn chat is one session and the history is not
        re-sent every turn — measured: spikes/S15.md §7).
        """
        await self._status(__event_emitter__, "Asking the agent", done=False)

        user_message = self._last_user_message(body)
        if not user_message:
            return "No user message to send."

        request = {
            "jsonrpc": "2.0",
            "id": str(uuid.uuid4()),
            "method": "SendMessage",
            "params": {
                "message": {
                    "messageId": str(uuid.uuid4()),
                    "role": "ROLE_USER",
                    "parts": [{"text": user_message}],
                    # The chat, not the message: the agent keys its retained
                    # session on this, so a follow-up lands in the conversation
                    # it belongs to. A chat whose session the agent has since
                    # evicted still answers — it just answers without history.
                    "contextId": self._context_id(body, __metadata__),
                }
            },
        }

        agent_id = self._agent_id(body)
        if not agent_id:
            return (
                "No agent selected and no AGENT_ID configured. Pick a model from "
                "the dropdown, or set the valve."
            )

        try:
            answer = await self._call(request, agent_id)
        except httpx.TimeoutException:
            # A deadline is not a failed turn. Say so, and say what it takes to
            # keep the result: the agent is still working, and a message asking
            # it to write the answer down is what makes that work collectable.
            seconds = self.valves.TIMEOUT_SECONDS
            await self._status(
                __event_emitter__, f"No answer within {seconds} s", done=True
            )
            return (
                f"No answer within {seconds} s. The turn was not cancelled — the "
                "agent is most likely still working. For a task like this, ask "
                "it in a new message to write its result down somewhere you can "
                "read it (a memo, a file) and then collect that."
            )
        except Exception as e:
            await self._status(__event_emitter__, f"Agent failed: {e}", done=True)
            raise

        await self._status(__event_emitter__, "Answer received", done=True)
        return answer

    async def _call(self, request, agent_id) -> str:
        url = f"{self.valves.A2A_ROUTE.rstrip('/')}/{agent_id}"
        headers = {"Content-Type": "application/json"}
        if self.valves.LITELLM_API_KEY:
            headers["Authorization"] = f"Bearer {self.valves.LITELLM_API_KEY}"

        async with httpx.AsyncClient(timeout=self.valves.TIMEOUT_SECONDS) as client:
            response = await client.post(url, json=request, headers=headers)

        # LiteLLM relays the agent's own errors through this route, so a non-2xx
        # is usually the agent (401 unauthenticated, a bad method) rather than
        # the proxy. The body is what says which, and it is worth keeping.
        if response.status_code >= 400:
            raise RuntimeError(
                f"{response.status_code} from {url}: {response.text[:400]}"
            )

        return self._answer(response.json())

    @staticmethod
    def _answer(payload: dict) -> str:
        """The turn's text, or the reason there isn't any.

        Two shapes arrive here: a JSON-RPC result (`{"result": {"task": …}}`) and
        whatever LiteLLM wrapped it in on the way out. Both are walked rather
        than assumed.
        """
        if "error" in payload and payload["error"]:
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
            # different problem from `TASK_STATE_FAILED`, and the caller can act
            # on the difference.
            reason = status.get("error") or status.get("message") or state
            raise RuntimeError(f"the turn ended {state}: {reason}")

        return "\n".join(texts) if texts else ""

    @staticmethod
    def _last_user_message(body) -> str:
        for message in reversed(body.get("messages") or []):
            if message.get("role") != "user":
                continue
            content = message.get("content")
            if isinstance(content, str):
                return content.strip()
            if isinstance(content, list):
                # Multi-modal turns arrive as parts; only the text is sent, and
                # saying so is better than sending a stringified image.
                return "".join(
                    part.get("text", "")
                    for part in content
                    if isinstance(part, dict) and part.get("type") == "text"
                ).strip()
        return ""

    @staticmethod
    def _context_id(body, metadata) -> str:
        """One context per Open WebUI chat, so turns accumulate on the agent.

        `chat_id` is present for a saved chat and absent for some API callers;
        a fresh id then means a fresh agent session, which is the honest
        fallback rather than reusing another conversation's memory.
        """
        for source in (metadata or {}, body):
            for key in ("chat_id", "chatId"):
                value = source.get(key) if isinstance(source, dict) else None
                if value:
                    return str(value)
        return str(uuid.uuid4())

    @staticmethod
    async def _status(emitter, description, done):
        # The emitter is absent for API callers and background tasks, and a
        # status line is a courtesy — never a reason to fail a turn.
        if emitter is None:
            return
        try:
            await emitter({"type": "status", "data": {"description": description, "done": done}})
        except Exception:
            pass
