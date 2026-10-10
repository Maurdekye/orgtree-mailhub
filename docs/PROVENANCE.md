# Provenance

This repository is the Orgtree V1 mail hub, extracted verbatim and made the
authoritative shared implementation. Orgtree consumes it as a Git submodule
pinned to an exact reviewed commit.

**Baseline**: `Maurdekye/claude-orgtree` revision
`a8199a598f0c62216ed41cf3e1a099d46517f43d`
(last commit touching `hub/` at import time: `fc99552`,
"Hub UI: per-row copy-id button on each listed client").

The first commit of this repository imports the V1 files byte-identical; its
commit message carries the full file mapping. `docs/mailserver-spec.md` is the
binding V1 design record (its §12 table lists the user rulings the hub
implements).

## Deviation ledger

Every intentional difference from the V1 baseline is listed here. Anything not
listed is meant to be byte-identical or behavior-identical to V1.

1. **Layout** — `hub/*` moved to the repository root (`hub/mailhub/` →
   `mailhub/`, tools and Docker assets to the root). Mechanical; no behavior
   change. Docker build context semantics are unchanged (`compose.yaml` built
   from the repo root exactly as it was built from `hub/`).
2. **Tests** — `backend/tests/test_hub.py` and `backend/tests/test_hubtool.py`
   imported as `tests/`; only their repo-root/sys.path resolution lines were
   adapted to the new layout. These two suites are the executable V1
   characterization baseline: all 61 + 36 checks pass unmodified otherwise.
3. **`hub/.env` not imported** — it carried the live deployment's values.
   `.env.example` documents every variable instead.
4. **Scaffolding added** — `.gitignore`, `.gitattributes`, `.dockerignore`,
   this file. No product behavior.
5. **hubtool identity storage: JSON files → SQLite** (the one ruled product
   enhancement, 2026-09-14: all mutable hub data is SQLite; no active JSON
   store may remain). Identities now live in
   `~/.orgtree/hub-clients/clients.sqlite3` (WAL, synchronous=FULL,
   0o600); the old one-file-per-identity JSONs are read ONCE as migration
   input — deterministic, idempotent, transactional, recorded in the
   `migrations` table, and strictly non-destructive (sources never renamed,
   rewritten or deleted; they are the rollback boundary). Every identity
   property survives: uid (the secret → the same address), hub list order,
   per-hub seen-ring order. Behavioral deltas, all strictly narrower than
   V1's: a corrupt pre-SQLite file is preserved IN PLACE instead of being
   quarantine-renamed (`register` still refuses read-only verbs and warns
   `reminted`); a name owned by both a database row and a JSON with a
   different secret is a recorded CONFLICT — the active row wins, nothing
   is imported, and `register` discloses it (`migration_conflict`).
   `tests/test_hubtool.py` keeps every behavioral check (36/36) with only
   its storage probes translated (fsync probes → synchronous=FULL +
   commit-before-return; quarantine → preserved-in-place);
   `tests/test_hubtool_migration.py` covers the migration itself (7 checks).
   The hub SERVER needed no change: V1 already stored everything in SQLite.
6. **`serve.py` honors `HUB_BIND` at the app** (`de2ffc4`, recorded here
   retroactively — it landed without a ledger entry). V1's compose already
   used `HUB_BIND` to qualify the host-side port mapping; the app itself
   always bound 0.0.0.0. For non-Docker hosting (the embedding desktop
   process) loopback-only must be expressible at the app, so `mailhub.serve`
   now reads it directly. Default unchanged; Docker behavior unchanged.
7. **`HUB_PUBLIC_BIND` + parameterized container name** (2026-09-16,
   coordinator ruling on a cross-org request from neoja). The public
   listener's interface becomes expressible the same two ways as the full
   app's: `HUB_PUBLIC_BIND` qualifies the compose mapping's host interface
   and is honored directly by `mailhub.serve` outside Docker; default stays
   0.0.0.0 both places, so no deployment moves. `HUB_PUBLIC_HOST_PORT`
   remains a bare port (an IP embedded in it used to interpolate into a
   valid mapping by accident; that form now fails compose validation).
   `container_name` becomes `${HUB_CONTAINER_NAME:-orgtree-mailhub}` —
   container names are host-global, so a fixed one blocked a second
   instance per host; the default is unchanged.

8. **v2.0.0: the hub rewritten in Rust with PostgreSQL storage** (2026-10-08,
   user ruling; docket item `mail-hub-v2-0-rewrite-in-rust-with-postgres-stor`).
   The server is the `hub/` crate (binary `orgtree-mailhub`); the Python
   server (`mailhub/`) and its suite (`tests/test_hub.py`) left the tree after
   the Phase 1 review, the suite ported check for check to
   `hub/tests/hub_suite.rs`. The protocol is unchanged; the deliberate
   differences are listed with their reasons in `docs/v2.md`. The last v1
   server (79a7c51, with the 1 GiB limit) stays reachable through git
   history: `tests/v1_reference.py` extracts it for the side-by-side
   comparisons and for hubtool's suite. `hubtool.py` is unchanged.

9. **v2.0.1: lazy history, and hubtool on several hubs** (2026-10-09,
   coordinator rulings; docket items
   `hubtool-hub-history-merges-every-hub-an-identity` and
   `hubchat-time-based-lazy-history-fetch-across-hub`). `hub_history` reads
   every hub on an identity's list and merges the conversation, with a
   cursor that means the same on every hub. A new device can sync from now
   (`start: "now"`) and page back by time (`before=<unix ms>`), and
   `/healthz` and sync answers carry the hub's clock (`now`). `/healthz` on
   the main port also says where the relay-only door listens (`door`), for
   Hubchat's phone linking (`hubchat-1-0-0-milestone`, item 1). All
   additive: v2.0.0 clients see no change.
10. **v2.0.2: presence that notices a client is gone** (2026-10-09,
   coordinator rulings 22:48Z; docket item
   `hubchat-shows-an-org-as-online-while-its-orgtree`, from the user's
   report that an Orgtree killed by an update stayed "online" in Hubchat).
   A parked poll or sync that ends with its client hung up leaves the
   address online only for a 10 s grace, not v1's 90 s window (a deliberate
   difference, docs/v2.md row 30); parked syncs of devices in use hear of
   presence changes within a second; `HUB_PUBLIC_ADVERTISE` names the
   door's address as clients reach it (`door.advertise`). TCP keepalive was
   tried and left out: probes every few seconds would wake phones' radios
   (coordinator's ruling, 23:20Z), so a client lost without closing its
   connection still shows offline after about 2 to 2.5 minutes.
11. **v2.0.3: presence follow-ups** (2026-10-10, coordinator's ruling 01:04Z;
   docket item `hub-v2-0-3-presence-follow-ups-fresh-last-seen-i`, found in
   the real-Hubchat check of v2.0.2). When an address goes offline its roster
   entry is marked changed, so syncs carry its `last_seen` as it stands
   (Hubchat showed the registration time); a device that reports itself no
   longer in use wakes its parked sync, which then stops being woken for
   presence at once instead of when its park ends.
12. **v2.1.0: optional UnifiedPush** (2026-10-10, docket item
   `hubchat-optional-unifiedpush-notifications-along`; version 2.1.0 by the
   coordinator's ruling 18:12Z). Device-owned subscriptions and encrypted
   content-free wakes, sent directly to an installed distributor's server.
   The user requires no project-hosted relay and keeps the background
   connection as default. Coordinator approved storing the RFC8291
   p256dh/auth protocol metadata; capabilities stay out of logs and listings
   and are removed on device/identity removal. See v2-additions.md for the
   API and endpoint policy; tests are listed in v2.md. The first hub that
   makes outbound requests of its own (only once a device registers for
   push). Schema 9 is one-way: a 2.0.x hub refuses a database a 2.1.0 hub
   has migrated (OPERATIONS.md, Upgrades).

## Known V1 gaps carried across deliberately

The V1 suites assert these as *known gaps/findings* and this import does not
fix them (faithful carry-across is the instruction; fixes need a ruling):

- `db.blob_path()` sanitizes an attachment id to alphanumerics; an id that
  sanitizes to nothing resolves to the blob DIRECTORY itself. Unreachable
  today (ids are server-minted hex); flagged in `tests/test_hub.py`.
- One session with both the hubtool MCP tools and an armed listener has two
  consumers on one deliver-once mailbox (`tests/test_hubtool.py` finding).
