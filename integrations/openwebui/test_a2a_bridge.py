"""The bridge's parsing and message-picking, against payloads taken off the wire.

Run it where the bridge runs — it imports the same `httpx`/`pydantic` the Open
WebUI container has:

    docker exec open-webui python /app/backend/data/a2a_bridge_test.py

Everything asserted here was measured on the NAS on 2026-09-17 (`spikes/S15.md`
§1 and §7): the completed task below is the live response to one `SendMessage`,
including the second, empty `answer` artifact that the agent emits and that a
parser reading `artifacts[0]` by index would survive by luck.
"""

import asyncio
import os
import sys
import unittest

import httpx

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from a2a_bridge import Pipe  # noqa: E402

# Straight off the live route: `POST /a2a/{agent_id}`, SendMessage, one turn.
COMPLETED_TURN = {
    "id": 1,
    "jsonrpc": "2.0",
    "result": {
        "task": {
            "id": "01a0b087-7a8e-7751-a195-415eafa8d931",
            "contextId": "bridge-probe-1",
            "status": {
                "state": "TASK_STATE_COMPLETED",
                "timestamp": "2026-09-17T18:01:07.015692080Z",
            },
            "artifacts": [
                {"artifactId": "answer", "name": "answer", "parts": [{"text": "alpha"}]},
                {"artifactId": "answer", "name": "answer"},
            ],
            "history": [
                {
                    "messageId": "m-1",
                    "contextId": "bridge-probe-1",
                    "role": "ROLE_USER",
                    "parts": [{"text": "Reply with the single word: alpha"}],
                }
            ],
        }
    },
}


class Answer(unittest.TestCase):
    def test_a_completed_turn_yields_its_artifact_text(self):
        self.assertEqual(Pipe._answer(COMPLETED_TURN), "alpha")

    def test_the_empty_second_artifact_does_not_add_a_blank_line(self):
        # The measured artifact list carries a duplicate id with no `parts` at
        # all; joining naively would answer "alpha\n".
        self.assertNotIn("\n", Pipe._answer(COMPLETED_TURN))

    def test_a_jsonrpc_error_is_raised_with_its_message(self):
        payload = {"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "method not found"}}
        with self.assertRaises(RuntimeError) as caught:
            Pipe._answer(payload)
        self.assertIn("method not found", str(caught.exception))

    def test_an_unfinished_state_is_named_rather_than_answered_emptily(self):
        # `INPUT_REQUIRED` is actionable and a blank answer is not, so the state
        # is what the user is shown.
        payload = {
            "result": {
                "task": {
                    "status": {"state": "TASK_STATE_INPUT_REQUIRED"},
                    "artifacts": [],
                }
            }
        }
        with self.assertRaises(RuntimeError) as caught:
            Pipe._answer(payload)
        self.assertIn("TASK_STATE_INPUT_REQUIRED", str(caught.exception))

    def test_text_in_a_status_message_is_used_when_there_are_no_artifacts(self):
        payload = {
            "result": {
                "task": {
                    "status": {
                        "state": "TASK_STATE_COMPLETED",
                        "message": {"role": "ROLE_AGENT", "parts": [{"text": "done"}]},
                    }
                }
            }
        }
        self.assertEqual(Pipe._answer(payload), "done")

    def test_an_unwrapped_task_is_read_the_same_way(self):
        # LiteLLM relays rather than rewrites, but the route's own wrapper has
        # changed shape between versions - so both are walked.
        self.assertEqual(Pipe._answer(COMPLETED_TURN["result"]), "alpha")


class MessagePicking(unittest.TestCase):
    def test_the_last_user_message_is_the_one_sent(self):
        body = {
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answered"},
                {"role": "user", "content": "  second  "},
            ]
        }
        self.assertEqual(Pipe._last_user_message(body), "second")

    def test_a_multimodal_turn_contributes_only_its_text(self):
        body = {
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what is this"},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                    ],
                }
            ]
        }
        self.assertEqual(Pipe._last_user_message(body), "what is this")

    def test_no_user_message_is_an_empty_string(self):
        self.assertEqual(Pipe._last_user_message({"messages": []}), "")
        self.assertEqual(Pipe._last_user_message({}), "")


class Discovery(unittest.TestCase):
    """The manifold's two moving parts: where the roster is read, and which
    agent the dropdown's choice names."""

    def test_the_registry_is_the_route_s_sibling(self):
        self.assertEqual(
            Pipe._registry_url("http://litellm:4000/a2a"),
            "http://litellm:4000/v1/agents",
        )

    def test_a_trailing_slash_does_not_move_the_registry(self):
        self.assertEqual(
            Pipe._registry_url("http://litellm:4000/a2a/"),
            "http://litellm:4000/v1/agents",
        )

    def test_a_sub_model_id_is_the_agent_id(self):
        # `get_pipe_id` splits the model id on its first dot, so everything
        # after it is the agent this turn is for.
        body = {"model": "a2a_goose.0a2d93c6-2b0e-471b-9507-a055b5cfe97d"}
        self.assertEqual(
            Pipe._agent_id(_valves(""), body),
            "0a2d93c6-2b0e-471b-9507-a055b5cfe97d",
        )

    def test_the_valve_is_the_fallback_when_there_is_no_sub_id(self):
        self.assertEqual(Pipe._agent_id(_valves("the-valve"), {"model": "a2a_goose"}), "the-valve")
        self.assertEqual(Pipe._agent_id(_valves("the-valve"), {}), "the-valve")

    def test_a_cleared_agent_id_reports_the_agent_rather_than_a_blank(self):
        # This is what a stale binding looks like from the proxy, and the text a
        # user should see names the id rather than "400".
        with self.assertRaises(RuntimeError) as caught:
            Pipe._answer({"error": {"message": "Agent 'x' not found"}})
        self.assertIn("Agent 'x' not found", str(caught.exception))


def _valves(agent_id):
    """A minimal stand-in for an instance, for the static helpers."""
    return type("boom", (), {"valves": type("v", (), {"AGENT_ID": agent_id})()})()


class Context(unittest.TestCase):
    def test_a_chat_id_keeps_the_conversation_on_one_agent_session(self):
        self.assertEqual(Pipe._context_id({}, {"chat_id": "abc"}), "abc")

    def test_without_a_chat_id_a_fresh_context_is_used_each_time(self):
        first = Pipe._context_id({}, {})
        second = Pipe._context_id({}, {})
        self.assertTrue(first and second)
        self.assertNotEqual(first, second)


class Deadline(unittest.TestCase):
    """A call that gives up is not a turn that failed."""

    def setUp(self):
        self.pipe = Pipe()
        self.pipe.valves.TIMEOUT_SECONDS = 42

    def _reply(self):
        async def too_slow(*_args, **_kwargs):
            raise httpx.ReadTimeout("no answer in time")

        self.pipe._call = too_slow
        body = {
            "model": "a2a_goose.0a2d93c6-2b0e-471b-9507-a055b5cfe97d",
            "messages": [{"role": "user", "content": "do a long thing"}],
        }
        return asyncio.run(self.pipe.pipe(body, {"chat_id": "c-1"}))

    def test_a_timeout_names_the_deadline_and_that_the_turn_survives(self):
        reply = self._reply()
        self.assertIn("42 s", reply)
        self.assertIn("not cancelled", reply)

    def test_a_timeout_points_at_the_durable_workaround(self):
        # The measured failure mode: the caller gives up, the agent keeps
        # working, and the result is lost only because nobody wrote it down.
        self.assertIn("write its result down", self._reply())


if __name__ == "__main__":
    unittest.main(verbosity=2)
