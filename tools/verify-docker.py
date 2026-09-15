"""Docker lifecycle verification — the standalone hub, end to end, isolated.

Builds the image, runs a throwaway container on its own ports and its own
named volume, and drives the real wire protocol against it: register (owned
addresses), send (idempotent), the multiplexed long poll, custody ack,
receipts, attachments, presence, the read-only UI, the FR-10 public listener
split, and persistence across a container restart. Everything it creates is
namespaced `mailhub-verify` and removed at the end (pass --keep to inspect).

Ports (loopback only): 7391 = full app, 7392 = public API-only listener.
Nothing here touches any other hub, container, or volume.

    python tools/verify-docker.py [--keep]
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

IMAGE = "orgtree-mailhub-verify"
NAME = "mailhub-verify"
VOL = "mailhub-verify-data"
FULL = "http://127.0.0.1:7391"
PUB = "http://127.0.0.1:7392"

PASS = 0
FAILED: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    global PASS
    if ok:
        PASS += 1
        print(f"  ok {PASS:3d}  {label}")
    else:
        FAILED.append(label)
        print(f"  FAIL     {label}  {detail}")


def run(*args: str, timeout: float = 300.0) -> str:
    r = subprocess.run(list(args), capture_output=True, text=True,
                       timeout=timeout)
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}\n{r.stdout}\n{r.stderr}")
    return r.stdout.strip()


def req(base: str, path: str, payload: dict | bytes | None = None,
        auth: str = "", method: str | None = None) -> tuple[int, dict | bytes]:
    headers: dict[str, str] = {}
    data = None
    if isinstance(payload, dict):
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    elif isinstance(payload, bytes):
        data = payload
    if auth:
        headers["X-Org-Auth"] = auth
    r = urllib.request.Request(
        base + path, data=data, headers=headers,
        method=method or ("POST" if data is not None else "GET"))
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            body = resp.read()
            if (resp.headers.get_content_type() or "") == "application/json":
                return resp.status, json.loads(body.decode() or "{}")
            return resp.status, body
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode() or "{}")
        except ValueError:
            return e.code, {}


def wait_health(base: str, deadline_s: float = 60.0) -> dict:
    end = time.monotonic() + deadline_s
    while time.monotonic() < end:
        try:
            code, body = req(base, "/healthz")
            if code == 200 and isinstance(body, dict) and body.get("ok"):
                return body
        except OSError:
            pass
        time.sleep(0.5)
    raise RuntimeError(f"hub at {base} never became healthy")


def slug_for(name: str, secret: str) -> str:
    return f"{name}.verify.{hashlib.sha256(secret.encode()).hexdigest()[:6]}"


def main() -> int:
    keep = "--keep" in sys.argv
    print("docker lifecycle verification — isolated container")
    run("docker", "build", "-q", "-t", IMAGE, ".")
    subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
    subprocess.run(["docker", "volume", "rm", VOL], capture_output=True)
    run("docker", "run", "-d", "--name", NAME,
        "-p", "127.0.0.1:7391:7370", "-p", "127.0.0.1:7392:7371",
        "-e", "HUB_NAME=verify-hub", "-e", "HUB_PUBLIC=1",
        "-e", "HUB_RETENTION_DAYS=30",
        "-v", f"{VOL}:/data", IMAGE)
    try:
        h = wait_health(FULL)
        check("clean startup: /healthz ok with the configured name",
              h.get("name") == "verify-hub" and h.get("retention_days") == 30,
              str(h))

        sa, sb = "a" * 32, "b" * 32
        alice, bob = slug_for("alice", sa), slug_for("bob", sb)
        code, out = req(FULL, "/api/register",
                        {"slug": alice, "org_name": "Alice"},
                        auth=f"{alice}:{sa}")
        check("register: owned address minted, roster returned",
              code == 200 and isinstance(out, dict)
              and any(r["slug"] == alice for r in out.get("roster", [])))
        req(FULL, "/api/register", {"slug": bob, "org_name": "Bob"},
            auth=f"{bob}:{sb}")

        code, _ = req(FULL, "/api/register",
                      {"slug": alice, "org_name": "Mallory"},
                      auth=f"{alice}:{'c' * 32}")
        check("auth: a different secret cannot claim an owned address (403)",
              code == 403)
        code, _ = req(FULL, "/api/poll?wait=0", {}, auth=f"{alice}:wrong")
        check("auth: a wrong secret cannot poll (401)", code == 401)

        code, up = req(FULL, "/api/attachments?name=note.txt",
                       b"hello from the verification", auth=f"{alice}:{sa}")
        aid = up.get("id") if isinstance(up, dict) else None
        check("attachment upload accepted", code == 200 and bool(aid))

        code, sent = req(FULL, "/api/send",
                         {"id": "verify-m1", "to": bob, "body": "hi bob",
                          "attachments": [aid]}, auth=f"{alice}:{sa}")
        check("send: hub custody confirmed (the 200 IS the receipt)",
              code == 200 and isinstance(sent, dict)
              and sent.get("duplicate") is False)
        code, dup = req(FULL, "/api/send",
                        {"id": "verify-m1", "to": bob, "body": "hi bob"},
                        auth=f"{alice}:{sa}")
        check("send: retry on the same id is a duplicate, not a second mail",
              code == 200 and isinstance(dup, dict)
              and dup.get("duplicate") is True)
        code, _ = req(FULL, "/api/send", {"to": "nobody.here.000000",
                                          "body": "x"}, auth=f"{alice}:{sa}")
        check("send: unknown recipient refused (422)", code == 422)

        code, polled = req(FULL, "/api/poll?wait=0", {}, auth=f"{bob}:{sb}")
        msgs = polled.get("messages", []) if isinstance(polled, dict) else []
        check("poll: queued mail arrives with the attachment metadata",
              code == 200 and len(msgs) == 1 and msgs[0]["id"] == "verify-m1"
              and msgs[0]["attachments"][0]["id"] == aid, str(polled)[:200])
        roster = polled.get("roster", []) if isinstance(polled, dict) else []
        check("presence: the sender shows online while recently authed",
              any(r["slug"] == alice and r["online"] for r in roster))

        code, body = req(FULL, f"/api/attachments/{aid}", auth=f"{bob}:{sb}")
        check("attachment download by the recipient",
              code == 200 and body == b"hello from the verification")
        code, _ = req(FULL, f"/api/attachments/{aid}",
                      auth=f"{slug_for('eve', 'e' * 32)}:{'e' * 32}")
        check("attachment refused to a third party", code in (401, 403))

        code, acked = req(FULL, "/api/ack", {"ids": ["verify-m1"]},
                          auth=f"{bob}:{sb}")
        check("ack: custody transfers exactly once",
              code == 200 and isinstance(acked, dict)
              and acked.get("acked") == 1)
        req(FULL, "/api/receipts",
            {"receipts": [{"id": "verify-m1", "state": "read"}]},
            auth=f"{bob}:{sb}")
        code, back = req(FULL, "/api/poll?wait=0", {}, auth=f"{alice}:{sa}")
        recs = back.get("receipts", []) if isinstance(back, dict) else []
        check("receipts: the sender is told the mail was read",
              any(r["id"] == "verify-m1" and r["state"] == "read"
                  for r in recs), str(recs))

        code, ui = req(FULL, "/ui/data")
        check("read-only UI: /ui/data serves the operator view",
              code == 200 and isinstance(ui, dict)
              and any(o["slug"] == alice for o in ui.get("orgs", [])))
        code, page = req(FULL, "/")
        check("read-only UI: / serves the page",
              code == 200 and b"orgtree mail hub" in page)

        code, ph = req(PUB, "/healthz")
        check("public listener: /healthz reachable", code == 200)
        code, _ = req(PUB, "/")
        check("public listener: the all-mail UI is NOT served (404)",
              code == 404)
        code, _ = req(PUB, "/ui/data")
        check("public listener: /ui/* is NOT served (404)", code == 404)
        code, pr = req(PUB, "/api/roster", auth=f"{alice}:{sa}")
        check("public listener: authed /api/* passes through",
              code == 200 and isinstance(pr, dict)
              and any(r["slug"] == bob for r in pr.get("roster", [])))

        run("docker", "restart", NAME)
        h2 = wait_health(FULL)
        check("persistence: orgs survive a container restart",
              h2.get("orgs") == 2, str(h2))
        code, sent2 = req(FULL, "/api/send",
                          {"id": "verify-m1", "to": bob, "body": "hi bob"},
                          auth=f"{alice}:{sa}")
        check("persistence: message store survives (same id still duplicate)",
              code == 200 and isinstance(sent2, dict)
              and sent2.get("duplicate") is True)
        code, body2 = req(FULL, f"/api/attachments/{aid}",
                          auth=f"{alice}:{sa}")
        check("persistence: attachment blob survives the restart",
              code == 200 and body2 == b"hello from the verification")

        logs = run("docker", "logs", NAME)
        check("logging: structured JSON request lines on stdout",
              any(line.startswith("{") and '"path"' in line
                  for line in logs.splitlines()))
    finally:
        if keep:
            print(f"(kept: container {NAME}, volume {VOL}, image {IMAGE})")
        else:
            subprocess.run(["docker", "rm", "-f", NAME],
                           capture_output=True)
            subprocess.run(["docker", "volume", "rm", VOL],
                           capture_output=True)
            subprocess.run(["docker", "rmi", IMAGE], capture_output=True)

    print()
    if FAILED:
        print(f"docker verification: {PASS} passed · {len(FAILED)} FAILED")
        for f in FAILED:
            print(f"  ✗ {f}")
        return 1
    print(f"docker verification: all {PASS} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
