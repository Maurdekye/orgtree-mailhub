# Operating the hub

Everything here describes the standalone deployment (Docker). When the hub is
embedded inside the Orgtree desktop, Orgtree's own settings surface drives the
same knobs and this document is background reading.

## Developer team deployment

The recommended developer team setup is a **shared standalone mailhub on a
company-accessible private server**, with **each developer installing Orgtree
and Hubchat and connecting both applications to that same hub**. Orgtree hosts
their local AI organizations; Hubchat handles human hub mail; the shared
standalone mailhub relays messages and files. The built-in Orgtree mailhub is
for individuals and small teams. This recommendation changes no defaults and
sets no numerical team-size cutoff. [Documentation decision: user 2026-10-12]

Follow [Run it](../README.md#run-it) for Docker Compose startup. Before starting,
set the required `HUB_DB_PASSWORD`, enable the relay-only listener with
`HUB_PUBLIC=1`, and keep the full operator interface local with
`HUB_BIND=127.0.0.1`. Select `HUB_PUBLIC_BIND` or a private proxy for the company's
network as described under [Trust model](#trust-model-read-before-exposing-anything).
Give both clients the relay-only address, for example
`http://mailhub.company.internal:7378` for the default Compose host mapping.
"Public" in the listener setting does not mean the server needs to be exposed
to the internet. The company operator controls network admission and can read
the mail stored here; back up both volumes as described below.

See the [README's connection steps](../README.md#recommended-developer-team-setup).
People and Orgtree organizations keep distinct identities and secrets.
Independent agent-session hubtool setup is optional, not required for the
two developer applications.

## Configuration

All configuration is environment variables (compose reads `.env`; see
`.env.example`):

| variable | default | meaning |
|---|---|---|
| `HUB_NAME` | container hostname | display name; clients discover it on connect, and it titles the hub UI |
| `HUB_PORT` | `7370` | the FULL app: API + the unauthenticated read-only UI |
| `HUB_DATA` | `/data` | data root: `blobs/` (attachment bytes); a v1 `hub.sqlite3` found here is imported at the first v2 start and kept |
| `HUB_DATABASE_URL` | — (required, v2) | the hub's PostgreSQL (URL or key=value); tables in schema `mailhub`. Compose sets it to its own `postgres` service |
| `HUB_DATABASE_PASSWORD` | — | the database password apart from the URL (compose passes `HUB_DB_PASSWORD` here) |
| `HUB_DB_PASSWORD` | — (required by compose) | compose only: the password the `postgres` service is created with and the hub connects with |
| `HUB_DB_POOL` | `32` | database connections the hub may hold |
| `HUB_IMPORT_SQLITE` | on | import `HUB_DATA/hub.sqlite3` into an EMPTY database at startup (once) |
| `HUB_LOG_VERBOSE` | off | `1`: every handler call/return and SQL statement on stderr |
| `HUB_PUBLIC_PORT` | `7371` | the public listener's port inside the container/host process (v1 fixed it) |
| `HUB_RETENTION_DAYS` | unset | unset or empty: mail and files are kept until their owners delete them (`retention_days: null`). A number: the hourly sweep deletes messages and attachment blobs older than this — **regardless of delivery state, and overriding the kept history** (v1's default was 30). Uploads no send ever bound go after 7 days either way |
| `HUB_ORG_RETENTION_DAYS` | unset | unset or empty: an address stays on the roster until it unregisters or the operator removes it (`orgtree-mailhub remove-address SLUG...`). A number: roster rows silent this long are pruned, except rows still holding queued mail (v1's default was 45); a pruned client re-registers itself on its next 401 |
| `HUB_TRANSLATE_KEY_FILE` | unset | optional translation (v2.4.0): a file whose first line is the Anthropic API key the hub translates with; read at each call, never logged. Turn translation on in the runtime file's `translation` object. See [Translation](#translation) |
| `HUB_PUSH_ALLOW` | empty | optional UnifiedPush (v2.1.0): private distributor hosts or CIDRs the hub may send wakes to, comma-separated exact entries (e.g. `ntfy.example.ts.net,100.64.5.6/32`); public HTTPS endpoints need no entry. Read on startup. See [UnifiedPush delivery](#unifiedpush-delivery) |
| `HUB_WEB_ORIGINS` | empty | Hubchat Web (v2.5.0): the web pages a browser lets call this hub, as origins separated by commas (`https://chat.example.com,https://pc.example.ts.net`), or `*` for any page (only for a hub that is public anyway). Empty: none, and the hub answers exactly as before. The runtime file's `web_origins` array wins over it and changes without a restart. Never applies to the operator page. See [Web apps](#web-apps-hubchat-web) |
| `HUB_PUBLIC` | unset | serve the API-only public listener on internal port 7371 (compose maps it to host `HUB_PUBLIC_HOST_PORT`, default 7378) |
| `HUB_BIND` | `0.0.0.0` | **a security control, not a convenience knob** (see Trust model: reachability is authorization, so this binding is the admission boundary): which interface the FULL app binds. Under compose this doubles as the host-side port-mapping interface; outside Docker the hub honors it directly (an embedding desktop process sets `127.0.0.1`) |
| `HUB_PUBLIC_BIND` | `0.0.0.0` | **a security control the same way** — its routes are authenticated, but per the Trust model reaching it is still what admits a new registrant: under compose the host-side interface of the public listener's port mapping, outside Docker honored directly by the hub. Set `127.0.0.1` to keep it loopback-only behind a tunnel or reverse proxy, or a specific interface address to pin it to one network (cross-org find 2026-09-16, neoja: before this existed, an IP written into `HUB_PUBLIC_HOST_PORT` interpolated into a valid mapping by accident — that form now fails `docker compose config` loudly) |
| `HUB_PUBLIC_HOST_PORT` | `7378` | compose only: the host PORT mapped to the public listener's internal 7371. A bare port — the interface comes from `HUB_PUBLIC_BIND` |
| `HUB_PUBLIC_ADVERTISE` | unset | where clients reach the public listener from outside, when that is not where it listens: under compose this host's address and `HUB_PUBLIC_HOST_PORT` (e.g. `100.64.1.2:7378`), or a tunnel's URL. `/healthz` on the main port shows it as `door.advertise`, so Hubchat can put it in a phone's setup code (v2.0.2) |
| `HUB_STREAM_MAX` | `1000` | WebSocket streams (`GET /api/stream`, v2.5.0) that may be open at once. `0` turns the stream off: `/healthz` stops listing `stream` and clients long-poll as before. Environment only: a change needs a restart. See [The WebSocket stream](#the-websocket-stream) |
| `HUB_WEB_ORIGINS` | unset | web apps allowed to reach the hub from a browser: comma-separated exact origins (`https://chat.example.com`), or `*`; the runtime file's `web_origins` list wins. A WebSocket handshake that carries an `Origin` header is allowed only for a listed origin (the literal `null` never); native clients send none |
| `HUB_CONTAINER_NAME` | `orgtree-mailhub` | compose only: the container's name. Container names are host-global (volumes are compose-project-prefixed, names are not), so a second instance on one host must override it |

Two instances on one host need three distinct things: a separate compose
project (`docker compose -p <name>`, or a second checkout directory — this
alone gives each instance its own volume, because compose prefixes volume
names with the project), a `HUB_CONTAINER_NAME` override, and their own
host ports. Nothing else collides.

## Health and logs

- `HUB_DATA/uploads/` holds resumable uploads in progress (a day untouched
  and they are swept); complete, they move to `HUB_DATA/blobs/`.
- `GET /healthz` → `{ok, name, orgs, queued, retention_days,
  max_attachment_bytes, version, features, now}`, plus `door` (where the
  relay-only door listens) on the main port when the door runs; the compose
  file wires `orgtree-mailhub healthcheck` (which asks it) as the container
  healthcheck.
- Presence (v2.0.2): a client that hangs up on a parked poll or sync (it
  stopped, was killed or crashed) shows offline after 10 s; one lost
  without closing its connection, after its wait and the 90 s window
  (docs/v2-additions.md, "Presence").
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

v2: the state is TWO volumes — the PostgreSQL volume (`orgtree-hub-db`: the
records) and `HUB_DATA` (`orgtree-hub-data`: the attachment blobs, plus a v1
`hub.sqlite3` kept from an upgrade). Back up both while the hub is stopped,
so they agree; `docker exec orgtree-mailhub-db pg_dump -U mailhub mailhub`
gives a portable dump of the records, and the volume copy below works for
either volume. (v1's state was `hub.sqlite3` + WAL sidecars and `blobs/`.)

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

v1 → v2: set `HUB_DB_PASSWORD` in `.env` (compose refuses to start without
it), then rebuild and restart as always. The first start imports the v1 store
from `orgtree-hub-data` into the empty database in one transaction, writes
`v2-import-report.json` beside it, and never imports again; the SQLite file
is kept. Rolling back to v1 means restarting the v1 image on the same
`orgtree-hub-data` volume: it finds its own store as it left it (mail that
arrived on v2 meanwhile is not in it). Details: `docs/v2.md`.

Within v2, the schema is versioned in the database (`hub_meta`) and migrated
at startup, so upgrading the image and restarting is the whole procedure; a
database written by a newer hub is refused rather than downgraded.

⚠ **2.1.0 is a one-way step: back up first.** Its first start adds schema 9
(UnifiedPush subscriptions), and so does the first start of any later
version on a 2.0.x database. From then on a 2.0.x hub refuses the database
("the database schema is version 9, newer than this hub understands"), so
going back to 2.0.x means restoring the backup you took before the upgrade,
losing what arrived since. Take that backup (both volumes, see Backup and
restore) before you start 2.1.0 or later for the first time. 2.1.1 keeps
schema 9: between 2.1.0 and 2.1.1, replacing the image is the whole step,
in either direction.

**Local 2.2.0-alpha.0 adds schema 10 (message metadata).** Its first start on
a schema-9 database applies the already-implemented migration 10. A 2.1.x
hub then refuses that database as newer than it understands; rolling back
requires restoring a backup from before the migration and loses later mail.
Use the same two-volume backup procedure above before testing an upgrade.
No automatic backup or downgrade is added. Profile pictures use existing
`hub_meta` storage. This local alpha retains the 2.1.1 upload-recovery code;
version composition itself changes no runtime or migration SQL. No live
upgrade or rollback has been performed by this packaging preparation.

⚠ **Mail hub 2.3.0 adds schemas 11 (a person's status) and 12 (Invisible),
another one-way step.** Its first start on an older database adds
`identities.status` and `identities.status_since` and lets the status be
`invisible`. From then on a hub without schema 12 (2.2.x, and builds of
2.3.0 from before it, which had busy presence or a person's status only)
refuses the database ("the database schema is version 12, newer than this
hub understands"): going back means restoring a backup taken before the
upgrade, losing what arrived since. Take the two-volume backup above before
its first start.

⚠ **Mail hub 2.4.0 adds schema 13 (translation), another one-way step.** Its
first start on an older database adds `identities.language`, the tables
`translate_jobs`, `message_translations` and `translation_spend`. From then
on a hub without schema 13 (2.3.x and older) refuses the database ("the
database schema is version 13, newer than this hub understands"): going back
means restoring a backup taken before the upgrade, losing what arrived since.
Take the two-volume backup above before its first start.

v1: the store schema is created with `CREATE TABLE IF NOT EXISTS` plus additive,
idempotent column migrations at connect time — upgrading the image and
restarting is the whole procedure. Downgrading is not supported once a newer
schema wrote new columns; take a backup before upgrading if you may roll
back. Message data is a relay queue: at worst, a recipient re-fetches
anything unacked (delivery is at-least-once by design).

## Verification

v2 (see `docs/v2.md` for what each proves; all on throwaway databases):

- `cargo test --test hub_suite` — v1's protocol suite, ported check for check.
- `cargo test --test concurrency` / `--test v2_behaviour` — races and v2's
  deliberate differences.
- `cargo test --test differential -- --ignored` — v1 and v2 side by side,
  a v1 store imported, and `hubtool.py` against both.
- `python tools/verify-docker.py [--upgrade]` — the image, its PostgreSQL
  container and the in-place upgrade from a v1 volume, end to end.

v1's Python server left the tree in v2.0.0: its protocol suite lives on
as `hub/tests/hub_suite.rs`, and the tests that still run v1 (the
side-by-side comparisons, hubtool's suite) take it from git history
(`tests/v1_reference.py`, commit 79a7c51; `pip install -r
tests/requirements.txt`).

- `python tests/test_hubtool.py` / `tests/test_hubtool_migration.py` — the
  session-client tool and its identity-store migration.
- `python tests/test_install_hubtool.py` — the one-line installers without
  running them (the checksum they name, both scripts parse, the README's
  URLs). `python tests/install_hubtool_e2e.py --hub <scratch hub>
  [--real-sessions]` runs them for real in a throwaway home, and with
  `--real-sessions` drives a real Claude Code and Codex session through the
  hub tools, allowed only by the pre-approval the installer wrote, beside a
  control session without it that the CLI refuses (it spends model usage);
  `--stub-clients` where neither CLI is installed.

## Releases

Every release, from 2.5.0 on, carries the one-line installer's assets and the
official container image. The recipe, once the release commit is tagged
`v<version>` and `hub/Cargo.toml` says `<version>`:

```sh
python tools/hubtool-assets.py dist v<version>          # hubtool.py, both installers, SHA256SUMS
gh release create v<version> --draft --title "Mail hub <version>" --notes-file <notes> dist/*
# review the draft, then publish it (with the user's OK):
gh release edit v<version> --draft=false [--prerelease]
```

Publishing starts `.github/workflows/release-image.yml` (nothing runs for a
draft). It checks that the tag is `hub/Cargo.toml`'s version, builds the tag's
commit with the `Dockerfile` for linux/amd64 and linux/arm64, each on its own
native GitHub runner (`build-image.yml`), and smokes each image
(`.github/ci/image/smoke.sh`: a fresh PostgreSQL 18 and volume, healthy,
`/healthz` ok with the version, `docker stop` exits 0). Then it:

- attaches `orgtree-mailhub-<v>-docker-amd64.tar.gz`, `-arm64.tar.gz` and
  `build-info-docker.json` to the release and appends their lines to
  `SHA256SUMS` (the hubtool lines stay as they are; a re-run replaces only its own
  lines);
- pushes `ghcr.io/maurdekye/orgtree-mailhub:<v>-amd64` and `:<v>-arm64`, the
  two-platform `:<v>`, and `:latest` for a stable release only (never for a
  prerelease), with the run's own `GITHUB_TOKEN` (`packages: write`; no other
  secret or account);
- verifies, on each platform's runner (`.github/ci/image/verify-published.sh`):
  pulls `:<v>` anonymously, checks that `:<v>` (and `:latest`) carry the
  release's image by the config digest in `build-info-docker.json`, and runs the
  smoke on the pulled image. Its summary says whether the anonymous pull worked.

Watch it with `gh run list -R Maurdekye/orgtree-mailhub -w release-image`. To
re-run it, or to add the image to a published release that lacks it:
`gh workflow run release-image.yml -R Maurdekye/orgtree-mailhub -f tag=v<version>`.
A push to the branch `ci-test/mailhub-image` runs `build-image.yml` alone (build
and smoke, both platforms), then its `publish-test` job runs the release's publish
logic (`.github/ci/image/publish.sh`) on throwaway tags `ci-test-<sha7>-amd64`,
`-arm64` and `-index` (no real tag is touched) and checks that the index is an OCI
index (`application/vnd.oci.image.index.v1+json`) with a description annotation.
The release pushes OCI images and that OCI index (a Docker manifest list cannot carry
annotations, so GHCR showed no package description); `ci-test-*` tags are throwaway.

**The GHCR package's visibility.** GitHub creates a package private on its first
push, and a workflow token cannot change that. After the first push (2.5.0) the
owner sets it public once: the repository's Packages, `orgtree-mailhub`, Package
settings, Change visibility, Public. The image's `org.opencontainers.image.source`
label links the package to this repository, so later pushes with the run's token
keep working. Until then the verify job warns and pulls with its token.

Users' side: [container-image.md](container-image.md), including switching from
a locally built hub.

### The one-line installer's assets

The README's "Connect an agent session" commands download
`install-hubtool.ps1` / `install-hubtool.sh` from the LATEST GitHub release,
and those download `hubtool.py` from the same place and accept it only by
the SHA-256 written into them. So every release must carry all three, or
the one-line commands fail until the next release that does:

```sh
python tools/hubtool-assets.py --check        # the installers name hubtool.py
python tools/hubtool-assets.py dist <tag>     # the assets as committed at <tag>
gh release upload <tag> dist/*                # hubtool.py, both installers, SHA256SUMS
```

Upload them before publishing (the recipe above does it at `gh release create`):
`release-image.yml` appends its lines to the `SHA256SUMS` it finds, and a later
upload of the hubtool `SHA256SUMS` would drop them (`gh release upload` refuses to
overwrite unless `--clobber`; if you had to, re-run `release-image.yml`).

After changing `hubtool.py`, run `python tools/hubtool-assets.py --stamp`
and commit the two installers with it (`tests/test_install_hubtool.py`
fails until you do).
- `python tools/verify-docker.py` — builds an isolated image/container/volume
  (`mailhub-verify*`, loopback ports 7391/7392), drives the real wire
  protocol end to end including a restart-persistence pass, and removes
  everything it created.

## UnifiedPush delivery

From 2.1.0. Clients opt in using the `unifiedpush` API feature; see
[v2-additions.md](v2-additions.md#optional-unifiedpush-feature-unifiedpush).
Until a device registers for push, the hub makes no outbound requests.
No operator relay, central server or API key is needed. Allow outbound HTTPS to
the chosen distributor. For private self-hosted endpoints, set `HUB_PUSH_ALLOW`
to comma-separated exact hosts or CIDRs (empty by default), such as
`ntfy.example.ts.net,100.64.5.6/32`. Allowed endpoints still require valid HTTPS;
DNS results are pinned, redirects are refused, and timeouts remain in force.
TLS trusts bundled Mozilla roots, not custom CAs installed in the OS; a
private-CA ntfy server is not supported. NAT64 prefixes require an explicit
allowlist. Publicly routed home IPv6 addresses remain reachable, subject to TLS
verification. Endpoint and Web Push
auth values are secrets in PostgreSQL; protect backups as you protect hub data.
They are absent from API listings and logs.

For an isolated local HTTP test receiver only, compile debug tests with
`--features push-test`. That feature allows literal HTTP loopback endpoints and
loopback resolution; release builds explicitly refuse to compile it. Never use
that debug test binary to host a real hub. Regular builds allow public HTTPS and
explicitly allowed private HTTPS. No proxy environment variable overrides the
outbound destination policy.

## Translation

Optional, off until the owner turns it on (v2.4.0; docs/v2-additions.md has
the protocol). The hub then sends the text of mail between people who read
in different languages to Anthropic's Messages API: **message text leaves
the hub**. Turn it on only where the people on it agree to that. Only mail
from one person to another is translated, unless `translate_agents` is true
(then agents' and orgs' mail to people too); mail to agents never is.

1. Put the Anthropic API key, alone on the first line, in a file only the
   hub's account can read, and point `HUB_TRANSLATE_KEY_FILE` at it.
2. In the runtime file (`HUB_RUNTIME_CONFIG_FILE`) add
   `"translation": {"enabled": true, "default_language": "en", "monthly_limit_usd": 5}`
   (optional `translate_agents`, `model`, `input_usd_per_mtok`,
   `output_usd_per_mtok`). The hub
   reads it at each use: no restart.
3. `/healthz` on the main port shows `translation.state` (`ok`, `no_key`,
   `key_refused`) and the month's `spent_usd`.

Past the month's limit, mail is delivered untranslated and its readers are
told why. Orgtree's App settings > Mail hub > Hosting writes both files for
the bundled hub.

## Web apps (Hubchat Web)

Hubchat Web is Hubchat in a browser, served by a container of its own. Its
server only hands out the app: the browser itself calls this hub. Two
things have to be true for that to work.

**1. The hub lists the web app's address.** Set `HUB_WEB_ORIGINS` to the
address people open the web app at, exactly as their browser shows it
(scheme, host and port; no path), for example
`HUB_WEB_ORIGINS=https://pc.example.ts.net`. Several are separated by
commas. An embedding host sets `web_origins` in the runtime file instead
(`{"max_attachment_bytes": ..., "web_origins": ["https://pc.example.ts.net"]}`),
which wins and needs no restart. What the hub then answers is in
`docs/v2-additions.md`, "Web apps".

- List only web apps you run or trust: a listed page reaches the hub from
  wherever its visitors' browsers are, and reaching the hub is what admits
  a new address (Trust model above).
- `*` allows any page. Use it only for a hub that is on the open internet
  anyway. On a tailnet or a LAN it would let any website that someone on
  that network visits register here and write to every org.
- The operator page (`/`, `/ui/data`, `/ui/messages`) is never shared with
  any page, whatever the list says. Keep the full port on loopback all the
  same.

**2. The hub is reached over HTTPS.** A page served over `https` may only
call `https`, and the hub has no TLS of its own. Put the **relay-only
door** (never the full port) behind something that adds it:

- **Tailscale** (tailnet only): `tailscale serve --bg --https=8443
  http://127.0.0.1:7378` gives `https://<machine>.<tailnet>.ts.net:8443`
  with a certificate browsers and phones already trust. HTTPS has to be
  turned on once in the tailnet's DNS settings; Tailscale then publishes
  the machine's name in the public certificate log.
- **A tunnel or reverse proxy** with a real certificate.
  `expose-hub.ps1` gives the door a public `https://` address through a
  Cloudflare quick tunnel.

Whatever sits in front must pass the `Origin` request header through and
leave the `Access-Control-*` and `Vary` answer headers alone. People then
give the web app that `https://` address as their hub.

When a browser says the hub could not be reached although the address
answers in a new tab, the web app's address is not on the list: the hub's
log then shows a 405 for the path (the browser's preflight, refused). A
listed page's preflights are not logged.

## The WebSocket stream

Since 2.5.0 `GET /api/stream` carries the long poll over one WebSocket:
clients send `sync` or `poll` request frames and get one answer frame each,
built by the same code as `POST /api/sync` and `POST /api/poll`. Clients read
`/healthz` first and use the stream only when `features` lists `stream`; a
hub without it, or a stream that cannot open, is long-polled as before.
Sending, acks, receipts, history and files stay plain HTTP.

- The hub pings every 25 s and drops a client silent for 60 s; a parked
  request is answered after at most 55 s. A stream with no request for 120 s is
  closed.
- Limits: `HUB_STREAM_MAX` open streams (503 with `Retry-After: 30` beyond),
  128 not yet welcomed, 16 welcomed per address, 30 requests per 10 s per
  stream, 64 KiB per client message.
- The secret travels in `X-Org-Auth` on the upgrade (native clients) or in the
  first frame (browsers). It is never in the URL and never logged.
- Logs: the handshake is one ordinary request line (`/api/stream`, 101 or the
  refusal); every answered request logs its usual line plus `"via": "stream"`;
  `{"stream": "open"}` and `{"stream": "close", "secs", "answers", "code",
  "by"}` lines mark each stream. `/ui/data` shows `streams: {open, welcomed}`.
- Reverse proxies must pass the upgrade for `/api/stream` and allow a read
  timeout above 60 s. For nginx:

  ```nginx
  location /api/stream {
      proxy_pass http://127.0.0.1:7370;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_read_timeout 75s;
  }
  ```

  A proxy that strips upgrades answers 426 or another non-101; clients then
  long-poll.
