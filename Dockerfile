# syntax=docker/dockerfile:1
# The mail hub v2: one Rust binary, no interpreter. Its records live in
# PostgreSQL (HUB_DATABASE_URL — compose.yaml runs a postgres service for
# it); attachment bytes live in the /data volume, as in v1, where the first
# start also imports a v1 store (hub.sqlite3) it finds.

FROM rust:1-bookworm AS build
WORKDIR /src
# kept low on purpose: the build shares the machine with whatever else runs
ARG CARGO_BUILD_JOBS=2
COPY Cargo.toml Cargo.lock ./
COPY hub/ hub/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orgtree-mailhub \
 && cp target/release/orgtree-mailhub /usr/local/bin/orgtree-mailhub

FROM debian:bookworm-slim
# Set by the release workflow (.github/workflows/build-image.yml); a local
# `docker compose build` leaves them empty. image.source links the GHCR
# package ghcr.io/maurdekye/orgtree-mailhub to this repository.
ARG HUB_IMAGE_VERSION=
ARG HUB_IMAGE_REVISION=
LABEL org.opencontainers.image.title="orgtree-mailhub" \
      org.opencontainers.image.description="The orgtree mail hub: org-to-org mail across machines (Rust + PostgreSQL)" \
      org.opencontainers.image.source="https://github.com/Maurdekye/orgtree-mailhub" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${HUB_IMAGE_VERSION}" \
      org.opencontainers.image.revision="${HUB_IMAGE_REVISION}"
COPY --from=build /usr/local/bin/orgtree-mailhub /usr/local/bin/orgtree-mailhub
# /data is the named volume: blobs/ (+ a v1 hub.sqlite3 to import)
ENV HUB_DATA=/data
EXPOSE 7370
# 7371 = the FR-10 public listener (API-only; served only when HUB_PUBLIC=1)
EXPOSE 7371
# compose.yaml declares the same check; this one serves a plain `docker run`
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["orgtree-mailhub", "healthcheck"]
CMD ["orgtree-mailhub", "serve"]
