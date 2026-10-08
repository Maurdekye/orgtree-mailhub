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
# the read-only operator page is compiled into the binary
COPY mailhub/static/index.html mailhub/static/index.html
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orgtree-mailhub \
 && cp target/release/orgtree-mailhub /usr/local/bin/orgtree-mailhub

FROM debian:bookworm-slim
COPY --from=build /usr/local/bin/orgtree-mailhub /usr/local/bin/orgtree-mailhub
# /data is the named volume: blobs/ (+ a v1 hub.sqlite3 to import)
ENV HUB_DATA=/data
EXPOSE 7370
# 7371 = the FR-10 public listener (API-only; served only when HUB_PUBLIC=1)
EXPOSE 7371
CMD ["orgtree-mailhub", "serve"]
