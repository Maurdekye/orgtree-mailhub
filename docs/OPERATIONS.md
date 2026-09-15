# Operating the hub

Everything here describes the standalone deployment (Docker). When the hub is
embedded inside the Orgtree desktop, Orgtree's own settings surface drives the
same knobs and this document is background reading.

## Configuration

All configuration is environment variables (compose reads `.env`; see
`.env.example`):

| variable | default | meaning |
|---|---|---|
| `HUB_NAME` | container hostname | display name; clients discover it on connect, and it titles the hub UI |
| `HUB_PORT` | `7370` | the FULL app: API + the unauthenticated read-only UI |
| `HUB_DATA` | `/data` | data root: `hub.sqlite3` (WAL) + `blobs/` |
| `HUB_RETENTION_DAYS` | `30` | hourly sweep deletes messages and attachment blobs older than this — **regardless of delivery state** |
| `HUB_ORG_RETENTION_DAYS` | `45` | roster rows silent this long are pruned, except rows still holding queued mail; a pruned client re-registers itself on its next 401 |
| `HUB_PUBLIC` | unset | serve the API-only public listener on internal port 7371 (compose maps it to host `HUB_PUBLIC_HOST_PORT`, default 7378) |
| `HUB_BIND` | `0.0.0.0` | which interface the FULL app binds. Under compose this doubles as the host-side port-mapping interface; outside Docker `mailhub.serve` honors it directly (an embedding desktop process sets `127.0.0.1`). The public listener always binds 0.0.0.0 — all its routes are authenticated |

## Health and logs

- `GET /healthz` → `{ok, name, orgs, queued, retention_days}`; the compose
  file wires it as the container healthcheck.
- One structured JSON line per request on stdout (`docker logs
  orgtree-mailhub`), plus one line per retention sweep. Slugs are logged,
  secrets never are. Logs are bounded by Docker's own log driver — set
  `max-size`/`max-file` under `logging:` in compose if the default json-file
  driver's growth matters on your host.

## Trust model (read before exposing anything)

The full port serves an UNAUTHENTICATED read-only view of every message at
`/`. That is the operator view, ruled deliberately for a closed network: hub
access IS read access to all mail. Never expose the full port beyond the
network you trust. For remote clients over the open internet, enable
`HUB_PUBLIC=1` and expose/tunnel ONLY the public listener (every route it
serves is authenticated with the caller's own org secret; the UI is not
served there). `expose-hub.ps1` does exactly this with a Cloudflare quick
tunnel and refuses to tunnel the full port.

TLS is not built in: on a closed network plain HTTP is the ruled default.
If you want TLS, put a reverse proxy (e.g. Caddy) in front — see README.

## Backup and restore

The whole state is `HUB_DATA`: `hub.sqlite3` (+ WAL sidecars) and `blobs/`.

- **Backup**: `docker compose stop mailhub`, copy the volume contents
  (`docker run --rm -v orgtree-hub-data:/data -v <dest>:/out alpine cp -a
  /data /out/`), start again. Online backup is also safe via SQLite's backup
  API if you prefer not to stop; copying the raw files while the hub is
  running is NOT safe (WAL).
- **Restore**: stop the container, restore the copied files into the volume,
  start. Clients need no action: identities live client-side, and
  re-registration is idempotent (same secret → same fingerprint → same
  address).

## Upgrades

The store schema is created with `CREATE TABLE IF NOT EXISTS` plus additive,
idempotent column migrations at connect time — upgrading the image and
restarting is the whole procedure. Downgrading is not supported once a newer
schema wrote new columns; take a backup before upgrading if you may roll
back. Message data is a relay queue: at worst, a recipient re-fetches
anything unacked (delivery is at-least-once by design).

## Verification

- `python tests/test_hub.py` — the hermetic protocol suite (no sockets).
- `python tests/test_hubtool.py` / `tests/test_hubtool_migration.py` — the
  session-client tool and its identity-store migration.
- `python tools/verify-docker.py` — builds an isolated image/container/volume
  (`mailhub-verify*`, loopback ports 7391/7392), drives the real wire
  protocol end to end including a restart-persistence pass, and removes
  everything it created.
