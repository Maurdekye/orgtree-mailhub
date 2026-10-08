"""Differential check: the v1 Python hub and the v2 Rust hub, side by side.

Both hubs start fresh on loopback ports (never a real hub's). Every step
sends the SAME request to both and compares what a client can observe:
status, content type, the headers that matter, and the body — JSON with
hub-clock timestamps and per-hub minted ids replaced by symbols, key order
included; anything else byte for byte. Their stdout request lines are
compared too.

A difference that is not listed in EXPECTED (the documented deliberate
differences) fails the run; an EXPECTED difference that does not occur is
reported as well.

    python tests/v2/differential.py --rust-bin PATH --database-url URL [-v]

The database must be empty (the Rust hub creates its schema). Needs fastapi,
uvicorn and httpx for the reference hub and the driver.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from typing import Any, Callable

import httpx

_HERE = os.path.dirname(os.path.abspath(__file__))
TS = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z")
LIMIT = 4096  # both hubs start with HUB_MAX_FILE_BYTES=4096

# step id -> why v2 deliberately differs (docs/v2.md carries the same list)
EXPECTED = {
    "h10-docs": "FastAPI's generated /docs, /redoc and /openapi.json are not served by v2",
    "h11-openapi": "FastAPI's generated /docs, /redoc and /openapi.json are not served by v2",
    "s19-nul-body-poll": "PostgreSQL cannot store U+0000: v2 removes it from stored text",
    "log-startup": "v1 printed its startup line once per listener (the public listener ran the app's lifespan again); v2 prints it once",
    "u7-bad-cursor": "a before_at that is not a timestamp: v1 compared it as a string, v2 refuses it (422)",
    "log-500": "v2 also writes a request line for a 500 (v1's middleware never saw the crash)",
    "h2-index": "the operator page also tags the person kind (an addition: docs/v2-additions.md)",
}


# keys v2's /healthz adds beside v1's (checked present, then set aside)
HEALTHZ_ADDITIONS = ("version", "features")


def ID(r: Any) -> str:
    return str(r.json()["id"])


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def fp6(secret: str) -> str:
    return hashlib.sha256(secret.encode()).hexdigest()[:6]


class Raw:
    """A response read off a raw socket (paths httpx would normalize)."""

    def __init__(self, data: bytes) -> None:
        head, _, body = data.partition(b"\r\n\r\n")
        lines = head.decode("latin-1").split("\r\n")
        self.status_code = int(lines[0].split()[1])
        self.headers = {}
        for ln in lines[1:]:
            k, _, v = ln.partition(":")
            self.headers[k.strip().lower()] = v.strip()
        self.content = body


class Side:
    def __init__(self, name: str, port: int, public_port: int, data: str, proc: subprocess.Popen[bytes]) -> None:
        self.name = name
        self.port = port
        self.public_port = public_port
        self.data = data
        self.proc = proc
        self.client = httpx.Client(timeout=90)
        self.vars: dict[str, str] = {}
        self.lines: list[dict[str, Any]] = []

    def base(self, public: bool = False) -> str:
        return f"http://127.0.0.1:{self.public_port if public else self.port}"

    def set(self, name: str, value: str) -> None:
        self.vars[name] = value

    def raw(self, method: str, path: str, headers: dict[str, str], public: bool) -> Raw:
        port = self.public_port if public else self.port
        with socket.create_connection(("127.0.0.1", port), timeout=30) as s:
            hdr = "".join(f"{k}: {v}\r\n" for k, v in headers.items())
            s.sendall(f"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{hdr}\r\n".encode("latin-1"))
            buf = b""
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
        r = Raw(buf)
        if r.headers.get("transfer-encoding", "").lower() == "chunked":
            body, rest = b"", r.content
            while rest:
                size_line, _, rest = rest.partition(b"\r\n")
                size = int(size_line.split(b";")[0], 16)
                if size == 0:
                    break
                body += rest[:size]
                rest = rest[size + 2:]
            r.content = body
        return r

    def norm(self, v: Any, keep_nul: bool = False, exact: bool = False, key: str = "") -> Any:
        """Symbols for per-hub ids; `<ts>` for hub-clock times — except in
        EXACT comparisons, where only a roster's `last_seen` (stamped by the
        very call being compared) is replaced."""
        if isinstance(v, str):
            for name, val in sorted(self.vars.items(), key=lambda kv: -len(kv[1])):
                if val and len(val) >= 6 and val in v:
                    v = v.replace(val, f"<{name}>")
            if not keep_nul:
                # the one deliberate content difference (v2 cannot store
                # U+0000); its own step keeps it visible, later views do not
                # count it again
                v = v.replace("\u0000", "")
            return TS.sub("<ts>", v) if (not exact or key == "last_seen") else v
        if isinstance(v, list):
            return [self.norm(x, keep_nul, exact, key) for x in v]
        if isinstance(v, dict):
            return {k: self.norm(x, keep_nul, exact, k) for k, x in v.items()}
        return v


Arg = Any  # a value, or a callable taking the Side


def resolve(a: Arg, side: Side) -> Any:
    return a(side) if callable(a) else a


class Diff:
    def __init__(self, py: Side, rs: Side, verbose: bool) -> None:
        self.sides = (py, rs)
        self.verbose = verbose
        self.same = 0
        # answers where v2's additive keys were checked and set aside
        self.additions = 0
        self.unexpected: list[str] = []
        self.expected_seen: dict[str, str] = {}
        # (path, python status, rust status) of deliberate differences, so
        # the request-log comparison can recognise their lines
        self.divergent: set[tuple[str, int, int]] = set()

    def step(self, sid: str, method: str, path: Arg, *, auth: Arg = None, body: Arg = None, content: Arg = None,
             headers: Arg = None, public: bool = False, raw: bool = False,
             compare_headers: tuple[str, ...] = (), capture: dict[str, Callable[[Any], str]] | None = None, exact: bool = False) -> tuple[Any, Any]:
        """Send one request to both hubs and compare the answers. `capture`
        names values a hub minted (ids) so later steps and comparisons
        refer to them symbolically."""
        out = []
        for s in self.sides:
            p = resolve(path, s)
            h = dict(resolve(headers, s) or {})
            a = resolve(auth, s)
            if a:
                h["x-org-auth"] = a
            if raw:
                out.append(s.raw(method, p, h, public))
                continue
            kw: dict[str, Any] = {}
            b = resolve(body, s)
            if b is not None:
                kw["json"] = b
            c = resolve(content, s)
            if c is not None:
                kw["content"] = c
            out.append(s.client.request(method, s.base(public) + p, headers=h, follow_redirects=False, **kw))
        for name, fn in (capture or {}).items():
            for s, r in zip(self.sides, out):
                try:
                    s.set(name, fn(r))
                except Exception:  # noqa: BLE001 — a failed answer simply has nothing to capture
                    pass
        self.compare(sid, out[0], out[1], compare_headers, exact)
        if sid in EXPECTED and out[0].status_code != out[1].status_code:
            p = urllib.parse.unquote(str(resolve(path, self.sides[0])).split("?")[0])
            self.divergent.add((p, out[0].status_code, out[1].status_code))
        return out[0], out[1]

    def view(self, side: Side, r: Any, extra: tuple[str, ...], keep_nul: bool, exact: bool = False) -> dict[str, Any]:
        ctype = r.headers.get("content-type", "")
        body = r.content.decode("utf-8", "replace")
        m = re.match(r"multipart/byteranges; boundary=(\w+)$", ctype)
        if m:
            ctype = ctype.replace(m.group(1), "<boundary>")
            body = body.replace(m.group(1), "<boundary>")
        v: dict[str, Any] = {"status": r.status_code, "content-type": ctype}
        for h in extra:
            val = r.headers.get(h)
            if val is not None and h == "location":
                val = val.replace(side.base(False), "<base>").replace(side.base(True), "<base>")
            if m and val is not None and h == "content-type":
                val = ctype
            v[h] = side.norm(val, keep_nul, exact) if isinstance(val, str) else val
        if ctype.startswith("application/json"):
            try:
                v["body"] = side.norm(json.loads(r.content), keep_nul, exact)
            except ValueError:
                v["body"] = body
        else:
            v["body"] = body
        return v

    def set_aside_additions(self, va: dict[str, Any], vb: dict[str, Any]) -> None:
        """v2's /healthz also says what it supports (`version`, `features`:
        docs/v2-additions.md). Present on v2 and absent on v1, they are set
        aside so the rest of the answer still compares exactly."""
        ba, bb = va.get("body"), vb.get("body")
        if not (isinstance(ba, dict) and isinstance(bb, dict) and "max_attachment_bytes" in bb):
            return
        if all(k in bb and k not in ba for k in HEALTHZ_ADDITIONS) and isinstance(bb["features"], list):
            for k in HEALTHZ_ADDITIONS:
                bb.pop(k)
            self.additions += 1

    def compare(self, sid: str, a: Any, b: Any, extra: tuple[str, ...], exact: bool = False) -> None:
        keep_nul = sid.startswith("s19")
        va = self.view(self.sides[0], a, extra, keep_nul, exact)
        vb = self.view(self.sides[1], b, extra, keep_nul, exact)
        self.set_aside_additions(va, vb)
        ja, jb = json.dumps(va, ensure_ascii=False), json.dumps(vb, ensure_ascii=False)
        same = ja == jb
        if not same:
            # show where they part, not just their first lines
            i = next((k for k, (x, y) in enumerate(zip(ja, jb)) if x != y), min(len(ja), len(jb)))
            lo = max(0, i - 300)
            va = {"…": ja[lo:i + 300]}
            vb = {"…": jb[lo:i + 300]}
        if same:
            self.same += 1
            if sid in EXPECTED:
                print(f"  ?  {sid}: listed as a deliberate difference but both hubs agree")
            elif self.verbose:
                print(f"  =  {sid}")
            return
        detail = f"python: {json.dumps(va, ensure_ascii=False)[:700]}\n      rust:   {json.dumps(vb, ensure_ascii=False)[:700]}"
        if sid in EXPECTED:
            self.expected_seen[sid] = detail
            print(f"  ~  {sid}  (deliberate: {EXPECTED[sid]})")
            if self.verbose:
                print("      " + detail)
        else:
            self.unexpected.append(f"{sid}\n      {detail}")
            print(f"  ✗  {sid}\n      {detail}")


def clean_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if not k.startswith("HUB_")}


def start_python(data: str) -> Side:
    port, public = free_port(), free_port()
    proc = subprocess.Popen([sys.executable, os.path.join(_HERE, "reference_hub.py"), "--port", str(port), "--public-port", str(public),
                             "--data", data, "--max-file-bytes", str(LIMIT)], env=clean_env(), stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL)
    return wait_healthy(Side("python", port, public, data, proc))


def start_rust(rust_bin: str, data: str, database_url: str) -> Side:
    port, public = free_port(), free_port()
    env = dict(clean_env(), HUB_DATABASE_URL=database_url, HUB_DATA=data, HUB_PORT=str(port), HUB_BIND="127.0.0.1", HUB_PUBLIC="1",
               HUB_PUBLIC_BIND="127.0.0.1", HUB_PUBLIC_PORT=str(public), HUB_NAME="diff-hub", HUB_RETENTION_DAYS="30",
               HUB_MAX_FILE_BYTES=str(LIMIT))
    proc = subprocess.Popen([rust_bin], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return wait_healthy(Side("rust", port, public, data, proc))


def wait_healthy(s: Side) -> Side:
    threading.Thread(target=read_lines, args=(s,), daemon=True).start()
    deadline = time.time() + 120
    while True:
        try:
            if httpx.get(s.base() + "/healthz", timeout=2).status_code == 200:
                return s
        except httpx.HTTPError:
            pass
        if s.proc.poll() is not None or time.time() > deadline:
            raise SystemExit(f"the {s.name} hub did not start")
        time.sleep(0.2)


def stop(s: Side) -> None:
    s.proc.terminate()
    try:
        s.proc.wait(10)
    except subprocess.TimeoutExpired:
        s.proc.kill()
        s.proc.wait(10)


def start_hubs(rust_bin: str, database_url: str) -> tuple[Side, Side, str]:
    tmp = tempfile.mkdtemp(prefix="hub-diff-")
    pd, rd = os.path.join(tmp, "py"), os.path.join(tmp, "rs")
    os.makedirs(pd)
    os.makedirs(rd)
    return start_python(pd), start_rust(rust_bin, rd, database_url), tmp


def read_lines(side: Side) -> None:
    assert side.proc.stdout is not None
    for line in side.proc.stdout:
        try:
            side.lines.append(json.loads(line))
        except ValueError:
            pass


def scenarios(d: Diff) -> None:
    py, rs = d.sides

    def me(name: str, secret: str) -> tuple[str, str]:
        return (f"{name}.tester.{fp6(secret)}", secret)

    def pair(o: tuple[str, str]) -> str:
        return f"{o[0]}:{o[1]}"

    alice, bob, carol = me("alice", "secret-alice-1"), me("bob", "secret-bob-1"), me("carol", "secret-carol-1")
    eve, zed, dave = me("eve", "secret-eve-1"), me("zed", "secret-zed-1"), (f"dave-chat.ncola-k8bx.{fp6('secret-dave')}", "secret-dave")
    print("\nhealth, the operator page, routing")
    d.step("h1-healthz", "GET", "/healthz")
    d.step("h2-index", "GET", "/")
    d.step("h3-ui-data", "GET", "/ui/data")
    d.step("h4-ui-messages", "GET", "/ui/messages")
    d.step("h5-head", "HEAD", "/healthz", compare_headers=("allow",))
    d.step("h6-post-healthz", "POST", "/healthz", compare_headers=("allow",))
    d.step("h7-unknown", "GET", "/nope")
    d.step("h8-slash", "GET", "/healthz/", compare_headers=("location",))
    d.step("h9-slash-api", "GET", "/api/roster/?x=1", compare_headers=("location",))
    d.step("h10-docs", "GET", "/docs")
    d.step("h11-openapi", "GET", "/openapi.json")
    d.step("h12-delete", "DELETE", "/api/send", compare_headers=("allow",))

    print("\nregistration")
    d.step("r1-alice", "POST", "/api/register", auth=pair(alice),
           body={"slug": alice[0], "org_name": "Alice Org", "username": "tester", "blurb": "first"})
    for o, nm in ((bob, "Bob"), (carol, "Carol"), (eve, "Eve"), (zed, "Zed")):
        d.step(f"r2-{nm.lower()}", "POST", "/api/register", auth=pair(o), body={"slug": o[0], "org_name": nm, "username": "tester"})
    d.step("r2-dave-chat", "POST", "/api/register", auth=pair(dave),
           body={"slug": dave[0], "org_name": "dave-chat", "username": "ncola_k8bx", "kind": "chat", "blurb": "independent Claude Code chat"})
    d.step("r3-refresh", "POST", "/api/register", auth=pair(alice),
           body={"slug": alice[0], "org_name": "Alice Renamed", "username": "alice2", "blurb": "second", "kind": "chat"})
    d.step("r4-wrong-secret", "POST", "/api/register", auth=f"{alice[0]}:wrong", body={"slug": alice[0]})
    d.step("r5-no-header", "POST", "/api/register", body={"slug": "zz.nobody.aaaaaa"})
    d.step("r6-other-slug", "POST", "/api/register", auth=pair(bob), body={"slug": "zz.victim.aaaaaa"})
    for i, bad in enumerate(["", "   ", "UPPER.case.aaaaaa", ".leading.dot", "-dash", "has space", "a" * 129, "emoji.\U0001f600.x"]):
        d.step(f"r7-malformed-{i}", "POST", "/api/register", auth=f"x:{'s' * 32}", body={"slug": bad})
    pad = me("padded", "secret-pad")
    d.step("r8-padded", "POST", "/api/register", auth=pair(pad), body={"slug": f"  {pad[0]}\t", "org_name": "Pad"})
    for i, kind in enumerate(["PERSON", 5, None, "CHAT"]):  # "person" itself is a v2 kind (docs/v2-additions.md)
        o = me(f"kind{i}", f"secret-kind-{i}")
        d.step(f"r9-kind-{i}", "POST", "/api/register", auth=pair(o), body={"slug": o[0], "kind": kind})
    for i, raw in enumerate([b"not json", b"[1, 2]", b"null", b"", b"\"text\"", b"{\"slug\": "]):
        d.step(f"r10-bad-body-{i}", "POST", "/api/register", auth=pair(alice), content=raw)
    d.step("r11-numeric-slug", "POST", "/api/register", auth="5:secret-five", body={"slug": 5})
    o = me("types", "secret-types")
    d.step("r12-field-types", "POST", "/api/register", auth=pair(o), body={"slug": o[0], "org_name": 123, "blurb": True, "username": None})
    d.step("r13-last-pair-empty", "POST", "/api/register", auth=f"{alice[0]}:secret-alice-1 {alice[0]}:", body={"slug": alice[0]})
    d.step("r14-text-plain", "POST", "/api/register", auth=pair(bob), content=json.dumps({"slug": bob[0], "org_name": "Bob"}).encode(),
           headers={"content-type": "text/plain"})

    print("\npoll")
    d.step("p1-no-auth", "POST", "/api/poll?wait=0")
    d.step("p2-bad-wait", "POST", "/api/poll?wait=abc", auth=pair(alice))
    d.step("p3-empty", "POST", "/api/poll?wait=0", auth=pair(alice), body={})
    d.step("p4-no-body", "POST", "/api/poll?wait=0", auth=pair(alice))
    d.step("p5-one-invalid", "POST", "/api/poll?wait=0", auth=f"{pair(alice)} zz.nope.aaaaaa:x {bob[0]}:wrong")
    d.step("p6-negative-wait", "POST", "/api/poll?wait=-1", auth=pair(alice))
    d.step("p7-empty-wait", "POST", "/api/poll?wait=", auth=pair(alice))
    d.step("p8-unauth-bad-wait", "POST", "/api/poll?wait=x")

    print("\nsend")
    d.step("s1-unknown", "POST", "/api/send", auth=pair(alice), body={"to": "zz.nobody.ffffff", "body": "x"})
    d.step("s2-quote", "POST", "/api/send", auth=pair(alice), body={"to": "it's", "body": "x"})
    d.step("s2b-both-quotes", "POST", "/api/send", auth=pair(alice), body={"to": "a'b\"c\\d\ne", "body": "x"})
    d.step("s3-forged-from", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "from": carol[0], "body": "x"})
    d.step("s4-no-creds", "POST", "/api/send", body={"to": bob[0], "body": "x"})
    a, b = d.step("s5-first", "POST", "/api/send", auth=pair(alice), body={"id": "m1-fixed", "to": bob[0], "body": "hello bob", "kind": "message",
                                                                       "sent_at": "2020-01-01T00:00:00Z"})
    first = (a.json(), b.json())
    a, b = d.step("s6-duplicate", "POST", "/api/send", auth=pair(alice), body={"id": "m1-fixed", "to": bob[0], "body": "DIFFERENT"})
    for side, f, again in ((py, first[0], a.json()), (rs, first[1], b.json())):
        if f.get("received_at") != again.get("received_at"):
            d.unexpected.append(f"s6-duplicate: {side.name} answered a retry with a new received_at")
    d.step("s7-long", "POST", "/api/send", auth=pair(alice), body={"id": "m2-long", "to": bob[0], "body": "é" * 25000})
    for i, v in enumerate([123, True, {"a": 1, "b": [None, "x"]}, [1, 2], False, 0, 1.5, 1e20, "ok"]):
        d.step(f"s8-body-{i}", "POST", "/api/send", auth=pair(alice), body={"id": f"m3-body-{i}", "to": bob[0], "body": v})
    for i, (k, t, sa) in enumerate([(5, True, 1.5), (None, None, None), ("status", "th-1", "yesterday"), (False, 0, -3)]):
        d.step(f"s9-meta-{i}", "POST", "/api/send", auth=pair(alice), body={"id": f"m4-meta-{i}", "to": bob[0], "body": "meta", "kind": k,
                                                                         "thread_id": t, "sent_at": sa})
    d.step("s10-kind-list", "POST", "/api/send", auth=pair(alice), body={"id": "m5", "to": bob[0], "body": "x", "kind": ["a"]})
    d.step("s11-eleven", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "body": "x", "attachments": [f"a{i}" for i in range(11)]})
    d.step("s12-att-string", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "body": "x", "attachments": "abc"})
    d.step("s13-att-dict", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "body": "x", "attachments": [{"id": "x", "n": 1.0}]})
    d.step("s14-att-number", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "body": "x", "attachments": 5})
    d.step("s15-padded-to", "POST", "/api/send", auth=pair(alice), body={"id": "m6-padded", "to": f" {bob[0]}\n", "body": "padded"})
    d.step("s16-numeric-from", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "from": 123, "body": "x"})
    d.step("s17-minted-id", "POST", "/api/send", auth=pair(alice), body={"to": bob[0], "body": "no id"}, capture={"minted1": ID})
    d.step("s18-nul-to", "POST", "/api/send", auth=pair(alice), body={"to": "x\u0000y", "body": "x"})
    d.step("s19-nul-body", "POST", "/api/send", auth=pair(alice), body={"id": "m7-nul", "to": carol[0], "body": "a\u0000b"})
    d.step("s19-nul-body-poll", "POST", "/api/poll?wait=0", auth=pair(carol))
    d.step("s20-multi-from", "POST", "/api/send", auth=f"{pair(alice)} {pair(carol)}",
           body={"id": "m8-multi", "to": bob[0], "from": carol[0], "body": "from carol"})
    d.step("s21-poll-bob", "POST", "/api/poll?wait=0", auth=pair(bob))

    print("\ncustody (ack)")
    d.step("a1-unknown", "POST", "/api/ack", auth=pair(bob), body={"ids": ["no-such-id"]})
    d.step("a2-string-ids", "POST", "/api/ack", auth=pair(bob), body={"ids": "m1-fixed"})
    d.step("a3-number-ids", "POST", "/api/ack", auth=pair(bob), body={"ids": 5})
    d.step("a4-sender-acks", "POST", "/api/ack", auth=pair(alice), body={"ids": ["m1-fixed"]})
    d.step("a5-ack", "POST", "/api/ack", auth=pair(bob), body={"ids": ["m1-fixed", "m1-fixed", "m6-padded"]})
    d.step("a6-again", "POST", "/api/ack", auth=pair(bob), body={"ids": ["m1-fixed"]})
    d.step("a7-no-auth", "POST", "/api/ack", body={"ids": []})
    d.step("a8-bad-body-no-auth", "POST", "/api/ack", content=b"nope")
    d.step("a9-receipts-to-alice", "POST", "/api/poll?wait=0", auth=pair(alice))

    print("\nreceipts")
    d.step("rc1-delivered", "POST", "/api/receipts", auth=pair(bob),
           body={"receipts": [{"id": "m1-fixed", "state": "delivered", "at": "2026-01-01T00:00:00Z"}]})
    d.step("rc2-again", "POST", "/api/receipts", auth=pair(bob),
           body={"receipts": [{"id": "m1-fixed", "state": "delivered", "at": "2030-01-01T00:00:00Z"}]})
    d.step("rc3-bad-states", "POST", "/api/receipts", auth=pair(bob),
           body={"receipts": [{"id": "m1-fixed", "state": s} for s in ("received", "", "DELIVERED", None)] + [{"id": "m1-fixed"}]})
    d.step("rc4-not-dicts", "POST", "/api/receipts", auth=pair(bob), body={"receipts": [1]})
    d.step("rc5-dict", "POST", "/api/receipts", auth=pair(bob), body={"receipts": {"x": 1}})
    d.step("rc6-read-now", "POST", "/api/receipts", auth=pair(bob), body={"receipts": [{"id": "m6-padded", "state": "read"}]})
    d.step("rc7-wrong-side", "POST", "/api/receipts", auth=pair(alice), body={"receipts": [{"id": "m2-long", "state": "read"}]})
    d.step("rc8-numeric-at", "POST", "/api/receipts", auth=pair(bob), body={"receipts": [{"id": "m2-long", "state": "read", "at": 5}]})
    d.step("rc9-poll-alice", "POST", "/api/poll?wait=0", auth=pair(alice))
    d.step("rc10-poll-alice-again", "POST", "/api/poll?wait=0", auth=pair(alice))

    print("\nattachments")
    d.step("f1-upload", "POST", "/api/attachments?name=notes.txt", auth=pair(alice), content=b"the bytes 0123456789", capture={"att1": ID})
    names = ["../../etc/passwd", "a b.txt", "\u00e9t\u00e9.md", "", "x" * 300, "C:evil.txt", "..\\..\\win.ini", "q\"uote.txt", "%41.txt"]
    for i, nm in enumerate(names):
        d.step(f"f2-name-{i}", "POST", "/api/attachments?" + str(httpx.QueryParams({"name": nm})), auth=pair(alice), content=b"n",
               capture={f"n{i}": ID})
        d.step(f"f2-name-{i}-download", "GET", lambda s, i=i: f"/api/attachments/{s.vars[f'n{i}']}", auth=pair(alice),
               compare_headers=("content-disposition", "content-length"))
    d.step("f2-no-name", "POST", "/api/attachments", auth=pair(alice), content=b"n", capture={"noname": ID})
    dl = lambda s: f"/api/attachments/{s.vars['att1']}"  # noqa: E731
    hdrs = ("content-disposition", "content-length", "accept-ranges", "content-range")
    d.step("f3-owner", "GET", dl, auth=pair(alice), compare_headers=hdrs)
    d.step("f4-unbound-stranger", "GET", dl, auth=pair(bob))
    d.step("f5-no-auth", "GET", dl)
    d.step("f6-unknown", "GET", "/api/attachments/nosuchid", auth=pair(alice))
    d.step("f7-bind", "POST", "/api/send", auth=pair(alice),
           body=lambda s: {"id": "m9-att", "to": bob[0], "body": "file", "attachments": [s.vars["att1"]]})
    d.step("f7-recipient", "GET", dl, auth=pair(bob), compare_headers=hdrs)
    d.step("f8-third", "GET", dl, auth=pair(eve))
    d.step("f9-rebind", "POST", "/api/send", auth=pair(alice), body=lambda s: {"to": bob[0], "body": "again", "attachments": [s.vars["att1"]]})
    d.step("f9b-not-owner", "POST", "/api/send", auth=pair(bob), body=lambda s: {"to": alice[0], "body": "x", "attachments": [s.vars["att1"]]})
    d.step("f9c-same-id-retry", "POST", "/api/send", auth=pair(alice), body=lambda s: {"id": "m9-att", "to": bob[0], "body": "file",
                                                                                     "attachments": [s.vars["att1"]]})
    for i, rng in enumerate(["bytes=0-3", "bytes=-2", "bytes=5-", "bytes=1000-2000", "bytes=3-1", "items=0-1", "bytes=0-0"]):
        d.step(f"f10-range-{i}", "GET", dl, auth=pair(alice), headers={"range": rng}, compare_headers=hdrs)
    d.step("f14-multirange", "GET", dl, auth=pair(alice), headers={"range": "bytes=0-1,3-4"}, compare_headers=hdrs)
    d.step("f15-oversize", "POST", "/api/attachments?name=big.bin", auth=pair(alice), content=b"x" * (LIMIT + 1))
    d.step("f15b-at-limit", "POST", "/api/attachments?name=edge.bin", auth=pair(alice), content=b"x" * LIMIT, capture={"edge": ID})
    d.step("f16-no-auth", "POST", "/api/attachments?name=x", content=b"x")
    for s in d.sides:
        os.remove(os.path.join(s.data, "blobs", s.vars["att1"]))
    d.step("f17-gone", "GET", dl, auth=pair(alice))
    d.step("f18-healthz", "GET", "/healthz")

    print("\nroster, unregister")
    d.step("ro1-no-auth", "GET", "/api/roster")
    d.step("ro2-roster", "GET", "/api/roster", auth=pair(eve))
    d.step("un1-unregister", "POST", "/api/unregister", auth=f"{pair(zed)} {pair(zed)}")
    d.step("un2-no-auth", "POST", "/api/unregister")
    d.step("un3-poll-gone", "POST", "/api/poll?wait=0", auth=pair(zed))
    d.step("un4-send-to-gone", "POST", "/api/send", auth=pair(alice), body={"to": zed[0], "body": "x"})
    d.step("un5-back", "POST", "/api/register", auth=pair(zed), body={"slug": zed[0], "org_name": "Zed again"})

    print("\noperator view")
    d.step("u1-global", "GET", "/ui/messages")
    d.step("u2-org", "GET", f"/ui/messages?org={bob[0]}")
    d.step("u3-client", "GET", "/ui/messages?client=tester&limit=5")
    d.step("u3b-client-chat", "GET", "/ui/messages?client=ncola_k8bx")
    for v in ("1", "0", "-5", "10000", "3"):
        d.step(f"u4-limit-{v}", "GET", f"/ui/messages?limit={v}")
    d.step("u5-bad-params", "GET", "/ui/messages?limit=abc&before_n=x")
    a, _ = d.step("u6-page1", "GET", "/ui/messages?limit=3")
    m = a.json()["messages"][-1]
    d.step("u6-page2", "GET", f"/ui/messages?limit=3&before_at={m['received_at']}&before_n={m['n']}")
    d.step("u7-bad-cursor", "GET", "/ui/messages?before_at=zzz")
    d.step("u8-ui-data", "GET", "/ui/data")

    print("\nthe public listener")
    for i, p in enumerate(["//ui/messages", "/api/../ui/messages", "/API/roster", "/UI/messages", "/healthz/../ui/messages", "/ui//messages",
                           "/./ui/messages", "/healthz/", "/api", "/apiX/roster", "/", "/ui/data", "/%75i/messages"]):
        d.step(f"pub1-trick-{i}", "GET", p, public=True, raw=True)
    d.step("pub2-roster", "GET", "/api/roster", auth=pair(eve), public=True)
    d.step("pub3-healthz", "GET", "/healthz", public=True)
    for i, (mth, p, body) in enumerate([("POST", "/api/register", {"slug": "x.y.z"}), ("POST", "/api/poll", {}), ("POST", "/api/ack", {"ids": []}),
                                        ("POST", "/api/send", {"to": "x", "body": "y"}), ("POST", "/api/receipts", {"ids": []}),
                                        ("GET", "/api/roster", None), ("GET", "/api/attachments/deadbeef", None)]):
        d.step(f"pub4-no-creds-{i}", mth, p, body=body, public=True)
    ws = {"connection": "Upgrade", "upgrade": "websocket", "sec-websocket-version": "13", "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ=="}
    d.step("ws-public", "GET", "/api/poll", auth=pair(alice), headers=ws, public=True, raw=True)
    d.step("ws-full", "GET", "/api/poll", auth=pair(alice), headers=ws, raw=True)

    print("\nlong poll")
    results: dict[str, Any] = {}

    def park(side: Side) -> None:
        t0 = time.time()
        r = side.client.post(side.base() + "/api/poll?wait=20", headers={"x-org-auth": pair(carol)})
        results[side.name] = (r, time.time() - t0)

    d.step("lp0-drain", "POST", "/api/ack", auth=pair(carol), body={"ids": ["m7-nul"]})
    threads = [threading.Thread(target=park, args=(s,)) for s in d.sides]
    for t in threads:
        t.start()
    time.sleep(1.5)
    d.step("lp1-roster-shows-parked", "GET", "/api/roster", auth=pair(eve))
    d.step("lp1-wake", "POST", "/api/send", auth=pair(alice), body={"id": "m10-wake", "to": carol[0], "body": "wake up"})
    for t in threads:
        t.join(30)
    (ra, ta), (rb, tb) = results["python"], results["rust"]
    d.compare("lp1-woken-answer", ra, rb, ())
    print(f"     woken after: python {ta:.2f} s, rust {tb:.2f} s (both parked 1.5 s before the send)")
    if not (ta < 4 and tb < 4):
        d.unexpected.append(f"lp1-woken-answer: a parked poll was not woken promptly (python {ta:.2f} s, rust {tb:.2f} s)")
    t0 = time.time()
    d.step("lp2-timeout", "POST", "/api/poll?wait=1", auth=pair(eve))
    print(f"     two 1 s polls took {time.time() - t0:.2f} s in total")


def compare_logs(d: Diff) -> None:
    py, rs = d.sides
    time.sleep(0.5)

    def key(side: Side, drop_500: bool) -> list[Any]:
        out = []
        for ln in side.lines:
            if "path" in ln:
                if drop_500 and ln.get("status") == 500:
                    continue
                out.append([side.norm(ln["path"]), ln["slugs"], ln["status"], sorted(ln)])
        return out

    a, b = key(py, False), key(rs, True)
    starts = [[sorted(ln) for ln in s.lines if "hub" in ln] for s in (py, rs)]
    if starts[0] == starts[1]:
        d.same += 1
    elif len(starts[0]) == 2 and starts[0][:1] == starts[1]:
        d.expected_seen["log-startup"] = f"python {len(starts[0])} startup lines, rust {len(starts[1])}"
        print(f"  ~  log-startup  (deliberate: {EXPECTED['log-startup']})")
    else:
        d.unexpected.append(f"log-startup: {starts}")
    i = j = agreed = tolerated = 0
    while i < len(a) and j < len(b):
        x, y = a[i], b[j]
        if x == y:
            agreed += 1
        elif x[0] == y[0] and x[1] == y[1] and (x[0], x[2], y[2]) in d.divergent:
            tolerated += 1
        else:
            d.unexpected.append(f"log-lines: first difference at line {i}: python {x} / rust {y}")
            break
        i, j = i + 1, j + 1
    else:
        if len(a) != len(b):
            d.unexpected.append(f"log-lines: python wrote {len(a)}, rust {len(b)}")
        else:
            d.same += 1
            print(f"  =  log-lines: {agreed} request lines agree (path, slugs, status, keys); "
                  f"{tolerated} differ only by a listed deliberate difference")
    extra = len([ln for ln in rs.lines if ln.get("status") == 500])
    if extra:
        d.expected_seen["log-500"] = f"rust wrote {extra} lines for 500 answers"
        print(f"  ~  log-500  (deliberate: {EXPECTED['log-500']}; {extra} lines)")


IMPORT_IDS: dict[str, tuple[str, str]] = {}


def populate(s: Side) -> None:
    """Give a v1 hub a store worth importing: every kind of row and state
    a running hub accumulates (queued, fetched, delivered, read, receipts
    owed and pushed, bound/unbound/missing attachments, an address that left
    with mail still queued, a duplicate, a truncated body, a NUL)."""
    def me(name: str, kind: str = "org", user: str = "tester") -> tuple[str, str]:
        secret = f"import-secret-{name}"
        slug = f"{name}.{user.replace('_', '-')}.{fp6(secret)}"
        IMPORT_IDS[name] = (slug, secret)
        r = s.client.post(s.base() + "/api/register", headers={"x-org-auth": f"{slug}:{secret}"},
                          json={"slug": slug, "org_name": name.title(), "username": user, "blurb": f"{name} blurb", "kind": kind})
        assert r.status_code == 200, r.text
        return slug, secret

    def call(path: str, who: tuple[str, str] | None, **kw: Any) -> httpx.Response:
        h = {"x-org-auth": f"{who[0]}:{who[1]}"} if who else {}
        return s.client.post(s.base() + path, headers=h, **kw)

    people = [me(n) for n in ("ann", "ben", "cat", "dan", "gone")] + [me("chat-one", "chat", "ncola_k8bx"), me("pers", "person")]
    ann, ben, cat, dan, gone, chat, pers = people
    bodies = ["hello", "", "é ✓ 漢字 😀", "x" * 25000, "line\nbreak\ttab", "a\u0000b"]
    n = 0
    for rnd in range(4):
        for i, frm in enumerate(people):
            to = people[(i + 1 + rnd) % len(people)]
            if to == frm:
                continue
            n += 1
            payload: dict[str, Any] = {"to": to[0], "body": bodies[n % len(bodies)], "kind": ["message", "status", None, 5][n % 4],
                                       "thread_id": [None, "th-1"][n % 2], "sent_at": [None, "2020-01-01T00:00:00Z", 12][n % 3]}
            if n % 3 == 0:
                payload["id"] = f"given-{n}"
            assert call("/api/send", frm, json=payload).status_code == 200
    call("/api/send", ann, json={"id": "given-3", "to": ben[0], "body": "duplicate"})
    up = {}
    for name, who, data in (("bound1", ann, b"first file"), ("bound2", ben, b"second"), ("loose", cat, b"never sent"),
                            ("lost", dan, b"blob removed"), ("weird", ann, b"w")):
        nm = {"weird": "my file é.txt"}.get(name, f"{name}.bin")
        r = s.client.post(s.base() + "/api/attachments", params={"name": nm}, headers={"x-org-auth": f"{who[0]}:{who[1]}"}, content=data)
        up[name] = r.json()["id"]
    for name, frm, to in (("bound1", ann, ben), ("bound2", ben, cat), ("lost", dan, ann), ("weird", ann, cat)):
        assert call("/api/send", frm, json={"id": f"att-{name}", "to": to[0], "body": "with a file", "attachments": [up[name]]}).status_code == 200
    os.remove(os.path.join(s.data, "blobs", up["lost"]))
    for name, v in up.items():
        IMPORT_IDS[f"att-{name}"] = (v, "")
    # custody and receipts in every state
    for who in (ben, cat):
        polled = call("/api/poll?wait=0", who, json={}).json()["messages"]
        ids = [m["id"] for m in polled]
        call("/api/ack", who, json={"ids": ids[: len(ids) // 2]})
        call("/api/receipts", who, json={"receipts": [{"id": i, "state": "delivered", "at": "2026-05-05T05:05:05Z"} for i in ids[:2]]
                                         + [{"id": ids[0], "state": "read", "at": 1234}]})
    call("/api/poll?wait=0", ann, json={})          # some receipts pushed, later ones owed
    call("/api/receipts", ben, json={"receipts": [{"id": "given-3", "state": "read"}]})
    call("/api/unregister", gone)                   # left with mail still queued


def import_scenarios(d: Diff) -> None:
    ids = IMPORT_IDS

    def pair(name: str) -> str:
        return f"{ids[name][0]}:{ids[name][1]}"

    names = ("ann", "ben", "cat", "dan", "chat-one", "pers")
    print("\nthe imported records, read back")
    d.step("im1-healthz", "GET", "/healthz")
    d.step("im2-ui-data", "GET", "/ui/data")
    d.step("im3-all-messages", "GET", "/ui/messages?limit=500", exact=True)
    a, _ = d.step("im4-page", "GET", "/ui/messages?limit=7", exact=True)
    m = a.json()["messages"][-1]
    d.step("im4-page2", "GET", f"/ui/messages?limit=7&before_at={m['received_at']}&before_n={m['n']}", exact=True)
    d.step("im5-org", "GET", f"/ui/messages?org={ids['ben'][0]}", exact=True)
    d.step("im5-client-chat", "GET", "/ui/messages?client=ncola_k8bx", exact=True)
    d.step("im6-roster", "GET", "/api/roster", auth=pair("ann"), exact=True)
    for nm in names:
        d.step(f"im7-poll-{nm}", "POST", "/api/poll?wait=0", auth=pair(nm), exact=True)
    print("\nattachments after the import")
    hdrs = ("content-disposition", "content-length", "content-type")
    for att, owner, rcpt in (("bound1", "ann", "ben"), ("bound2", "ben", "cat"), ("loose", "cat", "ann"), ("lost", "dan", "ann"),
                             ("weird", "ann", "cat")):
        path = f"/api/attachments/{ids['att-' + att][0]}"
        d.step(f"im8-{att}-owner", "GET", path, auth=pair(owner), compare_headers=hdrs)
        d.step(f"im8-{att}-recipient", "GET", path, auth=pair(rcpt), compare_headers=hdrs)
    print("\nlife goes on after the import")
    d.step("im9-reregister", "POST", "/api/register", auth=pair("ann"), body={"slug": ids["ann"][0], "org_name": "Ann"})
    d.step("im9-stolen", "POST", "/api/register", auth=f"{ids['ann'][0]}:wrong", body={"slug": ids["ann"][0]})
    d.step("im9-gone-is-gone", "POST", "/api/send", auth=pair("ann"), body={"to": ids["gone"][0], "body": "x"})
    d.step("im10-new-send", "POST", "/api/send", auth=pair("ann"), body={"id": "after-import", "to": ids["ben"][0], "body": "new"})
    d.step("im10-new-numbered", "GET", "/ui/messages?limit=2", exact=False)
    d.step("im11-ack", "POST", "/api/ack", auth=pair("ben"), body={"ids": ["after-import", "given-3"]})
    d.step("im11-receipts", "POST", "/api/receipts", auth=pair("ben"), body={"receipts": [{"id": "after-import", "state": "read"}]})
    d.step("im11-sender-hears", "POST", "/api/poll?wait=0", auth=pair("ann"))
    report = os.path.join(d.sides[1].data, "v2-import-report.json")
    try:
        with open(report, encoding="utf-8") as f:
            rep = json.load(f)
        print(f"     import report: {rep['orgs']} addresses, {rep['messages']} messages, {rep['attachments']} attachments, "
              f"anomalies {rep['anomalies']}")
        if set(rep["anomalies"]) != {"NUL removed from body"}:
            d.unexpected.append(f"import report anomalies: {rep['anomalies']} (expected only the NUL the store was given)")
    except (OSError, ValueError, KeyError) as e:
        d.unexpected.append(f"import report unreadable: {e}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust-bin", required=True)
    ap.add_argument("--database-url", required=True)
    ap.add_argument("--mode", choices=("protocol", "import"), default="protocol")
    ap.add_argument("-v", action="store_true")
    args = ap.parse_args()
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    if args.mode == "import":
        tmp = tempfile.mkdtemp(prefix="hub-import-")
        pd, rd = os.path.join(tmp, "py"), os.path.join(tmp, "rs")
        os.makedirs(pd)
        first = start_python(pd)
        populate(first)
        stop(first)
        shutil.copytree(pd, rd)                     # the v1 data folder, as an upgrade finds it
        rs = start_rust(args.rust_bin, rd, args.database_url)   # imports at startup
        py = start_python(pd)
    else:
        py, rs, tmp = start_hubs(args.rust_bin, args.database_url)
    d = Diff(py, rs, args.v)
    try:
        if args.mode == "import":
            import_scenarios(d)
        else:
            scenarios(d)
            compare_logs(d)
    finally:
        for s in d.sides:
            s.proc.terminate()
            try:
                s.proc.wait(10)
            except subprocess.TimeoutExpired:
                s.proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"\nagree: {d.same} · deliberate differences seen: {len(d.expected_seen)} · UNEXPECTED: {len(d.unexpected)}"
          f" · v2 additions set aside in {d.additions} answers (/healthz version, features)")
    for u in d.unexpected:
        print(f"\n✗ {u}")
    return 1 if d.unexpected else 0


if __name__ == "__main__":
    sys.exit(main())
