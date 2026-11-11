#!/usr/bin/env bash
# Puts one OCI image index over the two per-platform images on GHCR and checks it. Two modes share that part:
#   publish.sh push    <dist dir> <image> <platform tag prefix> <index tag>...
#   publish.sh reindex <image> <platform tag prefix> <index tag>...
# push: <dist dir> is the build-image.yml artifacts (image-<arch>.json and the docker-save files). The images go up
# as <image>:<prefix>-amd64 and -arm64, in OCI format (skopeo copies the saved files: a plain `docker push` of a
# loaded image sends Docker v2 manifests, and an index over those can only be a Docker manifest list, which carries
# no annotations, so GHCR's package page showed no description).
# reindex (release-image.yml's index_only dispatch, for an already published release): nothing is built or
# uploaded. The EXISTING <image>:<prefix>-amd64 and -arm64 are copied registry to registry to OCI (skopeo; the
# config blob, so the config digest, is kept) and the labels are read from the existing image's config.
# Both then point every <index tag> at one application/vnd.oci.image.index.v1+json whose annotations (description,
# source, url, documentation, licenses) are the image's own labels, and fail unless the pushed index says so.
# Needs GH_TOKEN and GITHUB_ACTOR. release-image.yml (<v>-amd64, <v>-arm64; <v>, latest) and build-image.yml's
# ci-test run (ci-test-<sha>-*) both run it.
set -euo pipefail
mode=${1:-}; shift || true
fail() { echo "::error::$*"; exit 1; }
command -v skopeo >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq skopeo; }
echo "$GH_TOKEN" | docker login ghcr.io -u "$GITHUB_ACTOR" --password-stdin
creds="$GITHUB_ACTOR:$GH_TOKEN"

case "$mode" in
  push)
    dist=$1 image=$2 prefix=$3; shift 3
    for a in amd64 arm64; do
      f="$dist/$(jq -r .file "$dist/image-$a.json")"
      docker load -i "$f"
      skopeo copy --format oci --dest-creds "$creds" "docker-archive:$f" "docker://$image:$prefix-$a"
    done
    labels="$(docker image inspect -f '{{json .Config.Labels}}' "$(jq -r .tag "$dist/image-amd64.json")")"
    ;;
  reindex)
    image=$1 prefix=$2; shift 2
    for a in amd64 arm64; do
      skopeo copy --format oci --src-creds "$creds" --dest-creds "$creds" "docker://$image:$prefix-$a" "docker://$image:$prefix-$a"
    done
    labels="$(skopeo inspect --config --creds "$creds" "docker://$image:$prefix-amd64" | jq -c '.config.Labels')"
    ;;
  *) fail "usage: publish.sh push <dist> <image> <prefix> <tag>... | reindex <image> <prefix> <tag>..." ;;
esac
[ "$#" -gt 0 ] || fail "no index tag"
index_tags=("$@")

# GHCR's package page shows an index annotation; take them from the image's labels
ann=()
for k in description source url documentation licenses; do
  val="$(jq -r --arg k "org.opencontainers.image.$k" '.[$k] // empty' <<<"$labels")"
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
