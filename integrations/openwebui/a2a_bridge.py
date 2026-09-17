"""
title: A2A Goose
author: a2a-goose
version: 0.1.0
required_open_webui_version: 0.11.0
license: MIT
description: >-
  Reaches an a2a-goose agent through LiteLLM's `/a2a/{agent_id}` JSON-RPC route,
  which is the only LiteLLM path that can talk to an A2A 1.0 agent today.
  Install as an Open WebUI **Pipe** function; it appears in the model dropdown
  as one model. See `integrations/openwebui/README.md` (and `spikes/S15.md` for
  why this is a function and not a model id).
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
                "The agent's `agent_id` from `GET /v1/agents` on the proxy. "
                "Agents never appear in `GET /v1/models`, so this id is the only "
                "handle the proxy has for it."
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
            description="A goose turn is an agent loop, not a completion. Seconds.",
        )

    def __init__(self):
        self.type = "pipe"
        self.id = "a2a_goose"
        self.name = "A2A Goose"
        self.valves = self.Valves()

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

        try:
            answer = await self._call(request)
        except Exception as e:
            await self._status(__event_emitter__, f"Agent failed: {e}", done=True)
            raise

        await self._status(__event_emitter__, "Answer received", done=True)
        return answer

    async def _call(self, request) -> str:
        if not self.valves.AGENT_ID:
            raise ValueError(
                "AGENT_ID is not set. Read it from `GET /v1/agents` on the proxy."
            )

        url = f"{self.valves.A2A_ROUTE.rstrip('/')}/{self.valves.AGENT_ID}"
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
