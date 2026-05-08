# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY codex-blackbox-cli/Cargo.toml codex-blackbox-cli/Cargo.toml
COPY codex-blackbox-core/Cargo.toml codex-blackbox-core/Cargo.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch --locked

COPY codex-blackbox-cli/src/ codex-blackbox-cli/src/
COPY codex-blackbox-core/build.rs codex-blackbox-core/build.rs
COPY codex-blackbox-core/proto/ codex-blackbox-core/proto/
COPY codex-blackbox-core/src/ codex-blackbox-core/src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release -p codex-blackbox-core \
    && cp /app/target/release/codex-blackbox-core /tmp/codex-blackbox-core

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/codex-blackbox-core /usr/local/bin/codex-blackbox-core
EXPOSE 50051 9090
CMD ["codex-blackbox-core"]
