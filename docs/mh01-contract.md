# MH01 — the mail hub's frozen contract

**Status:** MH01 candidate for independent source review. Inventory and freeze only.
**Pinned source:** `6e856eec8ccfbcf8d16451123903b9d2ce16450b` (this repository, `main` at grant time).
**Branch:** `rust/mailhub-product`.

This document is the readable half of the freeze. The checkable half is
`docs/mh01-source-inventory.json`, generated from pinned Git blobs by
`tools/mh01_inventory.py`, and `tests/fixtures/mh01-wire.json`, executed by
`tests/mh01_contract.py`. Where this prose and those files disagree, the
generated files win — they are derived from the source, and this is written by
hand.

## What this slice is, and what it is not

MH01 enumerates and freezes. It does not port anything.

| Claim | Status |
|---|---|
| Every source file, route, schema, SQL site, config source and launch dependency is enumerated and hash-pinned | **Delivered** |
| Every explicit HTTP route, refusal status and refusal **envelope** is frozen as executable, language-neutral fixtures | **Delivered** |
| Durable and protocol families a file census cannot see — queues, process custody, output artifacts, the MCP envelope — are dispositioned against source anchors | **Delivered** |
| Every place the product requires a Python interpreter is recorded with a disposition, fail-closed | **Delivered** |
| MCP, CLI and listener behaviour is **executed** | **No** — frozen from source and recorded as blocking unknowns; executing it needs a live listener, which this slice is forbidden |
| A Rust mail hub exists | **Not in this slice** — that is MH02 |
| The packaged backend runs with no usable Python | **Not in this slice** — that is R10 |

An earlier revision of this table claimed every public HTTP *behaviour* was
frozen. Independent review (wire-contract-astra, 2026-09-22, finding F1) showed
that claim was false: the fixtures asserted status codes and top-level response
keys, so a product whose roster was always empty and whose every refusal carried
the same wrong sentence still scored a full green run. The row above is narrowed
to what is actually asserted, and the gap it names is now closed by row
postconditions, refusal-envelope assertions and standing controls that break the
product on purpose (see *How to verify this candidate*).

The kickoff for this slice asked for "proof that package/runtime paths no
longer require Python for the mailhub service or schema initialization."
MH01 cannot produce that proof: there is nothing yet to run Python-free.
What it delivers instead is the thing that proof depends on — a complete,
fail-closed register of every interpreter dependency that exists today, so
R10's gate discriminates against a real list rather than a remembered one.
Coordinator-sol confirmed this reading on 2026-09-22 at 10:35Z.

Running these fixtures requires Python, because the subject under test is
still Python. That is not a contradiction of the eventual Python-free claim
and must not be recorded as one.

## Source registry

26 files at the pinned commit, each hashed, each assigned one family. The
families exist so nothing falls between the hub and its client — the product
is **not** only a server.

| Family | Files | What a port owns |
|---|---|---|
| `hub-runtime-or-operator-view` | `mailhub/{__init__,app,db,public,serve}.py`, `mailhub/static/index.html` | The network service, its store, the FR-10 public split, the operator UI |
| `chat-client-and-onboarding` | `hubtool.py`, `install-hook.py`, `session-start.sh` | The CLI, the MCP server, the per-session identity store, the SessionStart hook |
| `packaging-configuration-and-exposure` | `Dockerfile`, `compose.yaml`, `.env.example`, `requirements.txt`, `expose-hub.ps1`, `tools/verify-docker.py` | Image, compose topology, exposure, verification tooling |
| `legacy-characterization-tests` | `tests/test_hub.py`, `tests/test_hubtool.py`, `tests/test_hubtool_migration.py` | Existing behaviour records — **evidence of what is, not proof of what is fixed** |
| `normative-or-operational-documentation` | `README.md`, `docs/{OPERATIONS,PROVENANCE,mailserver-spec}.md` | Rulings and operator procedure |
| `repository-metadata` | `.dockerignore`, `.gitattributes`, `.gitignore`, `LICENSE` | — |

The census also freezes 266 function witnesses, 14 registrations (13 explicit
routes + 1 middleware), 4 implicit FastAPI doc routes, 8 MCP tools, 3 JSON-RPC
branches, **11 CLI comparison witnesses representing 9 distinct verbs**, 2
literal DDL scripts covering 8 tables, 73 SQL sites and 10 environment reads.

> The verb count is stated twice on purpose. `addhub` and `drophub` each appear
> in two branches, so a witness count of 11 is not a command count of 9.
> The earlier draft reported 11 as if it were the CLI surface.

## The wire contract

Thirteen explicit routes. `credential_check` says exactly what was measured,
because two different mistakes are easy here.

| Route | Credential check | Refusals | Response keys |
|---|---|---|---|
| `POST /api/register` | **inline-header** | 401, 403, 422 | ok, name, retention_days, roster |
| `POST /api/unregister` | auth-helper | 401 | unregistered |
| `POST /api/poll` | auth-helper | 401 | name, messages, receipts, roster |
| `POST /api/ack` | auth-helper | 401 | acked |
| `POST /api/send` | auth-helper | 401, 422 | id, received_at, duplicate |
| `POST /api/receipts` | auth-helper | 401 | recorded |
| `POST /api/attachments` | auth-helper | 401, 413 | id, bytes |
| `GET /api/attachments/{aid}` | auth-helper | 401, 403, 404, 410 | *(file stream)* |
| `GET /api/roster` | auth-helper | 401 | name, roster |
| `GET /healthz` | **none** | — | ok, name, orgs, queued, retention_days |
| `GET /` | **none** | — | *(HTML)* |
| `GET /ui/data` | **none** | — | name, retention_days, orgs |
| `GET /ui/messages` | **none** | — | messages |

Two readings to avoid:

- `/api/register` is **not** unauthenticated. It does not call the shared
  `_auth` helper because it cannot: the caller is not yet a principal. It
  parses `X-Org-Auth` inline and requires a secret for the very slug being
  registered.
- `/`, `/ui/data`, `/ui/messages` and `/healthz` genuinely have no credential
  check. That is **ruled**, not a defect: on a closed collaborative network,
  hub access *is* read access to everyone's correspondence. It is also the
  entire reason the FR-10 public listener exists, and a port that "fixes" it by
  adding auth changes a decision the operator made.

### The refusal envelope

A status code alone does not pin a refusal. Every explicit refusal detail is
extracted from the pinned AST — so the profile asserts the *source's own text*
rather than a transcription of it — and every refusal fixture asserts both the
status and the body.

The body of a hand-raised refusal is **exactly** `{"detail": <string>}`, nothing
more. Three distinctions a port must reproduce deliberately:

1. **Two different 401 sentences.** `/api/poll` and `/api/unregister` answer
   `no valid org credentials in X-Org-Auth`; every other auth-helper route
   answers `no valid org credentials`. Collapsing them is a silent contract
   change that a status-only profile cannot see.
2. **Framework refusals carry a different shape under the same key.** FastAPI's
   own query validation answers 422 with `detail` as a **list** of structured
   pydantic errors (`type`, `loc`, `msg`, `input`), not a string. Both shapes are
   frozen; a port must choose each one on purpose rather than inherit whatever
   its framework does.
3. **The public listener's 404 is not JSON at all.** `mailhub/public.py:44`
   writes the raw bytes `not found` with no envelope. A port that answers it
   with `{"detail": "not found"}` has changed the public surface.

Five refusals interpolate a per-request value (`no org registered as {}`,
`at most {} attachments`, `unknown attachment {}`, `attachment {} already
bound`, `attachment exceeds {} MB`). For those the profile freezes the constant
runs around the hole, never the whole string.

### Identity, authority and addressing

- `X-Org-Auth: <slug>:<secret> <slug>:<secret> …`, space separated. One call
  may carry several identities; invalid pairs silently drop out of the
  principal set and the call proceeds for the valid ones.
- Slug grammar: `^[a-z0-9][a-z0-9._-]{0,127}$`.
- The hub stores **only** `sha256(secret)`. Verification compares the **full**
  digest with a constant-time compare. The 6-character suffix that rides the
  address is a display label — 24 bits, collidable on a laptop — and must never
  be what a port compares.
- Joining is open; **addresses are owned**. First write wins a slug; a
  different secret for a held slug is `403`.
- Re-registering with the same secret is not an error: it refreshes
  `org_name`/`username`/`blurb` and keeps the address.
- `kind` is `chat` only for the exact string `chat`; anything else is `org`.

### Custody, ordering and duplication

- `received_at` (hub clock, millisecond ISO-8601 `Z`) is the **ordering
  authority**. `sent_at` is the sender's claim and is display only — the
  fixtures send a reversed `sent_at` to prove delivery order ignores it.
- Delivery is **at-least-once**. A message stays `queued` and redelivers on
  every poll until the recipient acks custody. Collapsing duplicates is the
  client's job, not the hub's.
- Only the addressee may ack. A repeated ack counts `0`.
- Poll order is `received_at, rowid`. The operator view reverses it
  (`received_at DESC, rowid DESC`) and rides the rowid out as `n`.
- `n` is the keyset cursor identity. `(before_at, before_n)` pages strictly
  older rows in the same order the page sorts by, so paging stays stable while
  new mail arrives. **A migration that loses rowid ordering loses the cursor.**
- Limits: body truncated at 20,000 characters (truncated and accepted, *not*
  refused); at most 10 attachments per message; 25 MiB per attachment
  (one byte over is `413`); operator page limit clamped to 1…500.

### The receipt ladder

`received` → `fetched` → `delivered` → `read`.

- The `200` from `/api/send` **is** the `received` receipt — hub custody
  confirmed.
- `fetched` comes from the recipient's ack; `delivered` and `read` come from
  the recipient's `/api/receipts`. Neither requires the other.
- Each timestamp column is written only while `NULL`, so the ladder is
  monotonic and a second report records `0`.
- Only the addressee may report. A third party records `0`.
- States other than `delivered`/`read` are **ignored silently** and the call
  still succeeds — not a refusal.
- Receipts are marked pushed **when returned**, before the sender is known to
  have received the response. This is deliberate and loss-tolerant: a dropped
  response costs display state, never a message. Network-response loss and
  custody loss are different failures and must stay different.

### Attachments

Blobs are files under `<HUB_DATA>/blobs/<id>`; the table holds metadata only.
Read rights: the owner always; the addressee once a send **binds** the
attachment to a message; nobody else (`403`). An unknown id is `404`;
a row whose blob is gone is `410`, which is distinct from both.

Upload writes the blob **before** the SQL commit, and the retention sweep
deletes blobs **before** committing the metadata delete. Both orders can leave
an orphan under a fault. MH02/MH03 fixtures must preserve that uncertainty and
identify orphaned content rather than assume native atomicity.

### The public listener (FR-10)

`PublicHub` is a route split, not a tunnel. It admits `/api/*` and `/healthz`;
everything else is `404`. Non-HTTP scopes are refused, and a WebSocket scope is
closed with `1008` — deliberately, so a future live-feed route cannot inherit
public exposure by accident. Tunnelling the full port would publish the
unauthenticated all-mail UI; the fixtures assert the `404`s because this is a
security boundary.

### Retention

Message and attachment retention is `HUB_RETENTION_DAYS` (standalone default
**30**). Roster hygiene is `HUB_ORG_RETENTION_DAYS` (default **45**, and it must
stay longer than message retention so an org outlives its own queued mail). A
silent client is pruned — **except** one still holding queued mail, so a
delivery is never stranded. The sweep runs hourly and takes message row,
attachment row and blob file together.

> The integrated parent host configures different values (loopback, effectively
> permanent retention via 36500 days, 45-day org retention). Standalone and
> integrated defaults are both real and must not be merged into one number.

## Durable state

**Hub store** — SQLite at `<HUB_DATA>/hub.sqlite3`, WAL, `busy_timeout=5000`,
`synchronous=NORMAL`, connection per request. Tables: `meta`, `orgs`,
`messages`, `attachments`. Message `id` is client-minted and is the primary
key; `INSERT OR IGNORE` on it is what makes send idempotent. `rowid` is the
`received_at` tie-breaker and is exported as `n`.

**Chat client store** — SQLite at `~/.orgtree/hub-clients/clients.sqlite3`,
WAL, `busy_timeout=10000`, `synchronous=FULL` (the uid is the only copy of the
secret), `0600` where the OS can express it. Tables: `identities` (**`uid` IS
the secret**), `identity_hubs` (ordered — order decides send resolution),
`seen_ids` (per name **and per hub**; ids are unique only within one hub, so
one hub's ring must never suppress another's mail; trimmed to the newest 200),
`migrations` (the durable record of which pre-SQLite JSON files were imported).

Legacy per-name JSON files are migration **inputs** and are never modified or
deleted — they are the rollback boundary. On conflict the active identity keeps
the address and the conflict is reported rather than resolved.

Secrets stay in their scoped custody. No secret appears in this document, in
the inventory, or in any fixture: the fixture profile declares symbolic
principals and the driver mints credentials at run time.

## Operation, state and protocol families

A file/function/SQL census does not express a queue that lives only in memory, a
file that carries process ownership, or an artifact written outside the blob
root. Those are exactly the families a port drops silently, so each is
dispositioned in `state_families` against an exact source anchor. An anchor that
stops resolving aborts the build rather than quietly listing a family with no
lines.

| Family | Category | Durability | Disposition |
|---|---|---|---|
| `receipt-retry-queue` (`hubtool.py:631`) | queue | memory-only, bounded 200/hub, newest kept | must-be-ported |
| `listener-process-ownership` (`hubtool.py:776`) | process custody | on-disk `.listening` lock | must-be-ported |
| `fetched-attachment-output` (`hubtool.py:1049`) | output artifact | on-disk, **outside** the blob root | must-be-ported |
| `onboarding-settings-mutation` (`install-hook.py:30`) | configuration state | `~/.claude/settings.json` + timestamped backups | out-of-scope-for-the-hub-port |
| `session-admission-environment` (`session-start.sh:18`) | configuration source | per-session decision | must-be-ported |
| `mcp-jsonrpc-envelope` (`hubtool.py:1172`) | protocol envelope | stdio request/response | must-be-ported |

Four of these carry consequences worth stating outright:

1. **The receipt retry queue loses display state, never messages.** A failed
   receipt re-queues per hub and retries on a later cycle; the queue is bounded
   at 200 and discards the *oldest* first; a listener restart drops it entirely.
   Making it durable in the port would be a behaviour change, not an upgrade.
2. **Listener custody is decided by a bare pid.** `_pid_alive` has no start-time
   or identity check, so a reused pid reads as the live holder and refuses the
   real listener. **This blocks conversion** — it is recorded, not solved.
   The lock *is* released, but only as a **best-effort attempt**. `listen()`
   unlinks it in a `finally` (`hubtool.py:866`) that belongs to the main loop's
   `try`, and the unlink is wrapped in `except OSError: pass`. So the lock
   survives three ways, not one: a death that does not unwind (`SIGKILL`, power
   loss, `os._exit`); an unlink that *fails*, whose `OSError` is swallowed; and
   any exit between taking the lock and reaching that `try` — `_hubs(d)`
   raising, or the `hubs0[0]` index on an empty hub list — because the `finally`
   is not armed yet. In each case the next start reads the dead pid and takes
   the lock over. The refusal path for a live holder also returns before the
   `try`, which is *correct* there: a refused second listener must leave the
   real holder's lock alone. Both the acquisition and the release are anchored
   in the register, and a control checks the anchored unlink really is the
   guarded one, so neither the claim nor its strength can drift from the source
   again.
3. **Fetched attachments are sanitized and collision-suffixed.** A port that
   writes the server-supplied `Content-Disposition` name unsanitized introduces
   a path-traversal bug the pinned source does not have. The two fallbacks are
   at different stages and do **not** chain: the attachment id becomes the name
   only when the header carries no parseable filename, and a name that
   sanitizes away becomes `file.bin` *directly* — never the id.
4. **The MCP envelope has no error member at all.** Every `serve()` reply is
   `{jsonrpc, id, result}`; a hub-unreachable or handler error is carried as
   *text inside a success result*, an unknown method with a non-null id gets
   `{}`, and unparseable input is dropped with no reply. A port that "correctly"
   emits `{error: {code, message}}` changes observable behaviour for every
   existing client. **This blocks conversion** until MH02 exercises it against a
   synthetic adapter.

Both blocking entries are listed in `state_families.blocking_unknowns` so they
fail a check rather than depending on someone reading this paragraph.

## Interpreter dependencies

43 witness lines, 16 dispositioned roles, 2 attributed parent claims, and a
fail-closed rule: any interpreter witness in a backend family that no role
accounts for lands in `uncovered_backend_witnesses`, and a non-empty list is an
error. 14 of the 16 roles are marked `must-be-replaced`/`must-be-removed`.

Two are worth naming because the obvious method misses them:

1. **`compose.yaml:38` — the container healthcheck is a second Python launch.**
   `test: ["CMD", "python", "-c", "import urllib.request; …urlopen('/healthz')"]`,
   every 30 seconds, inside the container. Replace the service binary and strip
   Python from the image without replacing this, and every hub reports
   unhealthy.
2. **`mailhub/db.py:79` — schema initialization has no interpreter word on its
   line.** `con.executescript(_SCHEMA)` is where every schema create and
   upgrade happens, including the FR-06 `ALTER TABLE` fallback below it. A text
   search for "python" misses it completely, so it is anchored explicitly.

Deliberately **excluded** from the backend assertion: `tools/verify-docker.py`
is test tooling. The docket rules that Python used by test tooling is not
reclassified as an Orgtree backend dependency, and counting it would send R10
chasing a dependency that is not one.

Carried as **attributed external claims, not verified here** — both live in the
parent repository, which this slice cannot inspect:

| Parent witness | Claim | Owner |
|---|---|---|
| `engine/mailhub_runtime.py:169` | `_migrate_store` runs `sys.executable -c 'import mailhub.db …'` to initialize the schema before import | R09/R10 |
| `engine/mailhub_runtime.py:335` | `start()` launches `sys.executable -m mailhub.serve` | R09/R10 |

Source: rust-program-astra, `mailhub-parent-boundary-20260922.md`, parent commit
`24c03bd4992ff331ca87be3e62f298aaa5c53835`. Replacing the product binary alone
does not close either one.

## Legacy behaviour recorded as-is

These are frozen because they are real, not because they are right. A port must
reproduce them, or change them under a recorded ruling. Neither silently
preserving nor silently fixing them is acceptable.

1. **Same-id replay with a changed payload is accepted as a duplicate.**
   `/api/send` validates sender, recipient and attachments, then does
   `INSERT OR IGNORE` on the client-minted id. A retry carrying a *different*
   body returns `duplicate: true` and the original `received_at`, and the new
   body is discarded without telling the sender. This is **not** the approved v3
   original-key replay contract, which wants a changed payload under a reused
   key to be a conflict. Fixture: `send.replay-of-the-same-id-is-a-duplicate-and-the-first-payload-wins`.
2. **Receipts are marked pushed before the sender has the response.** Display
   state is knowingly loss-tolerant; message custody is not. The two must not be
   conflated in MH03 fault schedules.
3. **A stale comment in the parent's `_migrate_store`** says roster rows are not
   copied while the SQL copies rows with non-empty fingerprints. The executed
   behaviour is what counts; the comment is stale. (Parent-side, attributed.)
4. **Existing suite gaps are documented, not fixed.** `docs/PROVENANCE.md`
   retains `blob_path(empty-normalized-id)` resolving to a directory, and the
   two-consumers-on-one-client-mailbox gap. The presence of a test file is not
   evidence a gap was closed.
5. **Malformed JSON can surface as `500`** while FastAPI's own query validation
   uses `422`. Framework-generated refusals and hand-raised ones are different
   surfaces; a port must decide each deliberately rather than inherit whatever
   its framework does.
6. **Every malformed request body is a 500, not a refusal.** A body that is not
   JSON, a JSON array, a JSON string, a literal `null` and an empty body all
   reach `json.loads`/`.get` unguarded, so the handler raises and the server
   answers **HTTP 500 with the plain-text body `Internal Server Error`**. A
   well-behaved API would refuse 400 or 422. All five are frozen as executable
   cases marked `legacy`. Observing this correctly requires a transport that
   does **not** re-raise handler exceptions into the caller — under the default
   test transport the exception escapes and looks like a traceback rather than a
   response, which is a test artifact and not the public boundary. The profile
   uses the `full-served` surface for exactly these cases.
7. **`mailhub/app.py:521` leaks a file handle on every operator-UI request.**
   `HTMLResponse(open(path, encoding="utf-8").read())` never closes the file;
   under `-W error::ResourceWarning` it surfaces as an unclosed-file warning
   attributed to the product, not to the harness. Recorded, not fixed — MH01
   does not edit product source.

## Unknowns and unexercised surfaces

Recorded so they block conversion rather than being discovered later.

- **MCP and CLI behaviour is enumerated but not executed.** The 8 MCP tools, 3
  JSON-RPC branches and 9 CLI verbs are frozen from source. They are not
  exercised: doing so means registering an identity or arming a listener against
  a real hub, which this slice is forbidden to do. MH02 needs either a synthetic
  adapter or a separate authorization.
- **The listener's reconnect, PID-file custody and multi-hub failover** are read
  but unexercised for the same reason. `_pid_alive` trusts an integer PID file;
  PID reuse is uncharacterized.
- **`/api/roster` requires credentials in this source**, while the parent's
  peer-probe call was observed without them. This mismatch is characterized, not
  resolved; neither side may be silently changed to match the other.
- **Concurrency under real multi-writer load** is not exercised. The fixtures
  drive the app in-process through a single ASGI transport.
- **Platform coverage is Windows-only.** No claim is made for POSIX behaviour,
  including the `0600` chmod, which is explicitly best-effort.
- **No fault injection.** Native process-crash, torn-write and interrupted-import
  behaviour belongs to MH02/MH03. Nothing here is evidence about them.

## How to verify this candidate

From the repository root, with an interpreter that has `fastapi` and `httpx`:

```
python tools/mh01_inventory.py          # census vs pinned source; must print errors: []
python tests/test_mh01_inventory.py     # 32 fail-closed census controls
python tests/mh01_contract.py           # 80 tests: 62 wire cases + 18 controls
```

`tests/mh01_contract.py` runs as **standard unittest**, so the shared harness
(`tools/run-python-verification.py`) reports a real denominator —
`tests_ran: 80` — instead of filing it as a module with no tests. Add
`--narrative` for the grouped human-readable report; both entry points execute
the same functions, so there is one source of truth for pass and fail.

The 18 controls are two registry checks plus two different kinds of control, and
the distinction is the point:

- **2 registry checks** — every registered route and refusal status in the
  frozen registry is exercised by some fixture, and the profile carries exactly
  the cases its manifest declares; and, measured on the test class rather than
  trusted from the install loop, every declared case really is installed as its
  own test.
- **10 against a bad expectation** — a wrong status, wrong response keys, an
  unresolvable placeholder, a deleted case, a stripped case manifest, two case
  ids that differ only in punctuation, the same case id declared twice, a
  dropped route, a dropped refusal, a profile pinned to the wrong commit. These
  prove the *checker* discriminates.
- **6 against a bad implementation** — the product is broken at the public
  boundary and named cases must go red: an always-empty roster, refusals that
  all carry the wrong detail, a roster row missing `last_seen`, a lost chat/org
  `kind` distinction, an operator read that is not capped at 500, and refusals
  that carry the wrong cap, size or echoed identifier. These prove the *profile
  constrains behaviour* rather than shape, and they exist because review found a
  profile that did not: an empty-roster build and a wrong-refusal build both
  passed the 63/63 suite of the first round.

Four of those controls answer a review round specifically, and each closed a
hole that a green run was hiding:

- **The operator read cap was asserted against a one-row store.** `limit=99999`
  checked only that the call returned 200, and `limit=0` checked that one
  message came back — which a hub with *no clamp at all* also satisfies when
  one message exists. The case now builds 501 messages, so 500 and 1 are
  observable numbers rather than accidents.
- **`detail_contains` threw away the contractual values.** Matching `at most`
  and `attachments` accepts `at most 999 attachments`. The five templated
  refusals are now asserted in full, including the `!r` quoting that `app.py`
  puts around an echoed slug or attachment id.
- **Deleting a case produced a smaller green run.** Coverage is satisfied by a
  neighbouring case on the same route and status, so an omission was invisible.
  `case_manifest` pins the set of case ids, and a profile missing one — or
  missing the manifest — now fails.
- **A declared case could be silently skipped.** Test methods are installed with
  `setattr` under a name that collapses every run of non-word characters to `_`,
  so the distinct ids `malformed.json-null-body-is-a-500` and
  `malformed.json_null_body_is_a_500` generate one method and the second
  replaces the first. The manifest could not see it: the raw id sets and the
  count were both correct. A profile declaring 63 obligations executed 62 and
  reported the same 77/77 as the 62-case baseline, with a deliberately failing
  case among the ones that never ran. `registration_errors` now rejects
  duplicate ids and generated-name collisions before the run, and a second
  check counts what the class actually carries.

The first command **checks** the frozen register against the pinned source; it
does not rewrite it. To reproduce the artifact itself, add `--write`:

```
python tools/mh01_inventory.py --write  # regenerate docs/mh01-source-inventory.json
```

On this machine that regeneration is byte-identical to the committed file.
`--repo-root` and `--inventory` make the probe portable to a checkout elsewhere.

One reproducibility limit: the register is written in text mode, so its line
endings follow the platform — CRLF on Windows, LF elsewhere. The committed blob
is LF and `core.autocrlf` reconciles the two, so a regeneration matches the
commit on either platform, but the raw bytes are not platform-independent.
Compare the register ignoring carriage returns if you verify it across
platforms. The census itself is immune: it normalizes CRLF when reading source
and reads pinned Git objects rather than the working tree.

Nothing binds a socket, spawns a process, registers an identity, arms a
listener, or opens live hub data. `HUB_DATA` is a throwaway temporary directory
created per run.

`tests/mh01_contract.py` is one **driver** for `tests/fixtures/mh01-wire.json`,
not the contract itself. Qualifying the Rust product means writing a second
driver against its binary and changing nothing in the fixture file — the
implementation selection sits outside the assertions on purpose.
