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
import contextlib
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
    # Reviewer finding F1: the default transport re-raises an unhandled handler
    # exception into the CALLER, so a malformed body looked like a Python
    # traceback rather than a response. That is a test artifact, not the public
    # boundary -- under uvicorn the same request is answered 500. This surface
    # turns the re-raise off so the profile can freeze the response a real
    # client actually receives. Treating the exception itself as the contract
    # would be exactly the mistake the docket warns about.
    "full-served": httpx.ASGITransport(app=hubapp.app, raise_app_exceptions=False),
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
    if spec.get("content_type"):
        # Needed for the malformed-body cases: the bytes are sent verbatim, so
        # nothing else declares the media type the handler will try to parse.
        headers["content-type"] = ctx.resolve(spec["content_type"])
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
                     ("keys_exactly", "json", "length", "equals_at", "sequence",
                      "detail", "detail_contains", "detail_type", "row", "no_row",
                      "keys_exactly_at"))
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
    # ---- the refusal envelope, not just its status code -------------------
    # Reviewer finding F1: an implementation answering every refusal with the
    # same wrong sentence passed a status-only profile. `detail` is the rest of
    # the envelope and the frozen strings come from the pinned AST, so what is
    # asserted here is the source's own text rather than a transcription of it.
    if "detail" in expect:
        if not isinstance(payload, dict) or "detail" not in payload:
            raise AssertionError("%s: refusal body has no `detail` member — got %r"
                                 % (label, payload))
        want = ctx.resolve(expect["detail"])
        if payload["detail"] != want:
            raise AssertionError("%s: detail == %r, expected %r — the refusal ENVELOPE is "
                                 "contractual, not only the status"
                                 % (label, payload["detail"], want))
    if "detail_contains" in expect:
        # For the f-string refusals, where the interpolation is a per-request
        # value: the constant runs around it are contractual, the value is not.
        if not isinstance(payload, dict) or "detail" not in payload:
            raise AssertionError("%s: refusal body has no `detail` member — got %r"
                                 % (label, payload))
        got = payload["detail"]
        for needle in expect["detail_contains"]:
            if ctx.resolve(needle) not in got:
                raise AssertionError("%s: detail %r does not contain %r"
                                     % (label, got, ctx.resolve(needle)))
    if "detail_type" in expect:
        # Hand-raised refusals in app.py carry a STRING detail. FastAPI's own
        # query validation carries a LIST of structured pydantic errors under
        # the same key. Two different surfaces behind one member name, and the
        # docket requires a port to decide each deliberately rather than
        # inherit whatever its framework does -- so the shape is frozen here.
        want = expect["detail_type"]
        got = payload.get("detail") if isinstance(payload, dict) else None
        kinds = {"string": str, "list": list, "object": dict}
        if want not in kinds:
            raise AssertionError("%s: unknown detail_type %r" % (label, want))
        if not isinstance(got, kinds[want]):
            raise AssertionError("%s: detail is %s, expected %s — the SHAPE of the refusal "
                                 "envelope is contractual"
                                 % (label, type(got).__name__, want))
    if "keys_exactly_at" in expect:
        for dotted, want in expect["keys_exactly_at"].items():
            got = sorted(_at(payload, dotted))
            if got != sorted(want):
                raise AssertionError("%s: keys at %s are %s, expected exactly %s"
                                     % (label, dotted, got, sorted(want)))
    # ---- postconditions on actual rows ------------------------------------
    # The other half of F1: `keys_exactly: ["name","roster"]` is satisfied by an
    # EMPTY roster, so the named roster/kind/refresh cases proved nothing about
    # the rows themselves. `row` selects one row and asserts its content.
    if "row" in expect:
        for spec in (expect["row"] if isinstance(expect["row"], list) else [expect["row"]]):
            rows = _at(payload, spec["path"])
            if not isinstance(rows, list):
                raise AssertionError("%s: %s is not a list" % (label, spec["path"]))
            where = ctx.resolve(spec["where"])
            hits = [r for r in rows
                    if all(isinstance(r, dict) and r.get(k) == v for k, v in where.items())]
            if len(hits) != 1:
                raise AssertionError(
                    "%s: expected exactly ONE row in %s matching %r, found %d — rows present: %r"
                    % (label, spec["path"], where, len(hits), rows))
            hit = hits[0]
            if "keys_exactly" in spec:
                got, want = sorted(hit), sorted(spec["keys_exactly"])
                if got != want:
                    raise AssertionError("%s: row keys %s, expected exactly %s" % (label, got, want))
            if "fields" in spec:
                _subset(ctx.resolve(spec["fields"]), hit, "%s row" % label)
            for name in spec.get("present", []):
                if name not in hit:
                    raise AssertionError("%s: row is missing field %r (present: %s)"
                                         % (label, name, sorted(hit)))
            for name in spec.get("non_empty", []):
                if not hit.get(name):
                    raise AssertionError("%s: row field %r is empty (%r) but must carry a value"
                                         % (label, name, hit.get(name)))
    if "no_row" in expect:
        for spec in (expect["no_row"] if isinstance(expect["no_row"], list) else [expect["no_row"]]):
            rows = _at(payload, spec["path"])
            where = ctx.resolve(spec["where"])
            hits = [r for r in rows
                    if all(isinstance(r, dict) and r.get(k) == v for k, v in where.items())]
            if hits:
                raise AssertionError("%s: expected NO row in %s matching %r, found %r"
                                     % (label, spec["path"], where, hits))
    if "text_equals" in expect:
        # The FR-10 listener's own 404 is a RAW ASGI body (public.py writes
        # b"not found"), not a {"detail": ...} envelope like every hand-raised
        # refusal in app.py. Freezing it as text is the point: a port that
        # answers this one with JSON has changed the public surface.
        want = ctx.resolve(expect["text_equals"])
        if resp.text != want:
            raise AssertionError("%s: body == %r, expected exactly %r" % (label, resp.text, want))
    if expect.get("not_json"):
        try:
            resp.json()
        except ValueError:
            pass
        else:
            raise AssertionError("%s: body parsed as JSON (%r) but this refusal is contractually "
                                 "a raw non-JSON body" % (label, resp.text[:200]))
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

    # ── controls against a bad IMPLEMENTATION, not a bad expectation ────────
    print("\n  implementation controls (a broken product must go RED)")
    for label, patch, case_ids in IMPLEMENTATION_CONTROLS:
        check(label, lambda l=label, p=patch, c=case_ids: _expect_empty(
            implementation_control_errors(l, p, c, profile["cases"])))

    print("\n%d passed, %d failed" % (PASS, len(FAIL)))
    if FAIL:
        print("\nfailures:")
        for label, why in FAIL:
            print("  - %s: %s" % (label, why))
    shutil.rmtree(_TMP, ignore_errors=True)
    return 1 if FAIL else 0


# ── discriminating controls against a BAD IMPLEMENTATION ───────────────────
# The controls above mutate the PROFILE: they prove the checker rejects a wrong
# expectation. Reviewer finding F1 showed that is only half a suite. A profile
# asserting status codes and top-level keys was satisfied by a product whose
# roster was always empty and whose every refusal carried the same wrong
# sentence -- both scored 63/63.
#
# These controls close that hole permanently. Each breaks the PRODUCT at the
# public boundary, leaves the profile untouched, and requires the named cases to
# go red. They are the standing proof that the profile constrains behaviour
# rather than shape, so a later edit that softens an assertion fails here
# instead of passing quietly.

@contextlib.contextmanager
def _patched(module, name, value):
    missing = object()
    original = getattr(module, name, missing)
    setattr(module, name, value)
    try:
        yield original
    finally:
        if original is missing:
            delattr(module, name)
        else:
            setattr(module, name, original)


def _empty_roster():
    return _patched(hubapp, "_roster", lambda con: [])


def _mapped_roster(fn):
    original = hubapp._roster
    return _patched(hubapp, "_roster", lambda con: [fn(dict(r)) for r in original(con)])


def _wrong_refusal_detail():
    original = hubapp.HTTPException
    return _patched(hubapp, "HTTPException",
                    lambda status_code, detail=None, **kw:
                    original(status_code, detail="deliberately incompatible refusal", **kw))


def _drop_row_field(name):
    return _mapped_roster(lambda r: {k: v for k, v in r.items() if k != name})


def _force_kind_org():
    return _mapped_roster(lambda r: {**r, "kind": "org"})


# (label, patch factory, the case ids that MUST go red while it is applied)
IMPLEMENTATION_CONTROLS = [
    ("an implementation whose roster is always empty must be caught",
     _empty_roster,
     ["register.first-registration-returns-hub-identity-and-roster",
      "register.roster-row-carries-display-fields-and-kind",
      "register.kind-chat-is-recorded",
      "register.re-registration-refreshes-display-fields"]),
    ("an implementation whose refusals all carry the wrong detail must be caught",
     _wrong_refusal_detail,
     ["register.malformed-slug-is-refused-422",
      "roster.requires-credentials-401",
      "send.unauthenticated-is-refused-401",
      "attachments.download-of-an-unknown-id-is-404"]),
    ("an implementation that drops last_seen from the roster row must be caught",
     lambda: _drop_row_field("last_seen"),
     ["register.roster-row-carries-display-fields-and-kind"]),
    ("an implementation that loses the chat/org kind distinction must be caught",
     _force_kind_org,
     ["register.kind-chat-is-recorded"]),
]


def implementation_control_errors(label, patch, case_ids, cases):
    """Apply one product mutation; every named case must FAIL while it holds."""
    survived = []
    with patch():
        for cid in case_ids:
            case = next((c for c in cases if c["id"] == cid), None)
            if case is None:
                survived.append("%s: no such case in the profile" % cid)
                continue
            try:
                run_case(case)
            except AssertionError:
                continue                       # correct -- the mutation was noticed
            survived.append("%s: PASSED under a deliberately broken implementation" % cid)
    return survived


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


# ── the shared runner's result envelope ────────────────────────────────────
# Reviewer finding F3: the narrative loop above emits prose, so
# tools/run-python-verification.py recorded `tests_ran: null` and filed this
# module under `non_failing_modules_without_tests`. The qualification receipt
# validator needs a positive integer denominator, so the suite could not be
# consumed as coverage no matter how much it actually checked.
#
# Every fixture case, every profile control and every implementation control is
# published here as a standard unittest test. The runner gets its `Ran N tests`
# line, and the denominator is the real one -- an omitted case or a dropped
# control shrinks it rather than silently leaving a green run the same size.
# Both entry points execute the SAME functions, so there is one source of truth.

import unittest                                                   # noqa: E402


def _load():
    return (json.loads(FIXTURES.read_text(encoding="utf-8")),
            json.loads(INVENTORY.read_text(encoding="utf-8")))


class MH01Contract(unittest.TestCase):
    """The frozen wire profile, one test per case and per control."""


def _method_name(prefix, label):
    return "test_%s_%s" % (prefix, re.sub(r"\W+", "_", label).strip("_"))


def _install_tests():
    profile, inventory = _load()
    cases = profile["cases"]

    def add(prefix, label, fn):
        fn.__name__ = _method_name(prefix, label)
        fn.__doc__ = label
        setattr(MH01Contract, fn.__name__, fn)

    add("registry", "every registered route and refusal status is exercised",
        lambda self: _expect_empty(profile_errors(profile, inventory)))

    for case in cases:
        add("case", case["id"],
            lambda self, c=case: run_case(c))

    # Controls against a bad EXPECTATION: the checker must reject each one.
    first = cases[0]
    for label, thunk in [
        ("a case whose expected status is wrong must fail",
         lambda: run_case(_mutate_status(first))),
        ("a case whose expected response keys are wrong must fail",
         lambda: run_case(_mutate_keys(first))),
        ("an unresolvable principal placeholder must fail",
         lambda: run_case(_mutate_placeholder(first))),
        ("dropping a route from the profile must be caught by coverage",
         lambda: _expect_empty(coverage_errors(_drop_route(profile), inventory))),
        ("dropping a refusal assertion must be caught by coverage",
         lambda: _expect_empty(coverage_errors(_drop_refusal(profile), inventory))),
        ("a profile pinned to a different commit must be caught",
         lambda: _expect_empty(profile_errors({**profile, "source_commit": "0" * 40}, inventory))),
    ]:
        def control(self, t=thunk):
            with self.assertRaises(AssertionError):
                t()
        add("control", label, control)

    # Controls against a bad IMPLEMENTATION: the product is broken at the
    # public boundary and the named cases must go red.
    for label, patch, case_ids in IMPLEMENTATION_CONTROLS:
        def impl(self, l=label, p=patch, ids=case_ids):
            _expect_empty(implementation_control_errors(l, p, ids, cases))
        add("implementation", label, impl)


_install_tests()


def tearDownModule():
    shutil.rmtree(_TMP, ignore_errors=True)


if __name__ == "__main__":
    # `--narrative` keeps the grouped human-readable report; the default is
    # unittest, because that is what the shared runner can count.
    if "--narrative" in sys.argv:
        raise SystemExit(main())
    unittest.main(argv=[sys.argv[0]] + [a for a in sys.argv[1:] if a != "-v"],
                  verbosity=2 if VERBOSE else 1)
