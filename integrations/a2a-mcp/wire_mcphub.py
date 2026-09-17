"""Wire the a2a-mcp server into mcphub: one server, offered by every group.

Doing this by hand means three calls, one of them with a header name nobody
guesses (`x-auth-token`, not `Authorization`) — so it is a script, and
re-running it is safe.

    sudo docker exec -i mcphub python3 - < wire_mcphub.py --password …   # no
    python3 wire_mcphub.py --hub http://127.0.0.1:8781 --password … --all-groups

Run it from the NAS. `--password` is mcphub's ADMIN_PASSWORD; pass it in the
environment instead if you would rather it stayed out of `ps`:

    MCPHUB_ADMIN_PASSWORD=… python3 wire_mcphub.py --all-groups

The upstream URL is what *mcphub* can resolve, not what you can: both normal
deployments put this server and mcphub on the same Docker network, so the
container name is right.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

DEFAULT_HUB = "http://127.0.0.1:8781"
DEFAULT_NAME = "a2a"
DEFAULT_UPSTREAM = "http://a2a-mcp:8090/mcp"
DESCRIPTION = "Hand work to the registered A2A agents (list_agents, ask_agent)"


def call(hub: str, path: str, payload: dict | None, token: str | None = None) -> dict:
    request = urllib.request.Request(
        f"{hub.rstrip('/')}{path}",
        data=json.dumps(payload).encode() if payload is not None else None,
        method="POST" if payload is not None else "GET",
        headers={
            "Content-Type": "application/json",
            # mcphub's dashboard API reads the JWT from this header. The MCP
            # endpoints use Authorization; this one does not, and the failure
            # ("No token, authorization denied") does not say so.
            **({"x-auth-token": token} if token else {}),
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as e:
        return {"success": False, "http": e.code, "message": e.read().decode()[:300]}


def servers(token: str, hub: str) -> dict[str, dict]:
    """`/api/servers` as name → entry.

    It has answered with both a list of entries and a name-keyed object
    depending on version; read the roster either way rather than guessing.
    """
    data = call(hub, "/api/servers", None, token).get("data") or []
    if isinstance(data, dict):
        return data
    return {entry.get("name"): entry for entry in data if isinstance(entry, dict)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hub", default=DEFAULT_HUB, help="mcphub base URL")
    parser.add_argument("--user", default="admin")
    parser.add_argument(
        "--password",
        default=os.environ.get("MCPHUB_ADMIN_PASSWORD", ""),
        help="mcphub admin password (or set MCPHUB_ADMIN_PASSWORD)",
    )
    parser.add_argument("--name", default=DEFAULT_NAME, help="server name in mcphub")
    parser.add_argument("--upstream", default=DEFAULT_UPSTREAM, help="the MCP URL as *mcphub* sees it")
    parser.add_argument("--all-groups", action="store_true", help="offer it in every group")
    parser.add_argument("--groups", nargs="*", default=[], help="group names, if not all")
    args = parser.parse_args()

    if not args.password:
        print("no password: pass --password or MCPHUB_ADMIN_PASSWORD", file=sys.stderr)
        return 2

    login = call(args.hub, "/api/auth/login", {"username": args.user, "password": args.password})
    token = login.get("token")
    if not token:
        print(f"login failed: {login}", file=sys.stderr)
        return 1
    print(f"logged in as {login.get('user', {}).get('username')}")

    existing = servers(token, args.hub)
    config = {"type": "streamable-http", "url": args.upstream, "description": DESCRIPTION}
    if args.name in existing:
        print(f"server {args.name!r} exists; leaving its config alone ({config['url']})")
    else:
        created = call(args.hub, "/api/servers", {"name": args.name, "config": config}, token)
        if not created.get("success"):
            print(f"could not create {args.name!r}: {created}", file=sys.stderr)
            return 1
        print(f"created {args.name!r} → {args.upstream}")

    detail = call(args.hub, f"/api/servers/{args.name}", None, token).get("data") or {}
    tools = [t.get("name") if isinstance(t, dict) else t for t in detail.get("tools") or []]
    print(
        f"mcphub sees {len(tools)} tool(s) on it, status={detail.get('status')}: "
        f"{tools or '(none — check the upstream URL)'}"
    )

    groups = (call(args.hub, "/api/groups", None, token).get("data") or [])
    wanted = {g["name"] for g in groups} if args.all_groups else set(args.groups)
    if not wanted:
        print("no groups named: pass --all-groups or --groups …", file=sys.stderr)
        return 1

    for group in groups:
        members = [s["name"] for s in group.get("servers") or []]
        if group["name"] not in wanted:
            continue
        if args.name in members:
            print(f"  {group['name']}: already there")
            continue
        added = call(args.hub, f"/api/groups/{group['id']}/servers", {"serverName": args.name}, token)
        print(f"  {group['name']}: {'added' if added.get('success') else added}")

    for group in call(args.hub, "/api/groups", None, token).get("data") or []:
        members = [s["name"] for s in group.get("servers") or []]
        mark = "→" if args.name in members else " "
        print(f"{mark} {group['name']}: {', '.join(members)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
