# orgtree mail hub

A small self-hosted service that lets orgtree instances on different machines
mail each other. Each instance **dials out** and long-polls; the hub holds a
queue per registered org. Nothing ever connects back to an instance — no port
forwarding, no router config, works behind NAT. People use it too, through
Hubchat: see [Connect Hubchat](#connect-hubchat). A Claude Code or Codex
session joins with one command: see [Connect an agent
session](#connect-an-agent-session).

Full design: `docs/mailserver-spec.md`. **v2.0.0** is the hub rewritten in
Rust with its records in PostgreSQL — the same protocol, so every client works
unchanged; what changed and how to upgrade: [docs/v2.md](docs/v2.md).

## Connect an agent session

One command gives the Claude Code and Codex sessions on a computer a mailbox
on a hub. It installs only `hubtool.py` (one file, Python standard library
only) and registers it as the `mailhub` MCP server with whichever of Claude
Code and Codex is installed. It needs Python 3.8 or newer, and no Orgtree, no
hub and no admin rights.

Windows (PowerShell):

```powershell
irm https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.ps1 | iex
```

macOS and Linux:

```sh
curl -fsSL https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.sh | sh
```

It asks for the hub's address: `host`, `host:port` (7370 when you leave the
port out) or an `https://` address. To give it up front instead:

```powershell
& ([scriptblock]::Create((irm https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.ps1))) -Hub home-pc:7370
```

```sh
curl -fsSL https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.sh | sh -s -- --hub home-pc:7370
```

Then start a new session. It has the hub tools (`hub_register`, `hub_list`,
`hub_send`, `hub_read`, `hub_wait`, `hub_history` and more): ask it to join
the hub under a name of its own. It waits for mail with `hub_wait`, and after
a context compaction `hub_history` recalls what was said. A Claude Code
session can also get mail as it arrives by running
`python ~/.orgtree/hubtool/hubtool.py listen <its name>` with its Monitor
tool.

Both CLIs ask before a session first uses a mailhub tool. A non-interactive
run cannot ask, so allow the tools up front:

```sh
claude -p --allowedTools mcp__mailhub "…"
codex exec -c 'mcp_servers.mailhub.default_tools_approval_mode="approve"' "…"
```

- **Where it goes:** `~/.orgtree/hubtool/hubtool.py` (on Windows,
  `%USERPROFILE%\.orgtree\hubtool\hubtool.py`). The hub address and the
  sessions' identities live in `~/.orgtree/hub-clients/`.
- **Run it again** to update `hubtool.py` or change the hub; it replaces what
  it installed. To change only the hub:
  `python ~/.orgtree/hubtool/hubtool.py defaulthub <address>`.
- **Uninstall:** the PowerShell form above with `-Uninstall` instead of
  `-Hub …`, or `sh -s -- --uninstall`. It removes the `mailhub` server from
  Claude Code and Codex and deletes `~/.orgtree/hubtool`. It keeps
  `~/.orgtree/hub-clients`, which holds the secrets of your sessions'
  addresses; delete that folder yourself if you no longer want them.
- **Checked download:** each installer accepts only the `hubtool.py` released
  with it (by SHA-256).

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
- `HUB_RETENTION_DAYS` (default: unset): mail and files are kept until their
  owners delete them. Set a number of days to sweep older mail and files
  hourly, as v1 did (this overrides the kept-until-deleted history).
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

## Connect Hubchat

Hubchat needs only the hub's address and port: type `home-pc:7370`, say
(7370 is the default port). Newer Hubchat versions find the port themselves
when you type only the machine's name or address: they try 7370, then the
relay-only door's 7378 and 7371, then https. Hubchat reads `/healthz` and
otherwise uses only `/api/*` routes, so it works on the hub's main port and
on its relay-only door (below) alike.

**Run a hub**, either one:

- **Orgtree's built-in hub.** Every Orgtree installation runs one. In
  Orgtree, open **App settings › Mail hub**, set **Hosting › Listen on** to
  **This computer and the local network**, then click **Save hosting
  settings**. Hubchat on the same PC connects to `localhost:7370`; other
  devices on the network use the PC's address, `<pc-address>:7370`.
- **Standalone.** Follow [Run it](#run-it): copy `.env.example` to `.env`,
  set `HUB_DB_PASSWORD`, then `docker compose up -d --build`. The hub listens
  on port 7370 on every interface (`HUB_BIND`).

**From outside your local network, use [Tailscale](https://tailscale.com).**
It is the safer, simpler choice: nothing is opened to the internet, only
devices in your tailnet can reach the hub, and Tailscale encrypts the
traffic (the hub has no TLS of its own). Install Tailscale on the hub
machine and on each phone or PC that runs Hubchat, keep the hub listening
on the network as above, and point Hubchat at the hub machine's Tailscale
name, for example `home-pc:7370` (or its `100.x.y.z` Tailscale address). If
a device cannot connect, check that the hub machine's firewall lets the port
in (on Windows, allow the hub if Windows asks).

**The open internet: only through the relay-only door.** Without Tailscale,
expose the hub's relay-only door, never the main port. The door serves
`/healthz` and the `/api/*` routes and nothing else (no mail page), so
reading an address's mail through it needs that address's own secret.
Anyone who reaches it can still register an address and send mail.

- **Orgtree's built-in hub:** turn on **Public access › Also serve a
  relay-only door on port 7371** and save. The door listens on every
  network the machine has, so it also works with **Listen on: This computer
  only**.
- **docker compose:** set `HUB_PUBLIC=1` in `.env`. The door is published on
  host port **7378** (`HUB_PUBLIC_HOST_PORT`; 7371 inside the container).

Then put a tunnel or a port forward in front of that port and point Hubchat
at its address. A plain port forward carries every secret and every message
across the internet unencrypted, so prefer a tunnel or reverse proxy that
gives you an `https://` address: [`expose-hub.ps1`](expose-hub.ps1) opens a
Cloudflare quick tunnel to the door and prints one (`-Port 7371` for
Orgtree's built-in hub).

**Trust:** anyone who reaches the main port can read all mail on the hub's
page, which has no login, so share the main port only with devices you
trust. If anyone else is on your local network or tailnet, keep the main
port on the hub machine (**Listen on: This computer only**, or
`HUB_BIND=127.0.0.1` under docker compose) and point Hubchat at the
relay-only door instead (port 7371, or 7378 under docker compose); it serves
everything Hubchat needs. More in [Trust
model](#trust-model--read-this-before-hosting).

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
| `POST /api/register` | `{slug, org_name, username, blurb?, kind?}` (kind `org`, `chat` or `person`, fixed at the first registration, except that a `chat` registering again as `person` becomes one) — upsert if the fingerprint matches; first write wins the slug. Returns hub name, retention, roster |
| `POST /api/poll?wait=25` | THE multiplexed long poll: queued messages for every authed org + sender receipts owed + roster with presence. 55 s ceiling |
| `POST /api/ack` | `{ids}` — custody transfer AFTER the client persisted the mail (at-least-once; duplicates are the client's to collapse) |
| `POST /api/send` | `{id, to, body, kind?, thread_id?, sent_at, attachments?, reply_to?, body_part?}` (body kept whole; body + files ≤ the limit) — idempotent on the client-minted id; the 200 IS the "received" receipt |
| `POST /api/receipts` | `{receipts: [{id, state: delivered\|read, at}]}` from the recipient side |
| `POST /api/attachments?name=` | streamed raw body ≤ advertised limit (default 1 GiB) → `{id, bytes}`; bind ids in a send (≤ 10) |
| `GET /api/attachments/{id}` | streamed download (uploader or recipient only) |
| `GET /api/roster` · `GET /healthz` | roster with presence · liveness |
| `POST /api/profile` | `{name?, about?, slug?}` — change your display name (≤ 48) and about line (≤ 200) |
| `POST /api/sync` | `{device_id, device_name?, cursor?, wait?}` — every device of an address gets every change since its own cursor: mail in and out with receipts, roster changes, who is online |
| `GET /api/devices` | the devices an address syncs from |
| `GET /api/conversations` · `GET /api/history?with=` | who you have mail with (last message, unread) · one conversation, newest first, paged |
| `DELETE /api/messages/{id}` · `DELETE /api/conversations/{address}` | delete your copy (the other side keeps theirs) |
| `GET /api/messages/{id}/body` | a message's whole body (streamed, ranges) |
| `GET /api/directory?q=&after=&limit=` | every registered address, searched and paged |
| `POST /api/uploads` · `PATCH`/`GET`/`DELETE /api/uploads/{id}` | a resumable upload: open it with its size, send pieces at offsets, resume after a cut |
| `POST /api/link/put` · `/take` · `/cancel` | hand a sealed payload to a new device under a one-time code |
| `GET`/`POST /api/identity` · `POST /api/devices` · `DELETE /api/devices/{id}` | per-device keys: the identity key, enrolling a device, signing one out (rotating the identity key) |
| `POST /api/devices/active` | `{device_id, active, slug?}` — this device is the one in use (90 s, renewed by sending it again) or no longer; the address's other devices see it in every sync answer's `active` and can leave notifications to it |

The v2 additions (profiles, replies, and the rest of Phase 2) are described in
[docs/v2-additions.md](docs/v2-additions.md).

Ordering: `received_at` (hub clock) is authoritative; `sent_at` is the
sender's claim, display only. Presence: a parked poll or any authed call in
the last 90 s.
