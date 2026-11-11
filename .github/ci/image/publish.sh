#!/usr/bin/env bash
# Puts one OCI image index over the two per-platform images on GHCR and checks it. Two modes share that part:
#   publish.sh push    <dist dir> <image> <platform tag prefix> <index tag>...
#   publish.sh reindex <build-info-docker.json> <image> <platform tag prefix> <index tag>...
# The per-platform images are NEVER converted: they stay exactly as `docker push` sends them (Docker v2 manifests,
# the config digest of the docker-load file, HEALTHCHECK included; a skopeo Docker-to-OCI conversion drops the
# healthcheck and changes the config digest). A Docker manifest list cannot carry annotations, and GHCR shows a
# package description only from an index annotation, so the index is written by hand: an
# application/vnd.oci.image.index.v1+json whose manifests[] reference the Docker manifests by digest, size,
# mediaType and platform, with the annotations description, source, url, documentation and licenses taken from the
# image's labels. It is PUT as a raw manifest (curl against the registry API; no extra tool).
# push: <dist dir> is the build-image.yml artifacts (image-<arch>.json and the docker-save files); the images are
#   docker loaded and pushed as <image>:<prefix>-amd64 and -arm64.
# reindex (release-image.yml's index_only dispatch, for an already published release): nothing is built or
#   uploaded. If AMD64_MANIFEST / ARM64_MANIFEST (sha256:...) are set, <prefix>-amd64 / -arm64 are first pointed back
#   at those manifests (same bytes, new tag); otherwise the tags as they are are used.
# Then every <index tag> is pointed at the index and the check runs: OCI index media type, non-empty description,
# exactly two platforms, and each platform's config digest equals the one in image-<arch>.json (push) or
# build-info-docker.json (reindex). Needs GH_TOKEN and GITHUB_ACTOR.
set -euo pipefail
mode=${1:-}; shift || true
fail() { echo "::error::$*"; exit 1; }
creds="$GITHUB_ACTOR:$GH_TOKEN"
DOCKER_V2=application/vnd.docker.distribution.manifest.v2+json
OCI_MANIFEST=application/vnd.oci.image.manifest.v1+json
OCI_INDEX=application/vnd.oci.image.index.v1+json
ACCEPT="$DOCKER_V2,$OCI_MANIFEST,$OCI_INDEX"
tmp="$(mktemp -d)"

echo "$GH_TOKEN" | docker login ghcr.io -u "$GITHUB_ACTOR" --password-stdin
command -v skopeo >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq skopeo; }

case "$mode" in
  push)
    dist=$1 image=$2 prefix=$3; shift 3
    ;;
  reindex)
    info=$1 image=$2 prefix=$3; shift 3
    ;;
  *) fail "usage: publish.sh push <dist> <image> <prefix> <tag>... | reindex <build-info-docker.json> <image> <prefix> <tag>..." ;;
esac
[ "$#" -gt 0 ] || fail "no index tag"
index_tags=("$@")
repo="${image#ghcr.io/}"
token="$(curl -fsS -u "$creds" "https://ghcr.io/token?service=ghcr.io&scope=repository:$repo:pull,push" | jq -r .token)"
[ -n "$token" ] && [ "$token" != null ] || fail "no registry token"

# reg_get <ref (tag or digest)> <out file>; reg_put <tag> <media type> <file>
reg_get() { curl -fsS -H "Authorization: Bearer $token" -H "Accept: $ACCEPT" -o "$2" "https://ghcr.io/v2/$repo/manifests/$1"; }
reg_put() { curl -fsS -X PUT -H "Authorization: Bearer $token" -H "Content-Type: $2" --data-binary "@$3" -o /dev/null "https://ghcr.io/v2/$repo/manifests/$1"; }
digest_of() { echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; }

declare -A mdigest expected
for a in amd64 arm64; do
  if [ "$mode" = push ]; then
    f="$dist/$(jq -r .file "$dist/image-$a.json")"
    docker load -i "$f"
    docker tag "$(jq -r .tag "$dist/image-$a.json")" "$image:$prefix-$a"
    docker push "$image:$prefix-$a"
    expected[$a]="$(jq -r .config_digest "$dist/image-$a.json")"
  else
    expected[$a]="$(jq -r ".platforms.$a.config_digest" "$info")"
    var="${a^^}_MANIFEST"; orig="${!var:-}"
    if [ -n "$orig" ]; then
      reg_get "$orig" "$tmp/orig-$a.json"
      [ "$(digest_of "$tmp/orig-$a.json")" = "$orig" ] || fail "$a: the manifest fetched for $orig has another digest"
      reg_put "$prefix-$a" "$(jq -r .mediaType "$tmp/orig-$a.json")" "$tmp/orig-$a.json"
    fi
  fi
  reg_get "$prefix-$a" "$tmp/m-$a.json"
  mdigest[$a]="$(digest_of "$tmp/m-$a.json")"
  mt="$(jq -r .mediaType "$tmp/m-$a.json")"
  [ "$mt" = "$DOCKER_V2" ] || [ "$mt" = "$OCI_MANIFEST" ] || fail "$a: $image:$prefix-$a is a $mt, not a single image manifest"
  [ "$(jq -r .config.digest "$tmp/m-$a.json")" = "${expected[$a]}" ] \
    || fail "$a: config digest $(jq -r .config.digest "$tmp/m-$a.json") is not the release's ${expected[$a]}"
  echo "$a: ${mdigest[$a]} ($mt) config ${expected[$a]}"
done

# the labels, from the amd64 image's config; the annotations are the package's description, source, ...
labels="$(skopeo inspect --config --creds "$creds" "docker://$image@${mdigest[amd64]}" | jq -c '.config.Labels // {}')"
ann="$(jq -nc --argjson l "$labels" '["description","source","url","documentation","licenses"]
  | map(select(($l["org.opencontainers.image." + .] // "") != ""))
  | map({key: ("org.opencontainers.image." + .), value: $l["org.opencontainers.image." + .]}) | from_entries')"
for a in amd64 arm64; do
  jq -n --arg d "${mdigest[$a]}" --arg a "$a" --argjson s "$(wc -c < "$tmp/m-$a.json")" --arg t "$(jq -r .mediaType "$tmp/m-$a.json")" \
    '{mediaType: $t, digest: $d, size: $s, platform: {architecture: $a, os: "linux"}}' > "$tmp/e-$a.json"
done
jq -n --argjson ann "$ann" --slurpfile a "$tmp/e-amd64.json" --slurpfile b "$tmp/e-arm64.json" \
  '{schemaVersion: 2, mediaType: "application/vnd.oci.image.index.v1+json", manifests: [$a[0], $b[0]], annotations: $ann}' > "$tmp/index.json"
cat "$tmp/index.json"
for t in "${index_tags[@]}"; do reg_put "$t" "$OCI_INDEX" "$tmp/index.json"; done

# the check, on what the registry now serves
for t in "${index_tags[@]}"; do
  reg_get "$t" "$tmp/got.json"
  echo "$image:$t"; jq -c '{mediaType, manifests: [.manifests[] | {mediaType, digest, platform: .platform.architecture}], annotations}' "$tmp/got.json"
  [ "$(jq -r .mediaType "$tmp/got.json")" = "$OCI_INDEX" ] || fail "$image:$t is not an OCI image index ($(jq -r .mediaType "$tmp/got.json"))"
  [ -n "$(jq -r '.annotations["org.opencontainers.image.description"] // empty' "$tmp/got.json")" ] || fail "$image:$t has no description annotation"
  [ "$(jq '.manifests | length' "$tmp/got.json")" = 2 ] || fail "$image:$t does not list exactly two platforms"
  for a in amd64 arm64; do
    d="$(jq -r --arg a "$a" '.manifests[] | select(.platform.architecture == $a) | .digest' "$tmp/got.json")"
    [ -n "$d" ] || fail "$image:$t lists no $a image"
    reg_get "$d" "$tmp/c-$a.json"
    [ "$(digest_of "$tmp/c-$a.json")" = "$d" ] || fail "$a: manifest $d does not hash to its digest"
    [ "$(jq -r .config.digest "$tmp/c-$a.json")" = "${expected[$a]}" ] \
      || fail "$image:$t $a: config digest $(jq -r .config.digest "$tmp/c-$a.json") is not the release's ${expected[$a]}"
  done
done
docker logout ghcr.io >/dev/null 2>&1 || true
