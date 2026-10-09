"""hubtool on two hubs at once: one identity registered on both, its mail
travelling through either.

hub_history must recall the whole conversation from every hub on the
identity's list: merged, each message once, oldest to newest. It must page
back through both hubs without gaps or repeats, and say when a hub did not
answer. With no peer, each correspondent is listed once.

Two v2 hubs on loopback ports, each with its own database, and a throwaway
HOME (hubtool's identity store). Never the machine's hub: the environment
points hubtool at a dead address, so a slip fails instead of reaching it.

    python tests/v2/multi_hub.py --rust-bin PATH --database-url URL --database-url-b URL [--hubtool PATH]

--hubtool runs another copy of hubtool.py (the v2.0.0 one shows what this
test catches).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from typing import Any

import httpx

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.normpath(os.path.join(_HERE, "..", ".."))
sys.path.insert(0, _HERE)
import differential as dif  # noqa: E402

DEAD = "http://127.0.0.1:9"      # nothing listens there: a slip fails loudly
FAILS: list[str] = []
CHECKS = 0


def check(label: str, ok: bool, detail: Any = "") -> None:
    global CHECKS
    CHECKS += 1
    if not ok:
        FAILS.append(label)
    print(("  ok    " if ok else "  FAIL  ") + label + ("" if ok else f"\n          {str(detail)[:700]}"), flush=True)


def start_hub(rust_bin: str, data: str, url: str, name: str, port: int = 0) -> dif.Side:
    port = port or dif.free_port()
    env = dict(dif.clean_env(), HUB_DATABASE_URL=url, HUB_DATA=data, HUB_PORT=str(port), HUB_BIND="127.0.0.1", HUB_NAME=name)
    proc = subprocess.Popen([rust_bin], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return dif.wait_healthy(dif.Side(name, port, port, data, proc))


class Home:
    """The throwaway HOME and the hubtool.py under test."""

    def __init__(self, hubtool: str, home: str) -> None:
        self.hubtool = hubtool
        self.home = home
        self.env = dict(dif.clean_env(), HOME=home, USERPROFILE=home, MAILHUB_URL=DEAD, PYTHONIOENCODING="utf-8")
        self.env.pop("MAILHUB_NAME", None)

    def cli(self, *args: str, url: str = DEAD) -> dict[str, Any]:
        r = subprocess.run([sys.executable, self.hubtool, *args], env=dict(self.env, MAILHUB_URL=url), capture_output=True,
                           text=True, encoding="utf-8", timeout=90)
        try:
            return dict(json.loads(r.stdout.strip().splitlines()[-1]))
        except (ValueError, IndexError):
            return {"error": "no JSON answer", "stdout": r.stdout[-600:], "stderr": r.stderr[-600:]}

    def secret(self, name: str) -> str:
        con = sqlite3.connect(os.path.join(self.home, ".orgtree", "hub-clients", "clients.sqlite3"))
        try:
            return str(con.execute("SELECT uid FROM identities WHERE name=?", (name,)).fetchone()[0])
        finally:
            con.close()


class Mcp:
    """hubtool's MCP server over stdio: the path a session uses."""

    def __init__(self, home: Home) -> None:
        self.p = subprocess.Popen([sys.executable, home.hubtool], env=home.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.n = 0
        self.rpc("initialize")

    def rpc(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        assert self.p.stdin is not None and self.p.stdout is not None
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("the MCP server exited")
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return dict(msg)

    def tool(self, name: str, args: dict[str, Any] | None = None) -> Any:
        res = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        return json.loads(res["result"]["content"][0]["text"])

    def close(self) -> None:
        assert self.p.stdin is not None
        self.p.stdin.close()
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def previews(page: Any) -> list[str]:
    return [str(m.get("preview")) for m in (page.get("messages") or [])] if isinstance(page, dict) else [repr(page)]


def walk(mcp: Mcp, peer: str, limit: int, stop_after: int = 20) -> tuple[list[list[str]], list[Any]]:
    """Every page from the newest back, as hub_history gives them."""
    pages: list[list[str]] = []
    answers: list[Any] = []
    cursor = ""
    for _ in range(stop_after):
        page = mcp.tool("hub_history", {"peer": peer, "limit": limit, **({"cursor": cursor} if cursor else {})})
        answers.append(page)
        pages.append(previews(page))
        if not isinstance(page, dict) or not page.get("has_more") or not page.get("next_cursor"):
            break
        cursor = str(page["next_cursor"])
    return pages, answers


def run(rust_bin: str, url_a: str, url_b: str, hubtool: str, tmp: str) -> None:
    a = start_hub(rust_bin, os.path.join(tmp, "a"), url_a, "hub-a")
    b = start_hub(rust_bin, os.path.join(tmp, "b"), url_b, "hub-b")
    hub_a, hub_b = a.base(), b.base()
    home = Home(hubtool, os.path.join(tmp, "home"))
    mcp: Mcp | None = None
    try:
        # alice's first hub is a, bob's is b; both are on both
        alice = str(home.cli("register", "alice", url=hub_a).get("slug"))
        bob = str(home.cli("register", "bob", url=hub_b).get("slug"))
        lists = [home.cli("addhub", "alice", hub_b, url=hub_a).get("hubs"), home.cli("addhub", "bob", hub_a, url=hub_b).get("hubs")]
        check("setup · alice is on [a, b] and bob on [b, a]", lists == [[hub_a, hub_b], [hub_b, hub_a]], lists)

        # the conversation: each side sends through its own first hub
        via: dict[str, str] = {}
        ids: dict[str, str] = {}
        for who, to, body in (("alice", bob, "a1"), ("bob", alice, "b1"), ("alice", bob, "a2"), ("bob", alice, "b2"),
                              ("alice", bob, "a3"), ("bob", alice, "b3")):
            out = home.cli("send", who, to, body)
            via[body], ids[body] = str(out.get("via")), str(out.get("id"))
            time.sleep(0.02)
        check("setup · alice's mail went through a, bob's through b",
              all(via[x] == hub_a for x in ("a1", "a2", "a3")) and all(via[x] == hub_b for x in ("b1", "b2", "b3")), via)
        # one message held on both hubs (the same id through each), and one of
        # bob's through a
        dup = uuid.uuid4().hex
        for h in (hub_a, hub_b):
            r = httpx.post(h + "/api/send", headers={"x-org-auth": f"{alice}:{home.secret('alice')}"},
                           json={"id": dup, "to": bob, "body": "dup"})
            check(f"setup · the same message through {h}", r.status_code == 200, r.text)
            time.sleep(0.02)
        r = httpx.post(hub_a + "/api/send", headers={"x-org-auth": f"{bob}:{home.secret('bob')}"}, json={"to": alice, "body": "b4"})
        check("setup · one of bob's through a", r.status_code == 200, r.text)
        ids["b4"] = str(r.json().get("id"))
        whole = ["a1", "b1", "a2", "b2", "a3", "b3", "dup", "b4"]

        mcp = Mcp(home)
        reg = mcp.tool("hub_register", {"name": "alice"})
        check("alice's session resumes her address", reg.get("slug") == alice, reg)

        # 1. one conversation, from every hub, each message once
        hist = mcp.tool("hub_history", {"peer": bob})
        check("history · the whole conversation from both hubs, oldest to newest, each message once",
              previews(hist) == whole, hist)
        check("history · the start of the conversation is said, with no cursor",
              hist.get("has_more") is False and hist.get("next_cursor") is None and "start" in str(hist.get("note")), hist)

        # 2. paging back through both hubs: no gaps, no repeats
        for limit in (3, 2, 1):
            pages, answers = walk(mcp, bob, limit)
            flat = [p for page in reversed(pages) for p in page]
            check(f"history · pages of {limit} walk back through both hubs without gaps or repeats",
                  flat == whole and all(len(p) <= limit for p in pages), {"pages": pages, "last": answers[-1]})

        # 3. a cursor belongs to its conversation
        first = mcp.tool("hub_history", {"peer": bob, "limit": 2})
        other = mcp.tool("hub_history", {"peer": "someone.else.abcdef", "cursor": str(first.get("next_cursor"))})
        check("history · a cursor from one conversation is refused in another", "error" in other, other)

        # 4. with no peer: each correspondent once, unread counted on every hub
        convs = mcp.tool("hub_history", {})
        rows = [c for c in convs.get("conversations", []) if c.get("with") == bob]
        check("conversations · bob is listed once", len(rows) == 1, convs)
        check("conversations · his unread mail is counted on both hubs (b1-b3 on b, b4 on a)",
              len(rows) == 1 and rows[0].get("unread") == 4, rows)
        check("conversations · the last message is the newest on either hub",
              len(rows) == 1 and rows[0].get("last") == "b4", rows)

        # 5. a whole text held on the other hub only; history consumed nothing
        whole_b2 = mcp.tool("hub_message", {"id": ids["b2"]})
        check("hub_message · finds a message held on alice's second hub only", whole_b2.get("body") == "b2", whole_b2)
        read = mcp.tool("hub_read")
        check("history is read-only · hub_read still delivers bob's four messages",
              sorted(str(m.get("body")) for m in read.get("messages", [])) == ["b1", "b2", "b3", "b4"], read)

        # 6. a hub goes dark between pages, then for a whole recall
        page1 = mcp.tool("hub_history", {"peer": bob, "limit": 3})
        dif.stop(a)
        page2 = mcp.tool("hub_history", {"peer": bob, "limit": 3, "cursor": str(page1.get("next_cursor"))})
        seen_b = [p for p in previews(page1) + previews(page2) if p in ("b1", "b2", "b3", "dup")]
        check("hub a stops between pages · every message hub b holds is shown once across the two pages",
              sorted(seen_b) == ["b1", "b2", "b3", "dup"], {"page1": page1, "page2": page2})
        dark = mcp.tool("hub_history", {"peer": bob})
        check("hub a is down · the page holds hub b's mail", previews(dark) == ["b1", "b2", "b3", "dup"], dark)
        check("hub a is down · the note names hub a", hub_a in str(dark.get("note")), dark)
        check("hub a is down · the note does not claim the start of the conversation",
              "start of your mail" not in str(dark.get("note")), dark)
        convs = mcp.tool("hub_history", {})
        check("hub a is down · the conversation list still names bob, and says hub a did not answer",
              [c.get("with") for c in convs.get("conversations", [])].count(bob) == 1 and hub_a in json.dumps(convs), convs)

        # 7. back up: the whole conversation again
        a = start_hub(rust_bin, os.path.join(tmp, "a"), url_a, "hub-a", port=a.port)
        back = mcp.tool("hub_history", {"peer": bob})
        check("hub a is back · the whole conversation again", previews(back) == whole, back)
    finally:
        if mcp:
            mcp.close()
        for side in (a, b):
            if side.proc.poll() is None:
                dif.stop(side)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust-bin", required=True)
    ap.add_argument("--database-url", required=True)
    ap.add_argument("--database-url-b", required=True)
    ap.add_argument("--hubtool", default=os.path.join(_REPO, "hubtool.py"))
    args = ap.parse_args()
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    print(f"hubtool on two hubs: {args.hubtool}")
    tmp = tempfile.mkdtemp(prefix="hub-multi-")
    try:
        run(args.rust_bin, args.database_url, args.database_url_b, args.hubtool, tmp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"hubtool on two hubs: {CHECKS - len(FAILS)} of {CHECKS} checks passed")
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
