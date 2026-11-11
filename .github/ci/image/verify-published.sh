#!/usr/bin/env bash
# The post-publish check of one platform (release-image.yml's verify job):
#   verify-published.sh <build-info-docker.json> <arch> <prerelease true|false>
# Pulls <image>:<version> as anyone would (anonymously; with the run's token
# only if the package is still private, and then says so), checks that GHCR's
# <version> (and, for a stable release, latest) carries this platform's image
# by the config digest in build-info-docker.json, then runs smoke.sh on the
# pulled image: healthy, /healthz ok with the version, clean stop.
set -euo pipefail
info=$1 arch=$2 prerelease=$3
image="$(jq -r .image "$info")"
version="$(jq -r .version "$info")"
want="$(jq -r ".platforms.$arch.config_digest" "$info")"
here="$(cd "$(dirname "$0")" && pwd)"
fail() { echo "::error::$*"; exit 1; }

docker logout ghcr.io >/dev/null 2>&1 || true
if docker pull --platform "linux/$arch" "$image:$version"; then
  public=yes
else
  public=no
  echo "::warning::$image is not public yet: an anonymous pull was refused. An owner sets the package public once (docs/OPERATIONS.md, Releases). Checking with the run's token."
  echo "$GH_TOKEN" | docker login ghcr.io -u "$GITHUB_ACTOR" --password-stdin
  docker pull --platform "linux/$arch" "$image:$version"
fi

tags="$version"; [ "$prerelease" = true ] || tags="$version latest"
for t in $tags; do
  idx="$(docker buildx imagetools inspect --raw "$image:$t")"
  m="$(jq -r --arg a "$arch" '.manifests[]? | select(.platform.os == "linux" and .platform.architecture == $a) | .digest' <<<"$idx")"
  [ -n "$m" ] || fail "$image:$t has no linux/$arch image"
  got="$(docker buildx imagetools inspect --raw "$image@$m" | jq -r .config.digest)"
  echo "$image:$t linux/$arch: manifest $m, config $got"
  [ "$got" = "$want" ] || fail "$image:$t linux/$arch is $got, not the release's $want"
done

bash "$here/smoke.sh" "$image:$version" "$version" "smoke-logs/$arch" "linux/$arch"
docker logout ghcr.io >/dev/null 2>&1 || true

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### $image:$version linux/$arch"; echo
    echo "- config digest \`$want\`, the release's image (tags: $tags)"
    echo "- anonymous pull: $public"
    echo "- smoke: healthy, /healthz version $version, clean stop"
  } >> "$GITHUB_STEP_SUMMARY"
fi
