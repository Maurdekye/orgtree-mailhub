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

## Known V1 gaps carried across deliberately

The V1 suites assert these as *known gaps/findings* and this import does not
fix them (faithful carry-across is the instruction; fixes need a ruling):

- `db.blob_path()` sanitizes an attachment id to alphanumerics; an id that
  sanitizes to nothing resolves to the blob DIRECTORY itself. Unreachable
  today (ids are server-minted hex); flagged in `tests/test_hub.py`.
- One session with both the hubtool MCP tools and an armed listener has two
  consumers on one deliver-once mailbox (`tests/test_hubtool.py` finding).
