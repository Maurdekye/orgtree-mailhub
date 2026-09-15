"""hubtool identity migration — pre-SQLite JSON files as read-only input.

The orgtree-mailhub ruling (2026-09-14) moves all mutable hub data to SQLite.
For hubtool that is the per-session identity store, and the OLD store — one
JSON file per identity under ~/.orgtree/hub-clients/ — becomes MIGRATION
INPUT with hard properties this suite pins:

    §1  every identity survives byte-for-byte: uid (the secret), the derived
        address, the hub list IN ORDER, the per-hub seen rings IN ORDER
    §2  the source JSONs are READ-ONLY: never renamed, rewritten or deleted
    §3  idempotent and deterministic: a second pass imports nothing new; a
        rebuilt database imports the identical state
    §4  the migration is RECORDED durably (the `migrations` table)
    §5  a name owned by BOTH a database row and a JSON with a different
        secret is a CONFLICT: the active row wins, nothing is guessed, the
        file is preserved, and register() says so out loud
    §6  the legacy single-profile file (~/.orgtree/hub-client.json) migrates
        under its own recorded name

Hermetic: HOME points at a throwaway directory; no hub, no sockets — the
migration is pure local storage work.

    python tests/test_hubtool_migration.py [-v]
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import traceback

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.normpath(os.path.join(_HERE, ".."))
sys.path.insert(0, _REPO)

_TMP = tempfile.mkdtemp(prefix="orgtree-hubtool-mig-")
os.environ["USERPROFILE"] = os.environ["HOME"] = os.path.join(_TMP, "home")
os.makedirs(os.environ["HOME"], exist_ok=True)
os.environ["MAILHUB_URL"] = "http://hub.test"
os.environ.pop("MAILHUB_NAME", None)

import hubtool                                                   # noqa: E402

hubtool._ID_DIR = os.path.join(_TMP, "home", ".orgtree", "hub-clients")
hubtool._LEGACY_ID = os.path.join(_TMP, "home", ".orgtree", "hub-client.json")

PASS = 0
FAIL: list[tuple[str, str]] = []


def check(label, fn) -> None:
    global PASS
    try:
        fn()
    except Exception:                                            # noqa: BLE001
        FAIL.append((label, traceback.format_exc()))
        print(f"  FAIL     {label}")
        return
    PASS += 1
    print(f"  ok {PASS:3d}  {label}")


def wipe() -> None:
    shutil.rmtree(hubtool._ID_DIR, ignore_errors=True)
    try:
        os.remove(hubtool._LEGACY_ID)
    except OSError:
        pass
    hubtool._CUR.clear()


def plant(name: str, d: dict) -> str:
    os.makedirs(hubtool._ID_DIR, exist_ok=True)
    p = hubtool._id_path(name)
    with open(p, "w", encoding="utf-8") as f:
        json.dump(d, f, indent=1)
    return p


def raw(path: str) -> bytes:
    return open(path, "rb").read()


def rows(sql: str, args=()) -> list:
    con = hubtool._db()
    try:
        return [dict(r) for r in con.execute(sql, args).fetchall()]
    finally:
        con.close()


UID_A = "a" * 64
UID_B = "b" * 64
UID_C = "c" * 64
SLUG_A = f"alpha.user.{hashlib.sha256(UID_A.encode()).hexdigest()[:6]}"


def _survives_complete() -> None:
    wipe()
    p = plant("alpha", {
        "uid": UID_A, "name": "alpha", "slug": SLUG_A,
        "hubs": ["http://one.test:7370", "http://two.test:7370"],
        "seen": {"http://one.test:7370": ["m1", "m2", "m3"],
                 "http://two.test:7370": ["n1"]},
    })
    before = raw(p)
    d = hubtool._load_ident("alpha")           # any store open migrates
    assert d.get("uid") == UID_A, d
    assert d.get("hubs") == ["http://one.test:7370",
                             "http://two.test:7370"], d
    assert d.get("seen", {}).get("http://one.test:7370") == \
        ["m1", "m2", "m3"], d
    assert d.get("seen", {}).get("http://two.test:7370") == ["n1"], d
    # the address derives from the uid, so the SAME uid is the SAME
    # fingerprint suffix: the storage move can never strand a hub-registered
    # identity. (The username segment is re-derived from the CURRENT OS user
    # on every load — V1 hubtool behavior, carried faithfully; only orgs pin
    # their slug verbatim.)
    full = hubtool._ident("alpha")
    fp6 = hashlib.sha256(UID_A.encode()).hexdigest()[:6]
    assert full["slug"] == f"alpha.{hubtool._user()}.{fp6}", full["slug"]
    assert raw(p) == before, "the source JSON was modified"


def _flat_ring_migrates_under_bootstrap() -> None:
    wipe()
    plant("beta", {"uid": UID_B, "name": "beta",
                   "seen_ids": ["x1", "x2"]})    # pre-multi-hub shape
    d = hubtool._load_ident("beta")
    assert d.get("uid") == UID_B
    assert d.get("seen", {}).get(hubtool.HUB) == ["x1", "x2"], d


def _idempotent_and_recorded() -> None:
    wipe()
    p = plant("gamma", {"uid": UID_C, "name": "gamma",
                        "seen": {"http://one.test:7370": ["z1"]}})
    hubtool._load_ident("gamma")
    rec = rows("SELECT source_path, sha256, state FROM migrations")
    assert len(rec) == 1 and rec[0]["state"] == "imported", rec
    assert rec[0]["sha256"] == hashlib.sha256(raw(p)).hexdigest(), rec
    # a second pass changes nothing — same row, same single record
    hubtool._load_ident("gamma")
    rec2 = rows("SELECT source_path, sha256, state FROM migrations")
    assert rec2 == rec, (rec2, rec)
    ids = rows("SELECT uid FROM identities WHERE name='gamma'")
    assert len(ids) == 1 and ids[0]["uid"] == UID_C


def _deterministic_rebuild() -> None:
    wipe()
    plant("alpha", {"uid": UID_A, "name": "alpha",
                    "hubs": ["http://one.test:7370"],
                    "seen": {"http://one.test:7370": ["m1", "m2"]}})
    first = hubtool._load_ident("alpha")
    # rebuild the database from nothing but the same inputs
    for f in list(os.listdir(hubtool._ID_DIR)):
        if f.startswith("clients.sqlite3"):
            os.remove(os.path.join(hubtool._ID_DIR, f))
    second = hubtool._load_ident("alpha")
    assert first == second, (first, second)


def _conflict_is_loud_and_lossless() -> None:
    wipe()
    minted = hubtool._ident("delta")           # the ACTIVE identity, in the DB
    assert minted["uid"] != UID_A
    p = plant("delta", {"uid": UID_A, "name": "delta"})   # a rival V1 file
    before = raw(p)
    d = hubtool._load_ident("delta")
    assert d["uid"] == minted["uid"], "the conflict overwrote the active row"
    rec = rows("SELECT state FROM migrations WHERE source_path=?", (p,))
    assert rec and rec[0]["state"] == "conflict", rec
    assert raw(p) == before, "the conflicting source file was modified"
    out = hubtool.register("delta")            # hub is down: local result only
    assert out.get("migration_conflict"), (
        "a secret split between file and database was not disclosed")


def _legacy_single_profile() -> None:
    wipe()
    os.makedirs(os.path.dirname(hubtool._LEGACY_ID), exist_ok=True)
    with open(hubtool._LEGACY_ID, "w", encoding="utf-8") as f:
        json.dump({"uid": UID_B, "name": "old-solo"}, f)
    before = raw(hubtool._LEGACY_ID)
    d = hubtool._load_ident("old-solo")
    assert d.get("uid") == UID_B, d
    assert raw(hubtool._LEGACY_ID) == before
    assert "old-solo" in hubtool._known_names()


def _non_identity_and_corrupt_left_alone() -> None:
    wipe()
    os.makedirs(hubtool._ID_DIR, exist_ok=True)
    junk = os.path.join(hubtool._ID_DIR, "notes.json")
    with open(junk, "w", encoding="utf-8") as f:
        f.write('{"hello": 1}')                # parses, but no uid
    wreck = hubtool._id_path("torn")
    with open(wreck, "wb") as f:
        f.write(b"\x00\x00")                   # does not parse
    hubtool._known_names()                     # opens the store, migrates
    assert raw(junk) == b'{"hello": 1}'
    assert raw(wreck) == b"\x00\x00"
    assert rows("SELECT * FROM migrations") == [], (
        "a non-identity or unreadable file gained a migration record — "
        "records are for inputs actually accounted for")
    assert hubtool._known_names() == []


print("hubtool migration — pre-SQLite JSON as read-only input")
print("\n§1  complete survival (uid, address, hub order, ring order)")
check("an identity migrates whole and its address is unchanged",
      _survives_complete)
check("a pre-multi-hub flat ring lands under the bootstrap hub",
      _flat_ring_migrates_under_bootstrap)
print("\n§2/§3/§4  read-only sources · idempotent · recorded · deterministic")
check("migration is recorded once (path + sha256) and a second pass is a "
      "no-op", _idempotent_and_recorded)
check("a rebuilt database migrates to the identical state",
      _deterministic_rebuild)
print("\n§5  a secret split between database and file")
check("conflict: the active row wins, the file survives untouched, and "
      "register() discloses it", _conflict_is_loud_and_lossless)
print("\n§6  the legacy single-profile file")
check("hub-client.json migrates under its own recorded name",
      _legacy_single_profile)
check("non-identity and unreadable files are left exactly alone",
      _non_identity_and_corrupt_left_alone)

print()
if FAIL:
    for label, tb in FAIL:
        print(f"\n✗ {label}\n{tb}")
    print(f"hubtool migration: {PASS} passed · {len(FAIL)} FAILED")
    shutil.rmtree(_TMP, ignore_errors=True)
    sys.exit(1)
print(f"hubtool migration: all {PASS} checks passed")
shutil.rmtree(_TMP, ignore_errors=True)
