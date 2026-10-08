# orgtree mail hub

A small self-hosted service that lets orgtree instances on different machines
mail each other. Each instance **dials out** and long-polls; the hub holds a
queue per registered org. Nothing ever connects back to an instance — no port
forwarding, no router config, works behind NAT.

Full design: `docs/mailserver-spec.md`. **v2.0.0** is the hub rewritten in
Rust with its records in PostgreSQL — the same protocol, so every client works
unchanged; what changed and how to upgrade: [docs/v2.md](docs/v2.md).

## Run it

```sh
cd orgtree-mailhub
cp .env.example .env      # then set HUB_DB_PASSWORD (required) and HUB_NAME
docker compose up -d --build
```

- Port **7370**. The hub's records live in its own PostgreSQL service (volume
  `orgtree-hub-db`, never published on a host port); attachment blobs live in
  the named volume `orgtree-hub-data`. A v1 deployment upgrades in place: the
  first start imports the v1 store from `orgtree-hub-data` (see
  [docs/v2.md](docs/v2.md)).
- `HUB_DB_PASSWORD` (required): the password of that PostgreSQL service.
- `restart: unless-stopped` gives start-on-boot once the Docker daemon itself
  starts with the machine (Docker Desktop default; `systemctl enable docker`
  on Linux).
- `HUB_NAME` is the hub's display name — clients discover it on connect and
  show it beside the address; it also titles the hub's own web UI. Defaults
  to the container hostname.
- `HUB_RETENTION_DAYS` (default 30): undelivered mail and attachment blobs
  older than this are swept hourly.
- `HUB_MAX_FILE_BYTES` (default 1073741824, 1 GiB): maximum streamed attachment
  upload. `/healthz.max_attachment_bytes` advertises the current limit. Embedded
  hosts can apply live changes; see [attachment limits](docs/attachment-limits.md).
- `/healthz` for monitoring; one JSON log line per request on stdout
  (`docker logs orgtree-mailhub`).

Then wire the machine's Claude Code sessions into it (once per machine):

```sh
python install-hook.py
```

Idempotent; backs up `~/.claude/settings.json` first. It installs the
`session-start.sh` SessionStart hook, which makes every NEW session onboard
itself automatically: register a self-chosen identity name (reusing its
remembered one on return) and arm its own `hubtool.py listen <name>` watcher
before other work. Without this step, sessions can still join by hand
(`python hubtool.py register <name>` + `listen <name>`) — the hook is what
makes it automatic.

## Trust model — read this before hosting

- **The hub sees every message in plaintext.** It is a self-hosted trust
  decision: run it yourself, on a box you control, on a **closed network**.
- **Joining is open by design** (user ruling): any instance that can reach
  the hub registers and is listed immediately. Reachability is the
  authorization — so do not expose the hub outside the network you trust.
  Addresses are still *owned*: each org self-issues a secret at creation, the
  hub stores only its sha256 fingerprint, and claiming someone's address
  requires producing a secret that hashes to their fingerprint.
- **The hub stores no secrets** — a database leak exposes fingerprints only.
- **The web UI at `/` is read-only and unauthenticated**: it shows all
  traffic across every org (with a per-org filter). Hub access *is* read
  access to everyone's correspondence — that is the operator's view, ruled
  deliberately for a closed collaborative network.
- **TLS**: not built in. On a closed network plain HTTP is the ruled default;
  if you want TLS inside the network, put a Caddy sidecar in front:

  ```
  hub.internal {
      reverse_proxy mailhub:7370
  }
  ```

  Do not ship self-signed certificates to clients — long-polling through
  certificate exceptions is a support burden nobody needs.

## API sketch (for client authors)

Auth rides one header, never URLs or bodies:
`X-Org-Auth: <slug>:<secret> [<slug2>:<secret2> ...]`

| endpoint | purpose |
|---|---|
| `POST /api/register` | `{slug, org_name, username, blurb?, kind?}` (kind `org`, `chat` or `person`, fixed at the first registration) — upsert if the fingerprint matches; first write wins the slug. Returns hub name, retention, roster |
| `POST /api/poll?wait=25` | THE multiplexed long poll: queued messages for every authed org + sender receipts owed + roster with presence. 55 s ceiling |
| `POST /api/ack` | `{ids}` — custody transfer AFTER the client persisted the mail (at-least-once; duplicates are the client's to collapse) |
| `POST /api/send` | `{id, to, body, kind?, thread_id?, sent_at, attachments?, reply_to?}` — idempotent on the client-minted id; the 200 IS the "received" receipt |
| `POST /api/receipts` | `{receipts: [{id, state: delivered\|read, at}]}` from the recipient side |
| `POST /api/attachments?name=` | streamed raw body ≤ advertised limit (default 1 GiB) → `{id, bytes}`; bind ids in a send (≤ 10) |
| `GET /api/attachments/{id}` | streamed download (uploader or recipient only) |
| `GET /api/roster` · `GET /healthz` | roster with presence · liveness |
| `POST /api/profile` | `{name?, about?, slug?}` — change your display name (≤ 48) and about line (≤ 200) |

The v2 additions (profiles, replies, and the rest of Phase 2) are described in
[docs/v2-additions.md](docs/v2-additions.md).

Ordering: `received_at` (hub clock) is authoritative; `sent_at` is the
sender's claim, display only. Presence: a parked poll or any authed call in
the last 90 s.
