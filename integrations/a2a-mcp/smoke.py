"""A smoke test against a running server: what a client sees, and one real turn.

    sudo docker run --rm --network ai_proxy \
        -v /volumeUSB1/usbshare/docker/a2a-mcp/smoke.py:/app/smoke.py \
        a2a-mcp:latest python /app/smoke.py nas-goose "Which host runs you? One word."

Called with no argument it only lists, which costs nothing and proves the MCP
handshake, the tool schemas and the registry read in one shot.
"""

import asyncio
import os
import sys

from mcp import ClientSession
from mcp.client.streamable_http import streamablehttp_client

URL = os.environ.get("MCP_URL") or "http://a2a-mcp:8090/mcp"


def pick(tools, name: str) -> str:
    """The tool's name here, direct or through a hub that prefixes it.

    mcphub offers a server's tools to a group as `<server>-<tool>`, so the same
    probe works against the server itself and against `…/mcp/core`.
    """
    for tool in tools:
        if tool.name == name or tool.name.endswith(f"-{name}"):
            return tool.name
    raise SystemExit(f"no {name} tool is offered at {URL}: {[t.name for t in tools]}")


async def main() -> None:
    agent = sys.argv[1] if len(sys.argv) > 1 else None
    message = sys.argv[2] if len(sys.argv) > 2 else None

    async with streamablehttp_client(URL) as (read, write, _):
        async with ClientSession(read, write) as session:
            hello = await session.initialize()
            print(f"server: {hello.serverInfo.name} {hello.serverInfo.version}")
            print(f"instructions: {(hello.instructions or '').strip()}")

            tools = (await session.list_tools()).tools
            print(f"\ntools: {[tool.name for tool in tools]}")
            for tool in tools:
                schema = tool.inputSchema
                print(f"  {tool.name}: required={schema.get('required')} optional="
                      f"{sorted(set(schema.get('properties') or {}) - set(schema.get('required') or []))}")

            listing = await session.call_tool(pick(tools, "list_agents"), {})
            print(f"\nlist_agents →\n{listing.content[0].text}")

            if agent and message:
                print(f"\nask_agent({agent!r}, {message!r}) →")
                asked = await session.call_tool(
                    pick(tools, "ask_agent"), {"agent": agent, "message": message}
                )
                print(asked.content[0].text)


if __name__ == "__main__":
    asyncio.run(main())
