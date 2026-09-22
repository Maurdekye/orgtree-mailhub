"""MH01 public-boundary contract runner — the frozen wire profile, executed.

`tests/fixtures/mh01-wire.json` is the CONTRACT: language-neutral cases that
say what the mail hub's public boundary must do. This file is only one DRIVER
for it — the implementation selection deliberately sits outside the
assertions, so the Rust product can be qualified against the same fixtures by
writing a second driver (an HTTP client against a spawned binary) and changing
nothing in the JSON.

What it proves and what it does not:

- It proves the PINNED PYTHON SOURCE at the frozen commit behaves the way the
  profile says. That is the baseline a port has to match.
- It does NOT prove anything about a Rust implementation, and it is not the
  R10 Python-free gate. Running these fixtures needs an interpreter precisely
  because the subject under test is still Python.

Hermetic, following the existing suite's rules (tests/test_hub.py): HUB_DATA
is a throwaway directory set BEFORE the import, the app is driven in-process
through httpx.ASGITransport, and no socket is ever bound. No identity is
registered against a real hub, no listener is armed, and the user's own
~/.orgtree store is never opened.

Secrets never appear in the fixture file. Each case declares symbolic
principals; this driver mints a fresh random slug and secret for each one at
run time, so the profile can be read and reviewed without carrying a
credential.

    <python> tests/mh01_contract.py [-v]
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import shutil
import sqlite3
import sys
import tempfile
import traceback
from datetime import datetime, timedelta, timezone
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parent
sys.path.insert(0, str(_REPO))

FIXTURES = Path(os.environ.get("MH01_FIXTURES") or (_HERE / "fixtures/mh01-wire.json"))
INVENTORY = Path(os.environ.get("MH01_INVENTORY") or (_REPO / "docs/mh01-source-inventory.json"))

HUB_NAME = "mh01-fixture-hub"
RETENTION_DAYS = 30

_TMP = tempfile.mkdtemp(prefix="mh01-wire-")
os.environ["HUB_DATA"] = _TMP                  # BEFORE the import — db reads it
os.environ["HUB_NAME"] = HUB_NAME
os.environ["HUB_RETENTION_DAYS"] = str(RETENTION_DAYS)
os.environ["HUB_ORG_RETENTION_DAYS"] = "45"

import httpx                                                      # noqa: E402
from mailhub import app as hubapp, db                             # noqa: E402
from mailhub.public import PublicHub                              # noqa: E402

VERBOSE = "-v" in sys.argv

# One structured JSON line per request is right for a service and wrong for a
# test run. Shadowing `print` in the app module's own globals silences exactly
# that module and nothing else.
if not VERBOSE:
    hubapp.print = lambda *a, **k: None        # type: ignore[attr-defined]

_SURFACES = {
    "full": httpx.ASGITransport(app=hubapp.app),
    # FR-10: the same app behind the public wrapper. Every request made here is
    # one a remote client could make over the tunnel.
    "public": httpx.ASGITransport(app=PublicHub(hubapp.app)),     # type: ignore[arg-type]
}


# ─────────────────────────────────────────────────────────────── placeholders

_PLACEHOLDER = re.compile(r"^@(\w+)\.([\w-]+)$")
# The same reference EMBEDDED in a longer string, e.g. "/api/attachments/@cap.aid".
# A whole-string match returns the value with its native type; an embedded one
# can only interpolate text.
_EMBEDDED = re.compile(r"@(\w+)\.([\w-]+)")


class Ctx:
    """Per-case state: minted principals and values captured from responses."""

    def __init__(self, principals):
        self.principals = {}
        for name in principals:
            # A slug must satisfy the hub's ^[a-z0-9][a-z0-9._-]{0,127}$; the
            # random tail keeps cases from colliding in the shared store.
            self.principals[name] = {
                "slug": "%s-%s" % (re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-") or "p",
                                   secrets.token_hex(4)),
                "secret": secrets.token_hex(16),
            }
        self.captured = {}

    def auth_header(self, spec, raw=None):
        if raw is not None:
            return raw
        if not spec:
            return None
        pairs = []
        for entry in spec:
            name, _, modifier = entry.partition(":")
            if name not in self.principals:
                raise AssertionError("case does not declare a principal named %r" % name)
            p = self.principals[name]
            secret = p["secret"]
            if modifier == "bad":
                # A wrong secret for a real slug. This is the shape of the
                # address-theft attempt the hub must refuse.
                secret = secrets.token_hex(16)
            elif modifier:
                raise AssertionError("unknown auth modifier %r" % modifier)
            pairs.append("%s:%s" % (p["slug"], secret))
        return " ".join(pairs)

    def resolve(self, value):
        if isinstance(value, dict):
            if "@repeat" in value:
                spec = value["@repeat"]
                return str(spec["unit"]) * int(spec["times"])
            return {k: self.resolve(v) for k, v in value.items()}
        if isinstance(value, list):
            return [self.resolve(v) for v in value]
        if isinstance(value, str):
            m = _PLACEHOLDER.match(value)
            if m:
                return self._lookup(m.group(1), m.group(2), value)
            return _EMBEDDED.sub(
                lambda mm: str(self._lookup(mm.group(1), mm.group(2), value)), value)
        return value

    def _lookup(self, kind, key, original):
        if kind == "cap":
            if key not in self.captured:
                raise AssertionError("no captured value named %r" % key)
            return self.captured[key]
        if kind == "env":
            return {"hub_name": HUB_NAME, "retention_days": RETENTION_DAYS}[key]
        if kind in self.principals:
            return self.principals[kind][key]
        raise AssertionError("unresolved placeholder %r in %r" % ("@%s.%s" % (kind, key), original))


def _at(payload, dotted):
    cur = payload
    for part in dotted.split("."):
        if part == "":
            continue
        if isinstance(cur, list):
            cur = cur[int(part)]
        else:
            cur = cur[part]
    return cur


def _subset(expected, actual, where=""):
    """Every key/value in `expected` must appear in `actual`. Extra keys in
    `actual` are fine — a subset match is what lets the profile pin the fields
    that are contractual without freezing incidental ones."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            raise AssertionError("%s: expected an object, got %r" % (where, type(actual).__name__))
        for k, v in expected.items():
            if k not in actual:
                raise AssertionError("%s: missing key %r (present: %s)"
                                     % (where, k, sorted(actual)))
            _subset(v, actual[k], "%s.%s" % (where, k))
        return
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            raise AssertionError("%s: expected a list of %d, got %r"
                                 % (where, len(expected), actual))
        for i, v in enumerate(expected):
            _subset(v, actual[i], "%s[%d]" % (where, i))
        return
    if expected != actual:
        raise AssertionError("%s: expected %r, got %r" % (where, expected, actual))


# ─────────────────────────────────────────────────────────────── step drivers

def _request(ctx, spec, surface):
    method = spec["method"]
    path = ctx.resolve(spec["path"])
    headers = {}
    auth = ctx.auth_header(spec.get("as"), spec.get("raw_auth"))
    if auth is not None:
        headers["x-org-auth"] = auth
    body_json = ctx.resolve(spec["json"]) if "json" in spec else None
    content = None
    if "content" in spec:
        content = ctx.resolve(spec["content"])
        if isinstance(content, str):
            content = content.encode()
    params = ctx.resolve(spec.get("params")) if spec.get("params") else None

    async def go():
        async with httpx.AsyncClient(transport=_SURFACES[surface],
                                     base_url="http://hub") as c:
            return await c.request(method, path, headers=headers or None,
                                   json=body_json, content=content,
                                   params=params, timeout=60)
    return asyncio.run(go())


def _store(ctx, spec):
    """The two seams a wire profile cannot express: aging a row, and running
    one retention pass. A Rust driver must provide the same two operations —
    they are the time seam, not an implementation detail of Python."""
    op = spec["op"]
    if op == "backdate":
        days = int(spec["days"])
        stamp = (datetime.now(timezone.utc) - timedelta(days=days)) \
            .isoformat(timespec="milliseconds").replace("+00:00", "Z")
        con = db.connect()
        try:
            con.execute("UPDATE %s SET %s=? WHERE %s=?"
                        % (spec["table"], spec["column"], spec["key"]),
                        (stamp, ctx.resolve(spec["value"])))
            con.commit()
        finally:
            con.close()
        return
    if op == "remove_blob":
        # Tear the blob away from its metadata row on purpose, to reach the
        # 410 branch. The sweep normally removes file and row together, so
        # this is the only way to observe the torn state deliberately.
        try:
            os.remove(db.blob_path(str(ctx.resolve(spec["id"]))))
        except OSError:
            pass
        return
    if op == "sweep":
        # The real `_sweep_loop` body, once: its tail sleeps for an hour, so
        # replacing the sleep is what ends the iteration (tests/test_hub.py).
        real_sleep = asyncio.sleep

        async def stop(_s):
            raise asyncio.CancelledError

        asyncio.sleep = stop                                      # type: ignore[assignment]
        try:
            asyncio.run(hubapp._sweep_loop())
        except asyncio.CancelledError:
            pass
        finally:
            asyncio.sleep = real_sleep                            # type: ignore[assignment]
        return
    raise AssertionError("unknown store op %r" % op)


def _assert(ctx, resp, expect, label):
    if "status" in expect and resp.status_code != expect["status"]:
        raise AssertionError("%s: expected HTTP %s, got %s — body %s"
                             % (label, expect["status"], resp.status_code,
                                resp.text[:300]))
    needs_json = any(k in expect for k in
                     ("keys_exactly", "json", "length", "equals_at", "sequence"))
    payload = None
    if needs_json:
        try:
            payload = resp.json()
        except ValueError:
            raise AssertionError("%s: response was not JSON — %s" % (label, resp.text[:300]))
    if "keys_exactly" in expect:
        want = sorted(expect["keys_exactly"])
        got = sorted(payload)
        if want != got:
            raise AssertionError("%s: response keys %s, expected exactly %s" % (label, got, want))
    if "json" in expect:
        _subset(ctx.resolve(expect["json"]), payload, label)
    if "length" in expect:
        for dotted, n in expect["length"].items():
            actual = len(_at(payload, dotted))
            if actual != n:
                raise AssertionError("%s: len(%s) == %d, expected %d" % (label, dotted, actual, n))
    if "equals_at" in expect:
        for dotted, want in expect["equals_at"].items():
            actual = _at(payload, dotted)
            want = ctx.resolve(want)
            if actual != want:
                raise AssertionError("%s: %s == %r, expected %r" % (label, dotted, actual, want))
    if "sequence" in expect:
        spec = expect["sequence"]
        rows = _at(payload, spec["path"])
        actual = [r[spec["field"]] for r in rows]
        want = ctx.resolve(spec["equals"])
        if actual != want:
            raise AssertionError("%s: %s[*].%s == %r, expected %r — ORDER is contractual"
                                 % (label, spec["path"], spec["field"], actual, want))
    if "text_contains" in expect:
        for needle in expect["text_contains"]:
            if needle not in resp.text:
                raise AssertionError("%s: response body does not contain %r" % (label, needle))
    if "header_contains" in expect:
        for name, needle in expect["header_contains"].items():
            got = resp.headers.get(name, "")
            if needle not in got:
                raise AssertionError("%s: header %s == %r, expected to contain %r"
                                     % (label, name, got, needle))


def run_case(case):
    """Execute one fixture case. Raises AssertionError on the first breach."""
    ctx = Ctx(case.get("principals", []))
    surface = case.get("surface", "full")
    for i, step in enumerate(case["steps"]):
        label = "%s step %d" % (case["id"], i + 1)
        if "store" in step:
            _store(ctx, step["store"])
            continue
        resp = _request(ctx, step["request"], step.get("surface", surface))
        if "expect" in step:
            _assert(ctx, resp, step["expect"], label)
        for name, path in (step.get("capture") or {}).items():
            try:
                ctx.captured[name] = _at(resp.json(), path)
            except (ValueError, KeyError, IndexError):
                raise AssertionError("%s: cannot capture %r from %s" % (label, path, resp.text[:200]))


# ───────────────────────────────────────────── fixture ↔ frozen registry ties
# The point of MH01 is that no family is silently omitted. These checks bind
# the profile to the frozen inventory, so "we forgot a route" is a FAILURE
# rather than a smaller green run.

def coverage_errors(profile, inventory):
    errors = []
    exercised_paths, exercised_statuses = set(), set()
    for case in profile["cases"]:
        for step in case["steps"]:
            if "store" in step:
                continue
            req, exp = step["request"], step.get("expect") or {}
            # Normalize a concrete attachment id back to its template form so
            # it matches the registry's `/api/attachments/{aid}`.
            path = req["path"].split("?")[0]
            path = re.sub(r"^/api/attachments/.+$", "/api/attachments/{aid}", path)
            key = (req["method"].upper(), path)
            if case.get("surface", "full") == "full" and step.get("surface", "full") == "full":
                exercised_paths.add(key)
                if "status" in exp:
                    exercised_statuses.add((key, exp["status"]))
    for row in inventory["http_contract"]:
        key = (row["method"], row["path"])
        if key not in exercised_paths:
            errors.append("no fixture exercises %s %s" % key)
            continue
        for status in row["refusal_statuses"]:
            if (key, status) not in exercised_statuses:
                errors.append("no fixture asserts the %s refusal of %s %s" % (status, key[0], key[1]))
    return errors


def profile_errors(profile, inventory):
    errors = list(coverage_errors(profile, inventory))
    if profile.get("source_commit") != inventory.get("source_commit"):
        errors.append("wire profile pins %r but the inventory pins %r"
                      % (profile.get("source_commit"), inventory.get("source_commit")))
    # Secrets must never enter a fixture (docket: secrets stay in their scoped
    # custody and do not enter fixtures, ordinary rows or logs).
    # Scan the CASES only: `source_commit` is a 40-char sha by design, and
    # letting it trip the heuristic would make this check meaningless.
    if re.search(r"\b[0-9a-f]{32,}\b", json.dumps(profile["cases"])):
        errors.append("wire profile contains what looks like a literal credential or digest")
    return errors


# ───────────────────────────────────────────────────────────────── the runner

PASS = 0
FAIL = []


def check(label, fn):
    global PASS
    try:
        fn()
    except Exception as e:                                        # noqa: BLE001
        FAIL.append((label, "".join(traceback.format_exception_only(type(e), e)).strip()))
        print("  FAIL  %s\n        %s" % (label, str(e).replace("\n", "\n        ")))
        return
    PASS += 1
    if VERBOSE:
        print("  ok    %s" % label)


def main():
    profile = json.loads(FIXTURES.read_text(encoding="utf-8"))
    inventory = json.loads(INVENTORY.read_text(encoding="utf-8"))

    print("MH01 public-boundary contract — %d cases against %s"
          % (len(profile["cases"]), profile["source_commit"][:12]))

    print("\n  profile ↔ frozen registry")
    check("every registered route and refusal status is exercised",
          lambda: _expect_empty(profile_errors(profile, inventory)))

    print("\n  cases")
    by_family = {}
    for case in profile["cases"]:
        by_family.setdefault(case["family"], []).append(case)
    for fam in sorted(by_family):
        print("  [%s]" % fam)
        for case in by_family[fam]:
            check(case["id"], lambda c=case: run_case(c))

    # ── bad controls ────────────────────────────────────────────────────────
    # A suite that cannot fail proves nothing. Each of these deliberately
    # breaks something and requires the machinery to NOTICE.
    print("\n  bad controls (each must be REJECTED)")

    def must_raise(label, fn):
        global PASS
        try:
            fn()
        except AssertionError:
            PASS += 1
            if VERBOSE:
                print("  ok    %s" % label)
            return
        except Exception as e:                                    # noqa: BLE001
            FAIL.append((label, "raised %r, expected AssertionError" % e))
            print("  FAIL  %s — raised %r, expected AssertionError" % (label, e))
            return
        FAIL.append((label, "was ACCEPTED"))
        print("  FAIL  %s — was ACCEPTED, so the check does not discriminate" % label)

    first = profile["cases"][0]

    must_raise("a case whose expected status is wrong must fail",
               lambda: run_case(_mutate_status(first)))
    must_raise("a case whose expected response keys are wrong must fail",
               lambda: run_case(_mutate_keys(first)))
    must_raise("an unresolvable principal placeholder must fail",
               lambda: run_case(_mutate_placeholder(first)))
    must_raise("dropping a route from the profile must be caught by coverage",
               lambda: _expect_empty(coverage_errors(_drop_route(profile), inventory)))
    must_raise("dropping a refusal assertion must be caught by coverage",
               lambda: _expect_empty(coverage_errors(_drop_refusal(profile), inventory)))
    must_raise("a profile pinned to a different commit must be caught",
               lambda: _expect_empty(profile_errors(
                   {**profile, "source_commit": "0" * 40}, inventory)))

    print("\n%d passed, %d failed" % (PASS, len(FAIL)))
    if FAIL:
        print("\nfailures:")
        for label, why in FAIL:
            print("  - %s: %s" % (label, why))
    shutil.rmtree(_TMP, ignore_errors=True)
    return 1 if FAIL else 0


def _expect_empty(errors):
    if errors:
        raise AssertionError("; ".join(errors))


def _clone(case):
    return json.loads(json.dumps(case))


def _mutate_status(case):
    bad = _clone(case)
    for step in bad["steps"]:
        if "expect" in step and "status" in step["expect"]:
            step["expect"]["status"] = 599
            break
    bad["id"] = case["id"] + " [status mutated]"
    return bad


def _mutate_keys(case):
    bad = _clone(case)
    for step in bad["steps"]:
        if "expect" in step and "keys_exactly" in step["expect"]:
            step["expect"]["keys_exactly"] = ["definitely_not_a_field"]
            break
    else:
        for step in bad["steps"]:
            if "expect" in step:
                step["expect"]["keys_exactly"] = ["definitely_not_a_field"]
                break
    bad["id"] = case["id"] + " [keys mutated]"
    return bad


def _mutate_placeholder(case):
    bad = _clone(case)
    bad["principals"] = []
    bad["id"] = case["id"] + " [principals dropped]"
    return bad


def _drop_route(profile):
    thin = _clone(profile)
    thin["cases"] = [c for c in thin["cases"] if c["family"] != "health"]
    return thin


def _drop_refusal(profile):
    thin = _clone(profile)
    for case in thin["cases"]:
        for step in case["steps"]:
            exp = step.get("expect") or {}
            if exp.get("status") == 413:
                exp["status"] = 200
    return thin


if __name__ == "__main__":
    raise SystemExit(main())
