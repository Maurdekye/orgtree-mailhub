"""Docker lifecycle verification — the standalone hub v2, end to end, isolated.

Builds the image, runs a throwaway PostgreSQL container and the hub on their
own network, ports and named volumes, and drives the real wire protocol
against it: register (owned addresses), send (idempotent), the multiplexed
long poll, custody ack, receipts, attachments, presence, the read-only UI,
the FR-10 public listener split, the container healthcheck, and
persistence across a hub restart and a database restart.

With --upgrade it also proves the in-place upgrade from v1: the v1 image
(built from the last v1 commit with `git archive`) fills a data volume, then
the v2 image starts on that same volume and imports it.

Everything it creates is namespaced `mailhub-verify` and removed at the end
(pass --keep to inspect). Ports (loopback only): 7391 = full app,
7392 = public API-only listener. Nothing here touches any other hub,
container, network or volume.

    python tools/verify-docker.py [--upgrade] [--keep]
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

IMAGE = "orgtree-mailhub-verify"
IMAGE_V1 = "orgtree-mailhub-verify-v1"
V1_COMMIT = "79a7c51"           # the last v1 (Python) hub, with the 1 GiB limit
NAME = "mailhub-verify"
DB = "mailhub-verify-db"
NET = "mailhub-verify-net"
VOL = "mailhub-verify-data"
DBVOL = "mailhub-verify-pg"
PASSWORD = "verify-" + hashlib.sha256(os.urandom(16)).hexdigest()[:24]
FULL = "http://127.0.0.1:7391"
PUB = "http://127.0.0.1:7392"
_REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

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


def run(*args: str, timeout: float = 3600.0) -> str:
    r = subprocess.run(list(args), capture_output=True, text=True, timeout=timeout, cwd=_REPO)
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}\n{r.stdout}\n{r.stderr}")
    return r.stdout.strip()


def quiet(*args: str) -> None:
    subprocess.run(list(args), capture_output=True)


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
    r = urllib.request.Request(base + path, data=data, headers=headers,
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


def wait_health(base: str, deadline_s: float = 90.0) -> dict:
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


def wait_db() -> None:
    end = time.monotonic() + 90
    while time.monotonic() < end:
        r = subprocess.run(["docker", "exec", DB, "pg_isready", "-U", "mailhub", "-d", "mailhub"], capture_output=True)
        if r.returncode == 0:
            return
        time.sleep(0.5)
    raise RuntimeError("the database container never became ready")


def container_health(name: str, deadline_s: float = 120.0) -> str:
    end = time.monotonic() + deadline_s
    status = ""
    while time.monotonic() < end:
        status = run("docker", "inspect", "-f", "{{.State.Health.Status}}", name)
        if status in ("healthy", "unhealthy"):
            return status
        time.sleep(1.0)
    return status


def slug_for(name: str, secret: str) -> str:
    return f"{name}.verify.{hashlib.sha256(secret.encode()).hexdigest()[:6]}"


def start_db() -> None:
    run("docker", "run", "-d", "--name", DB, "--network", NET, "-e", "POSTGRES_USER=mailhub",
        "-e", f"POSTGRES_PASSWORD={PASSWORD}", "-e", "POSTGRES_DB=mailhub",
        "-v", f"{DBVOL}:/var/lib/postgresql", "postgres:18")
    wait_db()


def start_hub(image: str, extra_env: list[str] | None = None) -> None:
    env = ["-e", "HUB_NAME=verify-hub", "-e", "HUB_PUBLIC=1", "-e", "HUB_RETENTION_DAYS=30"]
    if image == IMAGE:
        env += ["-e", "HUB_DATABASE_URL=postgres://mailhub@" + DB + ":5432/mailhub", "-e", f"HUB_DATABASE_PASSWORD={PASSWORD}"]
    run("docker", "run", "-d", "--name", NAME, "--network", NET,
        "-p", "127.0.0.1:7391:7370", "-p", "127.0.0.1:7392:7371",
        "--health-cmd", "orgtree-mailhub healthcheck" if image == IMAGE else "true", "--health-interval", "2s",
        *env, *(extra_env or []), "-v", f"{VOL}:/data", image)


def cleanup() -> None:
    for c in (NAME, DB):
        quiet("docker", "rm", "-f", c)
    for v in (VOL, DBVOL):
        quiet("docker", "volume", "rm", v)
    quiet("docker", "network", "rm", NET)


def protocol(sa: str, sb: str) -> tuple[str, str, str]:
    alice, bob = slug_for("alice", sa), slug_for("bob", sb)
    code, out = req(FULL, "/api/register", {"slug": alice, "org_name": "Alice"}, auth=f"{alice}:{sa}")
    check("register: owned address minted, roster returned",
          code == 200 and isinstance(out, dict) and any(r["slug"] == alice for r in out.get("roster", [])))
    req(FULL, "/api/register", {"slug": bob, "org_name": "Bob"}, auth=f"{bob}:{sb}")
    code, _ = req(FULL, "/api/register", {"slug": alice, "org_name": "Mallory"}, auth=f"{alice}:{'c' * 32}")
    check("auth: a different secret cannot claim an owned address (403)", code == 403)
    code, _ = req(FULL, "/api/poll?wait=0", {}, auth=f"{alice}:wrong")
    check("auth: a wrong secret cannot poll (401)", code == 401)
    code, up = req(FULL, "/api/attachments?name=note.txt", b"hello from the verification", auth=f"{alice}:{sa}")
    aid = up.get("id") if isinstance(up, dict) else None
    check("attachment upload accepted", code == 200 and bool(aid))
    code, sent = req(FULL, "/api/send", {"id": "verify-m1", "to": bob, "body": "hi bob", "attachments": [aid]}, auth=f"{alice}:{sa}")
    check("send: hub custody confirmed (the 200 IS the receipt)",
          code == 200 and isinstance(sent, dict) and sent.get("duplicate") is False)
    code, dup = req(FULL, "/api/send", {"id": "verify-m1", "to": bob, "body": "hi bob"}, auth=f"{alice}:{sa}")
    check("send: retry on the same id is a duplicate, not a second mail",
          code == 200 and isinstance(dup, dict) and dup.get("duplicate") is True)
    code, _ = req(FULL, "/api/send", {"to": "nobody.here.000000", "body": "x"}, auth=f"{alice}:{sa}")
    check("send: unknown recipient refused (422)", code == 422)
    code, polled = req(FULL, "/api/poll", None, auth=f"{bob}:{sb}", method="POST")
    msgs = polled.get("messages", []) if isinstance(polled, dict) else []
    check("poll (no body, as Orgtree sends it): queued mail arrives with the attachment metadata",
          code == 200 and len(msgs) == 1 and msgs[0]["id"] == "verify-m1" and msgs[0]["attachments"][0]["id"] == aid,
          str(polled)[:200])
    roster = polled.get("roster", []) if isinstance(polled, dict) else []
    check("presence: the sender shows online while recently authed", any(r["slug"] == alice and r["online"] for r in roster))
    code, body = req(FULL, f"/api/attachments/{aid}", auth=f"{bob}:{sb}")
    check("attachment download by the recipient", code == 200 and body == b"hello from the verification")
    code, _ = req(FULL, f"/api/attachments/{aid}", auth=f"{slug_for('eve', 'e' * 32)}:{'e' * 32}")
    check("attachment refused to a third party", code in (401, 403))
    code, acked = req(FULL, "/api/ack", {"ids": ["verify-m1"]}, auth=f"{bob}:{sb}")
    check("ack: custody transfers exactly once", code == 200 and isinstance(acked, dict) and acked.get("acked") == 1)
    req(FULL, "/api/receipts", {"receipts": [{"id": "verify-m1", "state": "read"}]}, auth=f"{bob}:{sb}")
    code, back = req(FULL, "/api/poll?wait=0", {}, auth=f"{alice}:{sa}")
    recs = back.get("receipts", []) if isinstance(back, dict) else []
    check("receipts: the sender is told the mail was read",
          any(r["id"] == "verify-m1" and r["state"] == "read" for r in recs), str(recs))
    code, ui = req(FULL, "/ui/data")
    check("read-only UI: /ui/data serves the operator view",
          code == 200 and isinstance(ui, dict) and any(o["slug"] == alice for o in ui.get("orgs", [])))
    code, page = req(FULL, "/")
    check("read-only UI: / serves the page", code == 200 and b"orgtree mail hub" in page)
    code, _ = req(PUB, "/healthz")
    check("public listener: /healthz reachable", code == 200)
    code, _ = req(PUB, "/")
    check("public listener: the all-mail UI is NOT served (404)", code == 404)
    code, _ = req(PUB, "/ui/data")
    check("public listener: /ui/* is NOT served (404)", code == 404)
    code, pr = req(PUB, "/api/roster", auth=f"{alice}:{sa}")
    check("public listener: authed /api/* passes through",
          code == 200 and isinstance(pr, dict) and any(r["slug"] == bob for r in pr.get("roster", [])))
    return alice, bob, str(aid)


def persistence(alice: str, sa: str, bob: str, aid: str, orgs: int) -> None:
    h2 = wait_health(FULL)
    check("persistence: orgs survive", h2.get("orgs") == orgs, str(h2))
    code, sent2 = req(FULL, "/api/send", {"id": "verify-m1", "to": bob, "body": "hi bob"}, auth=f"{alice}:{sa}")
    check("persistence: message store survives (same id still a duplicate)",
          code == 200 and isinstance(sent2, dict) and sent2.get("duplicate") is True)
    code, body2 = req(FULL, f"/api/attachments/{aid}", auth=f"{alice}:{sa}")
    check("persistence: attachment blob survives", code == 200 and body2 == b"hello from the verification")


def main() -> int:
    keep = "--keep" in sys.argv
    upgrade = "--upgrade" in sys.argv
    print("docker lifecycle verification (v2) — isolated containers")
    run("docker", "build", "-q", "-t", IMAGE, ".")
    cleanup()
    run("docker", "network", "create", NET)
    try:
        start_db()
        start_hub(IMAGE)
        h = wait_health(FULL)
        check("clean startup: /healthz ok with the configured name and the 1 GiB limit",
              h.get("name") == "verify-hub" and h.get("retention_days") == 30
              and h.get("max_attachment_bytes") == 1024 ** 3, str(h))
        # v2.0.1: the hub names its door as the container sees it (Docker
        # publishes it here as 7392); the door's own /healthz never does
        check("door: /healthz on the main port names it as the container sees it (7371, every address)",
              h.get("door") == {"port": 7371, "bind": "0.0.0.0"}, str(h))
        code, d = req(PUB, "/healthz")
        check("door: its own /healthz does not name it", code == 200 and isinstance(d, dict) and d.get("ok") is True and "door" not in d, str(d))
        sa, sb = "a" * 32, "b" * 32
        alice, bob, aid = protocol(sa, sb)
        check("container healthcheck: `orgtree-mailhub healthcheck` reports healthy", container_health(NAME) == "healthy")
        run("docker", "restart", NAME)
        persistence(alice, sa, bob, aid, 2)
        run("docker", "restart", DB)
        wait_db()
        time.sleep(2)
        code, _ = req(FULL, "/api/roster", auth=f"{alice}:{sa}")
        check("a database restart under a running hub: the pool reconnects", code == 200, str(code))
        logs = run("docker", "logs", NAME)
        check("logging: structured JSON request lines on stdout",
              any(line.startswith("{") and '"path"' in line for line in logs.splitlines()))
        check("logging: the startup line on stdout", any('"hub": "verify-hub"' in line for line in logs.splitlines()))
        if upgrade:
            print("\nin-place upgrade from v1")
            quiet("docker", "rm", "-f", NAME)
            quiet("docker", "rm", "-f", DB)
            for v in (VOL, DBVOL):
                quiet("docker", "volume", "rm", v)
            with tempfile.TemporaryDirectory(prefix="mailhub-v1-") as src:
                archive = os.path.join(src, "v1.tar")
                run("git", "archive", "-o", archive, V1_COMMIT)
                run("tar", "-xf", archive, "-C", src)
                os.remove(archive)
                r = subprocess.run(["docker", "build", "-q", "-t", IMAGE_V1, src], capture_output=True, text=True)
                if r.returncode != 0:
                    raise RuntimeError(r.stderr)
            start_hub(IMAGE_V1)
            wait_health(FULL)
            alice, bob, aid = protocol(sa, sb)
            quiet("docker", "rm", "-f", NAME)
            start_db()
            start_hub(IMAGE)
            h = wait_health(FULL)
            check("upgrade: v2 started on the v1 volume and imported its store", h.get("orgs") == 2 and h.get("queued") == 0, str(h))
            persistence(alice, sa, bob, aid, 2)
            code, ui = req(FULL, "/ui/messages")
            m = [x for x in (ui.get("messages", []) if isinstance(ui, dict) else []) if x["id"] == "verify-m1"]
            check("upgrade: v1's receipts carried over (read, fetched)",
                  len(m) == 1 and m[0]["state"] == "fetched" and bool(m[0]["read_at"]), str(m))
            report = run("docker", "exec", NAME, "cat", "/data/v2-import-report.json")
            rep = json.loads(report)
            check("upgrade: the import report counts what it moved", rep.get("orgs") == 2 and rep.get("messages") == 1
                  and rep.get("attachments") == 1 and rep.get("anomalies") == {}, report[:300])
            files = run("docker", "exec", NAME, "ls", "/data")
            check("upgrade: the v1 store is kept beside the import (rollback)", "hub.sqlite3" in files.split(), files)
    finally:
        if keep:
            print(f"(kept: containers {NAME}, {DB}; volumes {VOL}, {DBVOL}; network {NET}; images {IMAGE}, {IMAGE_V1})")
        else:
            cleanup()
            quiet("docker", "rmi", IMAGE)
            quiet("docker", "rmi", IMAGE_V1)
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
