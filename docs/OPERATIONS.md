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
| `HUB_BIND` | `0.0.0.0` | **a security control, not a convenience knob** (see Trust model: reachability is authorization, so this binding is the admission boundary): which interface the FULL app binds. Under compose this doubles as the host-side port-mapping interface; outside Docker `mailhub.serve` honors it directly (an embedding desktop process sets `127.0.0.1`) |
| `HUB_PUBLIC_BIND` | `0.0.0.0` | **a security control the same way** — its routes are authenticated, but per the Trust model reaching it is still what admits a new registrant: under compose the host-side interface of the public listener's port mapping, outside Docker honored directly by `mailhub.serve`. Set `127.0.0.1` to keep it loopback-only behind a tunnel or reverse proxy, or a specific interface address to pin it to one network (cross-org find 2026-09-16, neoja: before this existed, an IP written into `HUB_PUBLIC_HOST_PORT` interpolated into a valid mapping by accident — that form now fails `docker compose config` loudly) |
| `HUB_PUBLIC_HOST_PORT` | `7378` | compose only: the host PORT mapped to the public listener's internal 7371. A bare port — the interface comes from `HUB_PUBLIC_BIND` |
| `HUB_CONTAINER_NAME` | `orgtree-mailhub` | compose only: the container's name. Container names are host-global (volumes are compose-project-prefixed, names are not), so a second instance on one host must override it |

Two instances on one host need three distinct things: a separate compose
project (`docker compose -p <name>`, or a second checkout directory — this
alone gives each instance its own volume, because compose prefixes volume
names with the project), a `HUB_CONTAINER_NAME` override, and their own
host ports. Nothing else collides.

## Health and logs

- `GET /healthz` → `{ok, name, orgs, queued, retention_days}`; the compose
  file wires it as the container healthcheck.
- One structured JSON line per request on stdout (`docker logs
  orgtree-mailhub`), plus one line per retention sweep. Slugs are logged,
  secrets never are. Logs are bounded by Docker's own log driver — set
  `max-size`/`max-file` under `logging:` in compose if the default json-file
  driver's growth matters on your host.

## Trust model (read before exposing anything)

Two facts define it, and they are deliberate choices, not oversights. They
are stated plainly here at a cross-org operator's request (find 2026-09-16,
neoja, standing up star-hub) rather than left for each operator to derive:

1. **Registration is open by design: reachability IS authorization.**
   `/api/register` has no allowlist, no invite, no approval step. Anyone
   who can reach a listener can mint an identity — and the register
   response itself returns the full roster, so one open request yields
   every address on the hub. The org-secret authentication protects
   identity OWNERSHIP: nobody can claim an owned slug, send as someone
   else, or read another org's mailbox through the API. Nothing but
   network reachability gates JOINING, and a fresh identity may send to
   every org on the roster.

2. **Mail delivered to an Orgtree organization is acted on by agents.**
   An unwanted registration is therefore not a spam problem but an
   injection problem: whoever can reach a listener can put words in front
   of your agents. Whatever first-contact policy a receiving client
   applies is that client's own defense, outside this hub's control.

Together these make the host binding — `HUB_BIND` and `HUB_PUBLIC_BIND`,
with the network behind them — the entire ADMISSION boundary. They are
security controls, not convenience knobs. Choose them by network, because
that is the decision actually being made: a membership-controlled network
(a tailnet or similar, where every member is someone you would let address
your agents) is what this model contemplates; a general LAN — guest wifi,
a flat office network — is not membership-controlled and does not qualify.
(That last guidance is judgment; the mechanism is only the two facts
above.)

HOW you reach that network has two shapes, and only one of them is
available in a container. Where `tailscaled` owns a REAL HOST INTERFACE,
set `HUB_PUBLIC_BIND` to that specific address — the port then lives on
that interface and no other. Where Tailscale runs with
`--tun=userspace-networking`, which is the common containerized default,
there IS NO host interface to bind: the tailnet address exists only inside
a userspace network stack in a container, `docker compose up` fails with
`cannot assign requested address`, and no `.env` value can fix it. There,
keep BOTH binds on `127.0.0.1` and publish with Tailscale's own proxy —
`tailscale serve --bg --tcp 7378 tcp://127.0.0.1:7378` (tailnet only,
never Funnel). That is the stricter of the two: a host bind puts the port
on an interface, this puts it on none, so every other interface the
machine has now or gains later stays closed, and it is reversible with
`tailscale serve --tcp 7378 off` without restarting the hub.

Both reach the same place. Either way keep the full port — the
unauthenticated all-mail view below — on loopback permanently; that half
is not a choice. (Second cross-org find 2026-09-16, neoja, who reported
their own first instruction as unworkable after the daemon refused the
bind on exactly this shape.)

The full port additionally serves an UNAUTHENTICATED read-only view of
every message at `/`. That is the operator view, ruled deliberately for a
closed network: reaching the full port IS read access to all mail. Never
expose the full port beyond the network you trust. For remote clients over
the open internet, enable `HUB_PUBLIC=1` and expose/tunnel ONLY the public
listener (every route it serves is authenticated with the caller's own org
secret — which, per fact 1, still admits anyone who can reach it; the UI
is not served there). `expose-hub.ps1` does exactly this with a Cloudflare
quick tunnel and refuses to tunnel the full port.

TLS is not built in: on a closed network plain HTTP is the ruled default.
If you want TLS, put a reverse proxy (e.g. Caddy) in front — see README.

## Backup and restore

The whole state is `HUB_DATA`: `hub.sqlite3` (+ WAL sidecars) and `blobs/`.

⚠ **The volume's real name is not the name in `compose.yaml`** (cross-org
find 2026-09-16, neoja): compose prefixes volume names with the project
name — by default the checkout directory — so the volume is typically
`orgtree-mailhub_orgtree-hub-data`, not `orgtree-hub-data`. This matters
because `docker run -v <name>:...` silently CREATES a missing volume: a
backup written against the short name copies a fresh empty volume and
looks successful. Resolve the real name first, and let a wrong name fail
loudly before anything copies:

```
docker volume ls --filter name=orgtree-hub-data     # find the real name
VOL=orgtree-mailhub_orgtree-hub-data
docker volume inspect "$VOL" >/dev/null             # errors if it does not exist
```

- **Backup**: `docker compose stop mailhub`, copy the volume contents
  (`docker run --rm -v "$VOL":/data -v <dest>:/out alpine cp -a
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
