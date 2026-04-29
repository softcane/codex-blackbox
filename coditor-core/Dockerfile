# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY coditor-cli/Cargo.toml coditor-cli/Cargo.toml
COPY coditor-core/Cargo.toml coditor-core/Cargo.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch --locked

COPY coditor-cli/src/ coditor-cli/src/
COPY coditor-core/build.rs coditor-core/build.rs
COPY coditor-core/proto/ coditor-core/proto/
COPY coditor-core/src/ coditor-core/src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release -p coditor-core \
    && cp /app/target/release/coditor-core /tmp/coditor-core

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/coditor-core /usr/local/bin/coditor-core
EXPOSE 50051 9090
CMD ["coditor-core"]
