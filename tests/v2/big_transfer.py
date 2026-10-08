"""One whole 1 GiB transfer through a running v2 hub, over real sockets,
measured: the upload streamed in one request and again resumably in pieces
(with a connection cut mid-piece), the send that binds it, the recipient's
download (and a resumed half), the 1 GiB + 1 refusal, and the hub process's
peak memory throughout.

    python tests/v2/big_transfer.py --rust-bin <orgtree-mailhub> --database-url <url> --data <folder>

The data folder must have room for about 2 GiB; it is emptied afterwards.
Run through `cargo test --test big_transfer -- --ignored --nocapture`.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
from typing import Iterator

import httpx

GIB = 1024 * 1024 * 1024
BLOCK = os.urandom(1024 * 1024)  # 1 GiB is this block, 1024 times, each salted with its index


def chunk(i: int) -> bytes:
    return i.to_bytes(8, "little") + BLOCK[8:]


def stream(start: int = 0, end: int = GIB) -> Iterator[bytes]:
    """The 1 GiB file's bytes [start, end), 1 MiB at a time (never whole)."""
    first, last = start // len(BLOCK), (end - 1) // len(BLOCK)
    for i in range(first, last + 1):
        c = chunk(i)
        lo = max(start - i * len(BLOCK), 0)
        hi = min(end - i * len(BLOCK), len(BLOCK))
        yield c[lo:hi]


def file_sha256() -> str:
    h = hashlib.sha256()
    for c in stream():
        h.update(c)
    return h.hexdigest()


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Peak:
    """The hub process's peak resident memory, read from the OS."""

    def __init__(self, pid: int) -> None:
        self.pid = pid

    def bytes(self) -> int:
        if sys.platform == "win32":
            class PMC(ctypes.Structure):
                _fields_ = [("cb", ctypes.c_ulong), ("PageFaultCount", ctypes.c_ulong), ("PeakWorkingSetSize", ctypes.c_size_t),
                            ("WorkingSetSize", ctypes.c_size_t), ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPagedPoolUsage", ctypes.c_size_t), ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaNonPagedPoolUsage", ctypes.c_size_t), ("PagefileUsage", ctypes.c_size_t),
                            ("PeakPagefileUsage", ctypes.c_size_t)]
            k32 = ctypes.WinDLL("kernel32", use_last_error=True)
            psapi = ctypes.WinDLL("psapi", use_last_error=True)
            k32.OpenProcess.restype = ctypes.c_void_p
            h = k32.OpenProcess(0x0400 | 0x0010, False, self.pid)
            pmc = PMC()
            pmc.cb = ctypes.sizeof(PMC)
            ok = psapi.GetProcessMemoryInfo(ctypes.c_void_p(h), ctypes.byref(pmc), pmc.cb)
            k32.CloseHandle(ctypes.c_void_p(h))
            return int(pmc.PeakWorkingSetSize) if ok else -1
        with open(f"/proc/{self.pid}/status") as f:
            for line in f:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1]) * 1024
        return -1


def mib(n: float) -> str:
    return f"{n / 1024 / 1024:.1f} MiB"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust-bin", required=True)
    ap.add_argument("--database-url", required=True)
    ap.add_argument("--data", required=True)
    a = ap.parse_args()
    os.makedirs(a.data, exist_ok=True)
    port = free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith("HUB_")}
    env.update(HUB_DATABASE_URL=a.database_url, HUB_DATA=a.data, HUB_PORT=str(port), HUB_BIND="127.0.0.1",
               HUB_NAME="big-transfer", HUB_RETENTION_DAYS="")
    hub = subprocess.Popen([a.rust_bin], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    failures: list[str] = []
    try:
        for _ in range(200):
            try:
                if httpx.get(base + "/healthz", timeout=1).status_code == 200:
                    break
            except httpx.HTTPError:
                time.sleep(0.1)
        health = httpx.get(base + "/healthz").json()
        limit = health["max_attachment_bytes"]
        assert limit == GIB and health["max_message_bytes"] == GIB, health
        peak = Peak(hub.pid)
        start_peak = peak.bytes()
        print(f"hub up (pid {hub.pid}): limit {limit} bytes, peak memory so far {mib(start_peak)}")
        want = file_sha256()
        client = httpx.Client(timeout=httpx.Timeout(600.0))

        def me(name: str) -> tuple[str, dict[str, str]]:
            secret = os.urandom(16).hex()
            slug = f"{name}.big.{hashlib.sha256(secret.encode()).hexdigest()[:6]}"
            r = client.post(base + "/api/register", headers={"x-org-auth": f"{slug}:{secret}"},
                            json={"slug": slug, "org_name": name, "username": "big", "kind": "person"})
            assert r.status_code == 200, r.text
            return slug, {"x-org-auth": f"{slug}:{secret}"}

        (ann, ann_h), (bob, bob_h) = me("ann"), me("bob")

        # 1. the whole file in one streamed request
        t0 = time.monotonic()
        r = client.post(base + "/api/attachments?name=one-gib.bin", headers=ann_h, content=stream())
        up1 = time.monotonic() - t0
        assert r.status_code == 200, r.text
        whole = r.json()["id"]
        assert r.json()["bytes"] == GIB, r.text
        print(f"upload, one request: {GIB} bytes in {up1:.1f} s ({mib(GIB / up1)}/s)")

        # 2. resumably, in 64 MiB pieces, one piece cut off half way
        r = client.post(base + "/api/uploads", headers=ann_h, json={"bytes": GIB, "name": "one-gib-resumed.bin", "sha256": want})
        assert r.status_code == 200, r.text
        uid = r.json()["id"]
        piece = 64 * 1024 * 1024
        t0 = time.monotonic()
        offset = 0
        cut_done = False
        while offset < GIB:
            end = min(offset + piece, GIB)
            if not cut_done and offset >= GIB // 2:
                # a client that dies 40 MiB into a piece: the raw socket is closed
                cut_done = True
                s = socket.create_connection(("127.0.0.1", port))
                head = (f"PATCH /api/uploads/{uid}?offset={offset} HTTP/1.1\r\nhost: hub\r\n"
                        f"x-org-auth: {ann_h['x-org-auth']}\r\ncontent-length: {end - offset}\r\n\r\n").encode()
                s.sendall(head)
                sent = 0
                for c in stream(offset, offset + 40 * 1024 * 1024):
                    s.sendall(c)
                    sent += len(c)
                s.close()
                time.sleep(1.0)
                at = client.get(base + f"/api/uploads/{uid}", headers=ann_h).json()["offset"]
                print(f"  a piece cut off after {mib(sent)}: the upload resumes from {at} ({mib(at - offset)} of the cut piece kept)")
                if not (offset <= at <= offset + sent):
                    failures.append(f"resume offset {at} outside [{offset}, {offset + sent}]")
                offset = at
                continue
            r = client.patch(base + f"/api/uploads/{uid}?offset={offset}", headers=ann_h, content=stream(offset, end))
            assert r.status_code == 200, r.text
            offset = r.json()["offset"]
        up2 = time.monotonic() - t0
        done = client.get(base + f"/api/uploads/{uid}", headers=ann_h).json()
        assert done["complete"] is True and done["bytes"] == GIB, done
        print(f"upload, resumable in 64 MiB pieces with a cut: {GIB} bytes in {up2:.1f} s, checked against its sha256 by the hub")

        # 3. the send that binds it: the limit holds one message, body and file together
        r = client.post(base + "/api/send", headers=ann_h, json={"to": bob, "body": "x", "attachments": [whole]})
        if r.status_code != 413 or r.json().get("max_message_bytes") != GIB:
            failures.append(f"a 1 GiB file plus a 1-byte body was not refused 413: {r.status_code} {r.text[:200]}")
        r = client.post(base + "/api/send", headers=ann_h, json={"to": bob, "body": "", "attachments": [whole]})
        assert r.status_code == 200, r.text
        r = client.post(base + "/api/send", headers=ann_h, json={"to": bob, "body": "", "attachments": [uid]})
        assert r.status_code == 200, r.text

        # 4. the recipient downloads both, and resumes one half way
        for aid in (whole, uid):
            h = hashlib.sha256()
            t0 = time.monotonic()
            n = 0
            with client.stream("GET", base + f"/api/attachments/{aid}", headers=bob_h) as resp:
                assert resp.status_code == 200, resp.status_code
                for c in resp.iter_bytes(1024 * 1024):
                    h.update(c)
                    n += len(c)
            dt = time.monotonic() - t0
            ok = h.hexdigest() == want and n == GIB
            print(f"download {aid[:8]}: {n} bytes in {dt:.1f} s ({mib(n / dt)}/s), sha256 {'matches' if ok else 'DIFFERS'}")
            if not ok:
                failures.append(f"download of {aid} differs")
        h = hashlib.sha256()
        with client.stream("GET", base + f"/api/attachments/{whole}", headers=bob_h) as resp:
            n = 0
            for c in resp.iter_bytes(1024 * 1024):
                h.update(c)
                n += len(c)
                if n >= GIB // 2:
                    break
        h = hashlib.sha256()
        for c in stream(0, GIB // 2):
            h.update(c)
        with client.stream("GET", base + f"/api/attachments/{whole}", headers={**bob_h, "range": f"bytes={GIB // 2}-"}) as resp:
            assert resp.status_code == 206, resp.status_code
            for c in resp.iter_bytes(1024 * 1024):
                h.update(c)
        resumed_ok = h.hexdigest() == want
        print(f"download resumed at 512 MiB with Range: sha256 {'matches' if resumed_ok else 'DIFFERS'}")
        if not resumed_ok:
            failures.append("the resumed download differs")

        # 5. one byte over the limit: refused from its Content-Length, before
        # the body (a raw socket: an HTTP library will not send a short body)
        s = socket.create_connection(("127.0.0.1", port))
        s.sendall((f"POST /api/attachments?name=over.bin HTTP/1.1\r\nhost: hub\r\nx-org-auth: {ann_h['x-org-auth']}\r\n"
                   f"content-length: {GIB + 1}\r\n\r\n").encode() + chunk(0))
        s.settimeout(30)
        status_line = s.recv(4096).split(b"\r\n", 1)[0].decode()
        s.close()
        if " 413 " not in status_line + " ":
            failures.append(f"1 GiB + 1 was not refused: {status_line}")
        r = client.post(base + "/api/uploads", headers=ann_h, json={"bytes": GIB + 1})
        if r.status_code != 413:
            failures.append(f"a resumable 1 GiB + 1 was not refused: {r.status_code}")
        print("1 GiB + 1 byte: refused 413 (one request and resumable)")

        top = peak.bytes()
        print(f"hub peak memory over the whole run: {mib(top)} (at start {mib(start_peak)}) for 2 x 1 GiB up, 3 x 1 GiB down")
        if top > 256 * 1024 * 1024:
            failures.append(f"the hub's memory peaked at {mib(top)}")
        print(json.dumps({"upload_one_request_s": round(up1, 1), "upload_resumable_s": round(up2, 1), "peak_bytes": top}))
    finally:
        hub.terminate()
        try:
            hub.wait(15)
        except subprocess.TimeoutExpired:
            hub.kill()
        shutil.rmtree(a.data, ignore_errors=True)
    for f in failures:
        print("FAILED:", f)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
