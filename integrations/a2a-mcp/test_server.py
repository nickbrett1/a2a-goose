"""Tests for the a2a-mcp server.

Run them in the image the server actually ships in — the dev box this repo is
worked on has no working Python packaging:

    cd /volumeUSB1/usbshare/docker/a2a-mcp
    sudo docker run --rm -v "$PWD":/w -w /w a2a-mcp:latest \
        python -m unittest -v test_server

They cover three things the design depends on: the proxy's two endpoints are
addressed correctly, a caller's loose name (`the nas agent`) resolves to the
registry's spelling, and a real turn's response — captured from the live agent,
including the second empty artifact that a fixed index would have read — is
parsed into the answer. The last class drives the whole path over HTTP against
a stub proxy, so the request that leaves here is asserted, not assumed.
"""

import asyncio
import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import server

# Captured from the live chain (LiteLLM → a2a-lf), via spikes/S15.md §7.
# Note the second `answer` artifact: it has no `parts` key at all, and a parser
# that read `artifacts[0]` by position would be relying on luck.
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

AGENT = {
    "agent_id": "0a2d93c6-2b0e-471b-9507-a055b5cfe97d",
    "agent_name": "nas-goose",
    "litellm_params": {"api_key": "REDACTED_BY_LITELLM", "is_public": False},
    "agent_card_params": {
        "url": "http://100.82.223.13:10001",
        "name": "nas-goose",
        "description": "goose on the NAS",
        "protocolVersion": "1.0",
        "skills": [{"id": "ask", "name": "ask"}],
        "capabilities": {
            "streaming": True,
            # What a real row carries: `docs/turn-deadline.md` says a2a-goose
            # publishes its own prompt ceiling here.
            "extensions": [
                {
                    "uri": "https://github.com/nickbrett1/a2a-goose/blob/main/docs/turn-deadline.md",
                    "required": False,
                    "params": {"promptSecs": 900, "cancelSecs": 10},
                }
            ],
        },
    },
}

MAC = {
    "agent_id": "11111111-2222-3333-4444-555555555555",
    "agent_name": "mac-studio",
    "agent_card_params": {"url": "http://192.168.1.4:10001", "skills": []},
}


class Addressing(unittest.TestCase):
    """The route and the registry are siblings; only one is configured."""

    def test_the_registry_is_the_route_s_sibling(self):
        self.assertEqual(
            server.registry_url("http://litellm:4000/a2a"), "http://litellm:4000/v1/agents"
        )

    def test_a_trailing_slash_does_not_move_the_registry(self):
        self.assertEqual(
            server.registry_url("http://litellm:4000/a2a/"), "http://litellm:4000/v1/agents"
        )

    def test_the_configured_route_is_the_one_used(self):
        # `A2A_ROUTE` is read once at import; the tests below repoint it at a
        # stub, so this asserts the default a deployment gets is the container
        # name (what a container on the shared network can resolve).
        self.assertTrue(server.A2A_ROUTE.startswith("http://"), server.A2A_ROUTE)
        self.assertFalse(server.A2A_ROUTE.endswith("/a2a/"))

    def test_a_virtual_key_travels_on_every_call(self):
        original = server.LITELLM_API_KEY
        server.LITELLM_API_KEY = "sk-virtual"
        try:
            self.assertEqual(server.headers()["Authorization"], "Bearer sk-virtual")
        finally:
            server.LITELLM_API_KEY = original

    def test_no_key_means_no_header_rather_than_an_empty_bearer(self):
        original = server.LITELLM_API_KEY
        server.LITELLM_API_KEY = ""
        try:
            self.assertNotIn("Authorization", server.headers())
        finally:
            server.LITELLM_API_KEY = original


class Naming(unittest.TestCase):
    """A caller's phrase, not a registry key."""

    def test_the_request_that_motivated_this_resolves(self):
        # "please follow up with the nas agent" — the model may pass the phrase.
        self.assertEqual(server.canonical("the nas agent"), "nas")
        self.assertEqual(server.resolve([AGENT, MAC], "the nas agent")["agent_name"], "nas-goose")

    def test_case_and_quotes_do_not_matter(self):
        self.assertEqual(server.canonical('"NAS-Goose"'), "nas-goose")
        self.assertEqual(server.resolve([AGENT], "NAS-GOOSE")["agent_id"], AGENT["agent_id"])

    def test_a_distinctive_part_of_the_name_is_enough(self):
        self.assertEqual(server.resolve([AGENT, MAC], "nas")["agent_name"], "nas-goose")

    def test_the_agent_id_resolves_too(self):
        self.assertEqual(server.resolve([AGENT], AGENT["agent_id"])["agent_name"], "nas-goose")

    def test_an_unknown_name_lists_who_there_is(self):
        with self.assertRaises(server.UnknownAgent) as caught:
            server.resolve([AGENT, MAC], "windows-box")
        message = str(caught.exception)
        self.assertIn("nas-goose", message)
        self.assertIn("mac-studio", message)

    def test_an_ambiguous_name_is_an_error_rather_than_a_coin_toss(self):
        # `mac` could be either machine, and picking one of them would be a
        # silent wrong answer — the model gets both names back and chooses.
        mini = {"agent_id": "99999999-0000-0000-0000-000000000000", "agent_name": "mac-mini"}
        with self.assertRaises(server.UnknownAgent) as caught:
            server.resolve([MAC, mini], "mac")
        message = str(caught.exception)
        self.assertIn("more than one", message)
        self.assertIn("mac-studio", message)
        self.assertIn("mac-mini", message)

    def test_a_full_name_is_not_ambiguous_even_when_a_part_of_one_matches(self):
        # `studio` occurs in one name and none of the others: resolve, do not
        # hedge.
        self.assertEqual(server.resolve([AGENT, MAC], "studio")["agent_name"], "mac-studio")

    def test_an_empty_roster_says_so(self):
        with self.assertRaises(server.UnknownAgent) as caught:
            server.resolve([], "nas")
        self.assertIn("no agents registered", str(caught.exception))


class Answering(unittest.TestCase):
    """The response shapes a turn can arrive in."""

    def test_a_completed_turn_yields_its_text(self):
        self.assertEqual(server.answer(COMPLETED_TURN), "alpha")

    def test_the_empty_second_artifact_adds_nothing(self):
        # The measured response carries two `answer` artifacts and the second
        # has no `parts`; joining artifacts must not invent a blank line.
        task = COMPLETED_TURN["result"]["task"]
        self.assertEqual(len(task["artifacts"]), 2)
        self.assertEqual(server.answer(COMPLETED_TURN), "alpha")
        self.assertNotIn("\n", server.answer(COMPLETED_TURN))

    def test_an_error_body_is_raised_rather_than_returned_as_an_answer(self):
        with self.assertRaises(RuntimeError) as caught:
            server.answer({"error": {"message": "Agent 'x' not found"}})
        self.assertIn("Agent 'x' not found", str(caught.exception))

    def test_a_turn_that_did_not_complete_names_its_state(self):
        payload = {
            "result": {
                "task": {
                    "status": {
                        "state": "TASK_STATE_INPUT_REQUIRED",
                        "message": {"parts": [{"text": "which host?"}]},
                    }
                }
            }
        }
        # A status message *is* an answer: asked for input is not a failure.
        self.assertEqual(server.answer(payload), "which host?")

    def test_a_failed_turn_with_no_text_raises(self):
        payload = {"result": {"task": {"status": {"state": "TASK_STATE_FAILED"}}}}
        with self.assertRaises(RuntimeError) as caught:
            server.answer(payload)
        self.assertIn("TASK_STATE_FAILED", str(caught.exception))


class Tools(unittest.TestCase):
    """What a model is actually offered."""

    def setUp(self):
        self.tools = {tool.name: tool for tool in asyncio.run(server.mcp.list_tools())}

    def test_both_tools_are_exposed(self):
        self.assertEqual(set(self.tools), {"list_agents", "ask_agent"})

    def test_ask_agent_asks_for_the_agent_and_the_message_only(self):
        schema = self.tools["ask_agent"].inputSchema
        self.assertEqual(set(schema["required"]), {"agent", "message"})
        self.assertIn("context_id", schema["properties"])
        self.assertNotIn("context_id", schema.get("required") or [])

    def test_the_descriptions_say_when_to_call_them(self):
        self.assertIn("registered", self.tools["list_agents"].description.lower())
        self.assertIn("answer", self.tools["ask_agent"].description.lower())

    def test_the_client_gets_instructions_about_patience(self):
        # A caller that gives up in seconds will never see a turn finish.
        instructions = server.mcp.instructions or ""
        self.assertIn("minutes", instructions)

    def test_the_deadline_is_advertised_where_a_model_will_read_it(self):
        # The timeout is a fact about the call, so it belongs in the tool the
        # caller is about to use and not only in a README.
        deadline = f"{server.TIMEOUT_SECONDS:g} s"
        self.assertIn(deadline, self.tools["ask_agent"].description)
        self.assertIn(deadline, self.tools["list_agents"].description)
        self.assertIn("short-lived", self.tools["ask_agent"].description)
        self.assertIn(deadline, server.mcp.instructions or "")

    def test_a_docstring_arrives_without_its_python_indentation(self):
        # FastMCP passes the docstring through verbatim, so without flattening
        # the model reads four spaces on every continuation line.
        description = self.tools["list_agents"].description
        self.assertNotIn("\n    ", description)
        self.assertIn("\n\n", description)


class ProxyStub(BaseHTTPRequestHandler):
    """A stand-in for LiteLLM: the two endpoints this server uses, nothing else."""

    agents: list = []
    received: list = []
    # Seconds to sit on a POST before answering, so a test can make the
    # server's own deadline the thing that fires.
    delay: float = 0.0

    def log_message(self, *args):  # keep the test output readable
        pass

    def do_GET(self):
        if self.path != "/v1/agents":
            self.send_error(404)
            return
        body = json.dumps(self.agents).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        request = json.loads(self.rfile.read(length) or b"{}")
        ProxyStub.received.append(
            {"path": self.path, "request": request, "headers": dict(self.headers)}
        )
        if ProxyStub.delay:
            time.sleep(ProxyStub.delay)
        body = json.dumps(COMPLETED_TURN).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class ThroughTheWire(unittest.TestCase):
    """The whole path: registry → resolve → SendMessage → answer."""

    @classmethod
    def setUpClass(cls):
        ProxyStub.agents = [AGENT, MAC]
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), ProxyStub)
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

        cls.original_route = server.A2A_ROUTE
        cls.original_key = server.LITELLM_API_KEY
        server.A2A_ROUTE = f"http://127.0.0.1:{cls.httpd.server_address[1]}/a2a"
        server.LITELLM_API_KEY = "sk-virtual"

    @classmethod
    def tearDownClass(cls):
        server.A2A_ROUTE = cls.original_route
        server.LITELLM_API_KEY = cls.original_key
        cls.httpd.shutdown()

    def setUp(self):
        ProxyStub.received.clear()
        ProxyStub.delay = 0.0

    def test_listing_reports_the_roster_it_read(self):
        listing = asyncio.run(server.list_agents())
        self.assertIn("nas-goose", listing)
        self.assertIn("mac-studio", listing)
        self.assertIn("2 agent(s)", listing)

    def test_listing_reports_the_agent_s_own_turn_ceiling(self):
        # The card's number and this process's are different, and a caller that
        # can only see one of them is guessing.
        self.assertIn("turn ceiling: 900 s", asyncio.run(server.list_agents()))

    def test_an_agent_that_advertises_nothing_reads_as_unknown(self):
        self.assertIsNone(server.turn_ceiling(MAC["agent_card_params"]))
        self.assertIsNone(server.turn_ceiling({}))

    def test_a_junk_extension_is_not_mistaken_for_a_ceiling(self):
        card = {"capabilities": {"extensions": [{"params": {"promptSecs": "soon"}}, "x"]}}
        self.assertIsNone(server.turn_ceiling(card))

    def test_asking_lands_on_the_route_with_a_1_0_send_message(self):
        reply = asyncio.run(server.ask_agent(agent="the nas agent", message="say alpha"))
        self.assertEqual(reply, "alpha")

        sent = ProxyStub.received[-1]
        self.assertEqual(sent["path"], f"/a2a/{AGENT['agent_id']}")
        self.assertEqual(sent["request"]["method"], "SendMessage")
        message = sent["request"]["params"]["message"]
        self.assertEqual(message["role"], "ROLE_USER")
        self.assertEqual(message["parts"], [{"text": "say alpha"}])
        # A fresh session when the caller offers no context: not someone
        # else's conversation.
        self.assertTrue(message["contextId"])
        self.assertEqual(sent["headers"]["Authorization"], "Bearer sk-virtual")

    def test_a_context_id_is_carried_through_for_a_follow_up(self):
        asyncio.run(
            server.ask_agent(agent="nas-goose", message="and now?", context_id="chat-42")
        )
        message = ProxyStub.received[-1]["request"]["params"]["message"]
        self.assertEqual(message["contextId"], "chat-42")

    def test_a_timeout_is_not_reported_as_an_agent_failure(self):
        # The measured failure mode behind this item: a caller gives up
        # (`MCP error -32001`) while the turn is still running. The tool has to
        # say that, and say what to do about it, rather than let a model read it
        # as "the agent failed, try again".
        before = server.TIMEOUT_SECONDS
        server.TIMEOUT_SECONDS = 0.05
        ProxyStub.delay = 1.0
        try:
            reply = asyncio.run(server.ask_agent(agent="nas-goose", message="slow one"))
        finally:
            server.TIMEOUT_SECONDS = before
        self.assertIn("Gave up waiting for nas-goose after 0.05 s", reply)
        self.assertIn("not cancelled", reply)
        self.assertIn("write its result down", reply)

    def test_an_unknown_agent_never_reaches_the_proxy(self):
        with self.assertRaises(server.UnknownAgent):
            asyncio.run(server.ask_agent(agent="windows-box", message="hello"))
        self.assertEqual([call for call in ProxyStub.received if call["path"].startswith("/a2a/")], [])


if __name__ == "__main__":
    unittest.main()
