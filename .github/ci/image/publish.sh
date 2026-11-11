#!/usr/bin/env bash
# Pushes the two built images to GHCR as OCI images and puts one OCI image index over them, then checks it:
#   publish.sh <dist dir> <image> <platform tag prefix> <index tag>...
# <dist dir> is the build-image.yml artifacts (image-<arch>.json and the docker-save files). The images go up as
# <image>:<prefix>-amd64 and -arm64, in OCI format (skopeo copies the saved files: a plain `docker push` of a loaded
# image sends Docker v2 manifests, and an index over those can only be a Docker manifest list, which carries no
# annotations, so GHCR's package page showed no description). Every <index tag> then points at one
# application/vnd.oci.image.index.v1+json whose annotations (description, source, url, documentation, licenses)
# are the image's own labels. The step fails unless the pushed index says so. Needs GH_TOKEN and GITHUB_ACTOR.
# release-image.yml (<v>-amd64, <v>-arm64; <v>, latest) and build-image.yml's ci-test run (ci-test-<sha>-*) both run it.
set -euo pipefail
dist=$1 image=$2 prefix=$3; shift 3
[ "$#" -gt 0 ] || { echo "::error::no index tag"; exit 1; }
index_tags=("$@")
fail() { echo "::error::$*"; exit 1; }

command -v skopeo >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq skopeo; }
echo "$GH_TOKEN" | docker login ghcr.io -u "$GITHUB_ACTOR" --password-stdin

for a in amd64 arm64; do
  f="$dist/$(jq -r .file "$dist/image-$a.json")"
  docker load -i "$f"
  skopeo copy --format oci --dest-creds "$GITHUB_ACTOR:$GH_TOKEN" "docker-archive:$f" "docker://$image:$prefix-$a"
done

# GHCR's package page shows an index annotation; take them from the image's labels
ann=()
src="$(jq -r .tag "$dist/image-amd64.json")"
for k in description source url documentation licenses; do
  val="$(docker image inspect -f "{{index .Config.Labels \"org.opencontainers.image.$k\"}}" "$src")"
  [ -z "$val" ] || ann+=(--annotation "index:org.opencontainers.image.$k=$val")
done
tags=(); for t in "${index_tags[@]}"; do tags+=(-t "$image:$t"); done
docker buildx imagetools create "${tags[@]}" "${ann[@]}" "$image:$prefix-amd64" "$image:$prefix-arm64"

# the check: an OCI index of OCI manifests, with a description
for t in "${index_tags[@]}"; do
  raw="$(docker buildx imagetools inspect --raw "$image:$t")"
  echo "$image:$t"; jq -c '{mediaType, manifests: [.manifests[] | {mediaType, platform: .platform.architecture}], annotations}' <<<"$raw"
  [ "$(jq -r .mediaType <<<"$raw")" = application/vnd.oci.image.index.v1+json ] \
    || fail "$image:$t is not an OCI image index ($(jq -r .mediaType <<<"$raw"))"
  [ "$(jq '[.manifests[] | select(.mediaType != "application/vnd.oci.image.manifest.v1+json")] | length' <<<"$raw")" = 0 ] \
    || fail "$image:$t lists a platform image that is not an OCI manifest"
  [ "$(jq '.manifests | length' <<<"$raw")" = 2 ] || fail "$image:$t does not list exactly two platforms"
  [ -n "$(jq -r '.annotations["org.opencontainers.image.description"] // empty' <<<"$raw")" ] \
    || fail "$image:$t has no description annotation"
done
docker logout ghcr.io >/dev/null 2>&1 || true
