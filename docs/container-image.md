# The official container image

`ghcr.io/maurdekye/orgtree-mailhub` is **the standalone mail hub**: one server
that carries mail between Orgtree orgs, and Hubchat, on different machines. Run
it when you want a hub of your own that stays up on a server, a home lab or a
VM, separate from any desktop.

**It is optional.** The Orgtree desktop app uses the hub built into it, or any
hub it can reach; you need this package only to run a hub on its own.

Orgtree has two other optional packages, each with its own purpose, and this one
is neither: the **headless Orgtree engine** (`ghcr.io/maurdekye/orgtree-engine`,
which runs orgs without the desktop app) and **Hubchat for the web** (the Hubchat
app served to a browser). This package carries mail; it runs no orgs and serves
no Hubchat app.

## At a glance

| | |
| --- | --- |
| PostgreSQL | Required (the official compose file runs `postgres:18`, the version CI tests; not published on any host port). The hub reads `HUB_DATABASE_URL` and `HUB_DATABASE_PASSWORD`. |
| Port 7370 | The full app, including the operator page with **unauthenticated access to all mail**. Publish it only where everyone who can reach it may read all mail (loopback, a trusted tailnet; see [the Trust model](OPERATIONS.md#trust-model-read-before-exposing-anything)); `HUB_BIND` chooses the host interface under compose. |
| Port 7371 | The API-only public listener, off unless `HUB_PUBLIC=1`; compose maps it to host port `HUB_PUBLIC_HOST_PORT` (default 7378). Expose or tunnel this one, never 7370. |
| Volume `/data` | Attachment bytes (`blobs/`). The records are in PostgreSQL's own volume. Back up both. |
| Required env | `HUB_DB_PASSWORD` in `.env` (compose passes it to both services). Optional: `HUB_NAME`, retention, `HUB_WEB_ORIGINS` and the rest in [OPERATIONS.md, Configuration](OPERATIONS.md#configuration). |
| Health | `GET /healthz` (`ok`, `version`, `name`); the image's `HEALTHCHECK` runs `orgtree-mailhub healthcheck`. |
| Upgrade | Back up, `pull`, `up -d` ([Use it](#use-it)); the first start migrates the schema. Going back to an older version means restoring the backup ([OPERATIONS.md, Upgrades](OPERATIONS.md#upgrades)). |
| Backup | Both volumes, or `pg_dump` plus `blobs/`: [OPERATIONS.md, Backup and restore](OPERATIONS.md#backup-and-restore). |

## Releases and tags

From 2.5.0 on, every mail hub release publishes its container image, built by
CI from the release's own commit with this repository's `Dockerfile`
(`.github/workflows/release-image.yml`):

| Where | What |
| --- | --- |
| `ghcr.io/maurdekye/orgtree-mailhub:<version>` | the image for linux/amd64 and linux/arm64 (Raspberry Pi 4/5 and other 64-bit ARM hosts); Docker picks the host's platform |
| `ghcr.io/maurdekye/orgtree-mailhub:latest` | the newest stable release (never a prerelease) |
| `ghcr.io/maurdekye/orgtree-mailhub:<version>-amd64`, `-arm64` | one platform each, the same images |
| release asset `orgtree-mailhub-<version>-docker-amd64.tar.gz`, `-arm64.tar.gz` | the same images as `docker save` files, for `docker load` without the registry |
| release asset `build-info-docker.json` | version, commit, each file's SHA-256 and each image's config digest |
| release asset `SHA256SUMS` | the SHA-256 of every release asset |

Before publishing, CI starts each image on a fresh PostgreSQL 18 and data
volume and requires it to become healthy, answer `/healthz` with its version
and stop cleanly. After publishing, it pulls the published `<version>` on each
platform, checks it is the release's image by config digest, and runs the same
check on what it pulled.

## Use it

With this checkout's compose file (keeps `.env`, ports, volumes and the
PostgreSQL service exactly as they are):

```sh
cp .env.example .env      # first time only: set HUB_DB_PASSWORD (required) and HUB_NAME
echo HUB_VERSION=2.5.0 >> .env   # optional: pin a release; unset follows latest
docker compose -f compose.yaml -f compose.release.yaml pull
docker compose -f compose.yaml -f compose.release.yaml up -d
```

Offline, from the release's files:

```sh
v=2.5.0
curl -LO https://github.com/Maurdekye/orgtree-mailhub/releases/download/v$v/orgtree-mailhub-$v-docker-amd64.tar.gz
curl -LO https://github.com/Maurdekye/orgtree-mailhub/releases/download/v$v/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
docker load -i orgtree-mailhub-$v-docker-amd64.tar.gz   # gives ghcr.io/maurdekye/orgtree-mailhub:$v-amd64
docker tag ghcr.io/maurdekye/orgtree-mailhub:$v-amd64 ghcr.io/maurdekye/orgtree-mailhub:$v
```

then set `HUB_VERSION=$v` in `.env` and run the `up -d` line above (skip `pull`).

**Check what you run.** Each image's config digest is in the release's
`build-info-docker.json` (`platforms.<arch>.config_digest`). It stays the same
through `docker load`, push and pull:

```sh
docker buildx imagetools inspect --raw ghcr.io/maurdekye/orgtree-mailhub:2.5.0   # the index: one manifest per platform
docker buildx imagetools inspect --raw ghcr.io/maurdekye/orgtree-mailhub@<manifest digest> | jq -r .config.digest
curl -s http://127.0.0.1:7370/healthz | jq -r .version
```

## Switching from a locally built hub

For a hub started with `docker compose up -d --build` from a checkout.

**Image reference and digest.** Use `ghcr.io/maurdekye/orgtree-mailhub:<version>`
(pin a version for a server you maintain by hand; `latest` follows each stable
release on the next `pull`). The release's `build-info-docker.json` names each
platform's config digest; to pin exactly, use the digest form
`ghcr.io/maurdekye/orgtree-mailhub@sha256:<index digest>` as the `image:` line.

**Configuration and environment: no change.** The official image is the same
`Dockerfile` built from the release's tagged commit, so it reads the same
variables (`HUB_DB_PASSWORD`, `HUB_NAME`, `HUB_PUBLIC`, `HUB_BIND`, ...) from the
same `.env`, listens on the same ports (7370, and 7371 for `HUB_PUBLIC=1`) and
runs the same `orgtree-mailhub serve`. `compose.release.yaml` changes only where
the hub's image comes from; container names, port mappings, the PostgreSQL
service and its password stay as `compose.yaml` and `.env` define them. Two small
differences in the image itself: it carries OCI labels (version, commit, source)
and its own `HEALTHCHECK` (the same `orgtree-mailhub healthcheck` compose already
runs; compose's settings still apply).

**Volumes and data: no migration.** The volumes are compose's, by name
(`<project>_orgtree-hub-data` for blobs, `<project>_orgtree-hub-db` for
PostgreSQL), so the switch reuses them as they are; run it from the same
directory (or the same `-p` project name) so compose finds them. If the release
is newer than the hub you ran, its first start migrates the database schema
exactly as a local build of that version would (see that release's notes and
[OPERATIONS.md, Upgrades](OPERATIONS.md#upgrades)). Switching at the same version
changes nothing in the data.

**Steps.**

1. Back up first if the version changes ([Backup and restore](OPERATIONS.md#backup-and-restore)).
2. `docker compose -f compose.yaml -f compose.release.yaml pull`
3. `docker compose -f compose.yaml -f compose.release.yaml up -d` (this recreates
   only the hub's container; PostgreSQL keeps running).
4. Check: `curl -s http://127.0.0.1:7370/healthz` shows `"ok": true` and the
   expected `version`; `docker inspect -f '{{.Config.Image}}' orgtree-mailhub`
   shows the official image.

**Compatibility.** The same version behaves the same as a local build of its
tag: same binary sources, same Rust release profile, same base image family
(`debian:bookworm-slim`). Not included: changes you made in your checkout, or
a branch newer than the release. Clients (Orgtree, Hubchat, hubtool) see no
difference.

**Rollback.** To return to the local build, run `docker compose up -d --build`
without `compose.release.yaml`; at the same version the data needs nothing. To an
older version: not supported once a newer schema has migrated the database
(the older hub refuses it); restore the backup from step 1 instead.

## For maintainers

The release recipe, the GHCR package's one-time visibility setting and how to
re-run or backfill a release are in [OPERATIONS.md, Releases](OPERATIONS.md#releases).
