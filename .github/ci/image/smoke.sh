#!/usr/bin/env bash
# Smoke-tests a mail hub image the way compose.yaml runs it: its own network,
# a throwaway PostgreSQL 18 and a fresh data volume.
#   smoke.sh <image> <version> <log-dir> [<platform>]
# Checks: `version` prints <version>; the hub becomes healthy (the image's own
# HEALTHCHECK, `orgtree-mailhub healthcheck`); /healthz on the loopback-mapped
# port answers ok:true with "version": <version>; `docker stop` (SIGTERM) ends
# it with status 0. Everything it creates is named mailhub-smoke-<random> and
# removed on exit; nothing else on the host is touched.
set -euo pipefail
image=$1 version=$2 logs=$3 platform=${4:-}
mkdir -p "$logs"
id="mailhub-smoke-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
pw="smoke-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
plat=(); [ -n "$platform" ] && plat=(--platform "$platform")

cleanup() {
  docker logs "$id-hub" > "$logs/hub.log" 2>&1 || true
  docker rm -f "$id-hub" "$id-db" >/dev/null 2>&1 || true
  docker network rm "$id" >/dev/null 2>&1 || true
  docker volume rm "$id-data" "$id-pg" >/dev/null 2>&1 || true
}
trap cleanup EXIT
fail() { echo "::error::$*"; exit 1; }

got="$(docker run --rm "${plat[@]}" "$image" orgtree-mailhub version)"
echo "version: $got"
[ "$got" = "orgtree-mailhub $version" ] || fail "the image says '$got', not 'orgtree-mailhub $version'"
arch="$(docker image inspect -f '{{.Os}}/{{.Architecture}}' "$image")"
echo "platform: $arch"

docker network create "$id" >/dev/null
docker run -d --name "$id-db" --network "$id" -v "$id-pg:/var/lib/postgresql" \
  -e POSTGRES_USER=mailhub -e POSTGRES_PASSWORD="$pw" -e POSTGRES_DB=mailhub postgres:18 >/dev/null
for _ in $(seq 60); do docker exec "$id-db" pg_isready -U mailhub -d mailhub >/dev/null 2>&1 && break; sleep 1; done
docker exec "$id-db" pg_isready -U mailhub -d mailhub >/dev/null || fail "PostgreSQL did not start"

docker run -d --name "$id-hub" "${plat[@]}" --network "$id" -v "$id-data:/data" -p 127.0.0.1::7370 \
  --health-interval 2s --health-start-period 0s \
  -e HUB_NAME=smoke -e HUB_DATABASE_URL="postgres://mailhub@$id-db:5432/mailhub" \
  -e HUB_DATABASE_PASSWORD="$pw" "$image" >/dev/null
port="$(docker port "$id-hub" 7370/tcp | head -n1)"; port=${port##*:}
health=""
for _ in $(seq 90); do
  health="$(docker inspect -f '{{.State.Health.Status}}' "$id-hub" 2>/dev/null || true)"
  [ "$health" = healthy ] && break
  [ "$(docker inspect -f '{{.State.Running}}' "$id-hub")" = true ] || fail "the hub exited: $(docker logs "$id-hub" 2>&1 | tail -n 5)"
  sleep 1
done
[ "$health" = healthy ] || fail "the hub is '$health', not healthy, after 90 s"
echo "health: $health"

curl -fsS "http://127.0.0.1:$port/healthz" > "$logs/healthz.json" || fail "/healthz did not answer"
cat "$logs/healthz.json"; echo
[ "$(jq -r .ok "$logs/healthz.json")" = true ] || fail "/healthz is not ok"
[ "$(jq -r .version "$logs/healthz.json")" = "$version" ] || fail "/healthz version is $(jq -r .version "$logs/healthz.json"), not $version"
[ "$(jq -r .name "$logs/healthz.json")" = smoke ] || fail "/healthz name is not HUB_NAME"

docker stop -t 20 "$id-hub" >/dev/null
status="$(docker inspect -f '{{.State.ExitCode}}' "$id-hub")"
[ "$status" = 0 ] || fail "after SIGTERM the hub exited with status $status, not 0"
echo "stop: exit 0"
echo "smoke passed: $image ($arch) $version"
