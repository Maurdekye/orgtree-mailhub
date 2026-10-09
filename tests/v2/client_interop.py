"""A real client against both hubs: hubtool.py, the Claude Code chat client.

The same session — CLI verbs, the listener, and the MCP server over stdio —
runs once against the v1 Python hub and once against the v2 binary, each
with its own throwaway HOME (hubtool's identity store) on loopback ports,
never the machine's hub. Each run asserts what the client should see; the
two transcripts are then compared with addresses, ids and times replaced by
symbols.

    python tests/v2/client_interop.py --rust-bin PATH --database-url URL
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from typing import Any

import httpx

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.normpath(os.path.join(_HERE, "..", ".."))
sys.path.insert(0, _HERE)
import differential as dif  # noqa: E402

HUBTOOL = os.path.join(_REPO, "hubtool.py")
TS = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?")
HEX32 = re.compile(r"\b[0-9a-f]{32}\b")


class Session:
    def __init__(self, side: dif.Side, home: str) -> None:
        self.side = side
        self.home = home
        self.env = dict(dif.clean_env(), HOME=home, USERPROFILE=home, MAILHUB_URL=side.base(), PYTHONIOENCODING="utf-8")
        self.env.pop("MAILHUB_NAME", None)
        self.transcript: list[Any] = []
        self.slugs: dict[str, str] = {}
        self.failures: list[str] = []

    def cli(self, *args: str, timeout: float = 30) -> str:
        r = subprocess.run([sys.executable, HUBTOOL, *args], env=self.env, capture_output=True, text=True,
                           encoding="utf-8", timeout=timeout)
        return r.stdout.strip() or json.dumps({"error": "no output", "stderr": r.stderr.strip()[-600:], "rc": r.returncode})

    def check(self, label: str, ok: bool, detail: Any = "") -> None:
        if not ok:
            self.failures.append(f"{self.side.name}: {label} — {detail}")

    def note(self, label: str, value: Any) -> None:
        self.transcript.append([label, value])

    def secret(self, name: str) -> str:
        con = sqlite3.connect(os.path.join(self.home, ".orgtree", "hub-clients", "clients.sqlite3"))
        try:
            return str(con.execute("SELECT uid FROM identities WHERE name=?", (name,)).fetchone()[0])
        finally:
            con.close()

    def norm(self, v: Any) -> Any:
        if isinstance(v, str):
            for name, slug in sorted(self.slugs.items(), key=lambda kv: -len(kv[1])):
                v = v.replace(slug, f"<{name}>")
            v = v.replace(self.side.base(), "<hub>")
            v = HEX32.sub("<id>", v)
            return TS.sub("<ts>", v)
        if isinstance(v, list):
            return [self.norm(x) for x in v]
        if isinstance(v, dict):
            return {k: self.norm(x) for k, x in v.items()}
        return v


def run(s: Session) -> None:
    base = s.side.base()
    # ── CLI verbs
    for name in ("interop-one", "interop-two"):
        out = json.loads(s.cli("register", name))
        s.slugs[name] = out.get("slug", "")
        s.check(f"register {name}", bool(out.get("slug")) and out.get("hub") == "diff-hub" and "error" not in out, out)
        s.note(f"register {name}", {k: out.get(k) for k in ("hub", "error")} | {"roster": sorted(r["slug"] for r in out.get("roster") or [])})
    one, two = s.slugs["interop-one"], s.slugs["interop-two"]
    out = json.loads(s.cli("register", "interop-one"))
    s.check("register again resumes", "resumed" in out, out)
    s.note("register again", sorted(out))
    listing = s.cli("list", "interop-one")
    s.check("list shows both chats", one in listing and two in listing and "[chat]" in listing, listing)
    s.note("list", sorted(ln.split("  ")[0] + "  " + ln.split("  ")[1] for ln in listing.splitlines()))
    out = json.loads(s.cli("send", "interop-one", two, "hello", "from", "one"))
    s.check("send", out.get("duplicate") is False and out.get("id"), out)
    s.note("send", out)
    out = json.loads(s.cli("send", "interop-one", "zz.nobody.ffffff", "lost"))
    s.check("send to nobody is refused", "error" in out, out)
    s.note("send to nobody", out)
    # an attachment (hubtool has no upload verb: upload the way an org does)
    sec_one = s.secret("interop-one")
    up = httpx.post(base + "/api/attachments", params={"name": "report é.txt"}, headers={"x-org-auth": f"{one}:{sec_one}"},
                    content=b"attached bytes").json()
    sent = httpx.post(base + "/api/send", headers={"x-org-auth": f"{one}:{sec_one}"},
                      json={"to": two, "body": "with a file", "attachments": [up["id"]]}).json()
    s.check("attachment upload and send", "id" in sent, sent)
    # ── the listener: prints, acks, sends delivered receipts
    lst = subprocess.Popen([sys.executable, HUBTOOL, "listen", "interop-two"], env=s.env, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, text=True, encoding="utf-8")
    lines: list[str] = []
    deadline = time.time() + 30
    assert lst.stdout is not None
    while time.time() < deadline and not any("with a file" in ln for ln in lines):
        ln = lst.stdout.readline()
        if not ln:
            break
        lines.append(ln.rstrip())
    time.sleep(2.0)            # the ack and the receipt follow the print
    lst.terminate()
    try:
        lst.wait(10)
    except subprocess.TimeoutExpired:
        lst.kill()
    s.check("the listener printed both messages", any("hello from one" in ln for ln in lines) and any("with a file" in ln for ln in lines),
            lines)
    s.note("listener", [ln for ln in lines if "hello" in ln or "file" in ln])
    view = httpx.get(base + "/ui/messages", params={"org": two}).json()["messages"]
    states = sorted((m["body"], m["state"], bool(m["delivered_at"])) for m in view if m["to"] == two)
    s.check("custody taken and delivery receipted by the listener", all(st == "fetched" and dl for _, st, dl in states), states)
    s.note("hub view after listen", states)
    # ── fetch: the download, named from Content-Disposition
    dest = os.path.join(s.home, "fetched")
    os.makedirs(dest, exist_ok=True)
    out = json.loads(s.cli("fetch", "interop-two", up["id"], dest))
    path = out.get("saved") or ""
    s.check("fetch saved the bytes", os.path.isfile(path) and open(path, "rb").read() == b"attached bytes", out)
    # hubtool parses only filename="…"; an RFC 5987 filename*= (a non-ASCII
    # name) falls back to the attachment id, on either hub
    s.note("fetch (non-ASCII name)", {"name": s.norm(os.path.basename(path)), "bytes": out.get("bytes")})
    up2 = httpx.post(base + "/api/attachments", params={"name": "plain-report.txt"}, headers={"x-org-auth": f"{one}:{sec_one}"},
                     content=b"plain bytes").json()
    httpx.post(base + "/api/send", headers={"x-org-auth": f"{one}:{sec_one}"},
               json={"to": two, "body": "another file", "attachments": [up2["id"]]})
    out = json.loads(s.cli("fetch", "interop-two", up2["id"], dest))
    path = out.get("saved") or ""
    s.check("fetch saved an ASCII-named file under its sent name",
            os.path.basename(path) == "plain-report.txt" and open(path, "rb").read() == b"plain bytes", out)
    s.note("fetch (ASCII name)", {"name": os.path.basename(path), "bytes": out.get("bytes")})
    # ── the MCP server over stdio
    mcp = subprocess.Popen([sys.executable, HUBTOOL], env=s.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")
    assert mcp.stdin is not None and mcp.stdout is not None

    def rpc(i: int, method: str, params: dict[str, Any] | None = None) -> Any:
        mcp.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}}) + "\n")
        mcp.stdin.flush()
        return json.loads(mcp.stdout.readline())

    rpc(1, "initialize")
    tools = rpc(2, "tools/list")["result"]["tools"]
    s.note("mcp tools", sorted(t["name"] for t in tools))

    def call(i: int, tool: str, args: dict[str, Any]) -> Any:
        res = rpc(i, "tools/call", {"name": tool, "arguments": args})
        return json.loads(res["result"]["content"][0]["text"])

    reg = call(3, "hub_register", {"name": "interop-three"})
    s.slugs["interop-three"] = reg.get("slug", "")
    s.check("mcp register", bool(reg.get("slug")), reg)
    s.note("mcp register", {k: reg.get(k) for k in ("hub",)})
    sent = call(4, "hub_send", {"to": one, "body": "from three via mcp"})
    s.check("mcp send", "id" in sent, sent)
    roster = call(5, "hub_list", {})
    s.note("mcp list", sorted([r.get("slug"), r.get("kind")] for r in (roster if isinstance(roster, list) else roster.get("roster", []))))
    mcp.stdin.close()
    mcp.wait(10)
    mcp2 = subprocess.Popen([sys.executable, HUBTOOL], env=s.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")
    assert mcp2.stdin is not None and mcp2.stdout is not None
    mcp = mcp2
    rpc(1, "initialize")
    call(2, "hub_register", {"name": "interop-one"})
    got = call(3, "hub_wait", {"timeout": 5})
    msgs = got if isinstance(got, list) else got.get("messages", got)
    s.check("mcp wait receives the mail", "from three via mcp" in json.dumps(msgs), got)
    s.note("mcp wait", s.norm(json.dumps(msgs, sort_keys=True)))
    # ── hub_history: a v2 hub keeps the conversation; v1 has no history route
    hist = call(4, "hub_history", {"peer": two})
    convs = call(5, "hub_history", {})
    if s.side.name == "rust":
        rows = hist.get("messages", [])
        s.check("history: one conversation, oldest to newest, all sent, from the start",
                [m.get("preview") for m in rows] == ["hello from one", "with a file", "another file"]
                and all(m.get("direction") == "sent" for m in rows) and hist.get("has_more") is False
                and hist.get("next_cursor") is None and "This is the start" in str(hist.get("note")), hist)
        s.check("history: a message's files are named",
                [a.get("name") for a in rows[1].get("attachments", [])] == ["report é.txt"] if len(rows) > 1 else False, hist)
        page = call(6, "hub_history", {"peer": two, "limit": 2})
        s.check("history: a page of 2 is the newest two, with a cursor",
                [m.get("preview") for m in page.get("messages", [])] == ["with a file", "another file"]
                and page.get("has_more") is True and bool(page.get("next_cursor")), page)
        older = call(7, "hub_history", {"peer": two, "limit": 2, "cursor": page.get("next_cursor")})
        s.check("history: cursor= gives the page before it, and the start ends the paging",
                [m.get("preview") for m in older.get("messages", [])] == ["hello from one"] and older.get("has_more") is False, older)
        s.check("history: with no peer, who this address has mail with",
                sorted(c.get("with") for c in convs.get("conversations", [])) == sorted([two, s.slugs["interop-three"]]), convs)
        whole = call(8, "hub_message", {"id": rows[0].get("id") if rows else ""})
        s.check("hub_message: a message's whole text", whole.get("body") == "hello from one" and whole.get("chars") == 14, whole)
        wait = call(9, "hub_wait", {"timeout": 1})
        s.check("history consumed nothing (hub_wait has nothing new)", not wait.get("messages"), wait)
    else:
        whole = call(8, "hub_message", {"id": "anything"})
        s.check("history and whole texts on a v1 hub say they need v2",
                "v2.0" in str(hist.get("error")) and "v2.0" in str(convs.get("error")) and "v2.0" in str(whole.get("error")),
                (hist, convs, whole))
    s.note("mcp history", "checked per hub")
    mcp.stdin.close()
    mcp.wait(10)
    # ── unregister
    out = json.loads(s.cli("unregister", "interop-one"))
    s.check("unregister", "error" not in out, out)
    s.note("unregister", out)
    roster = httpx.get(base + "/ui/data").json()["orgs"]
    s.check("gone from the roster", one not in [r["slug"] for r in roster], roster)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust-bin", required=True)
    ap.add_argument("--database-url", required=True)
    args = ap.parse_args()
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    tmp = tempfile.mkdtemp(prefix="hub-client-")
    sessions = []
    try:
        for name, start in (("python", lambda d: dif.start_python(d)), ("rust", lambda d: dif.start_rust(args.rust_bin, d, args.database_url))):
            data, home = os.path.join(tmp, name, "data"), os.path.join(tmp, name, "home")
            os.makedirs(data)
            os.makedirs(home)
            side = start(data)
            s = Session(side, home)
            try:
                run(s)
            finally:
                dif.stop(side)
            sessions.append(s)
            print(f"{name}: {len(s.transcript)} transcript entries, {len(s.failures)} failed checks")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    failures = [f for s in sessions for f in s.failures]
    a, b = (s.norm(s.transcript) for s in sessions)
    for (la, va), (lb, vb) in zip(a, b):
        if json.dumps(va, sort_keys=True, ensure_ascii=False) != json.dumps(vb, sort_keys=True, ensure_ascii=False):
            failures.append(f"transcripts differ at {la!r}:\n      python: {va}\n      rust:   {vb}")
    if len(a) != len(b):
        failures.append(f"transcript lengths differ: {len(a)} / {len(b)}")
    for f in failures:
        print(f"✗ {f}")
    print("hubtool interop: " + ("all checks passed, transcripts agree" if not failures else f"{len(failures)} problems"))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
