# syntax=docker/dockerfile:1

# ── builder ───────────────────────────────────────────────────────────────
# rusqlite's bundled SQLite compiles C, so the builder needs a C toolchain.
# reqwest uses rustls (pure Rust), so no OpenSSL dev headers are required.
FROM rust:1.96-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Cache the dependency build: compile a stub against the manifests, then drop it.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Real build — templates (askama) and assets (rust-embed) are embedded at compile time.
# `touch` is load-bearing: COPY preserves the repo's (older) mtimes, so without it
# cargo's mtime fingerprint would treat the real sources as unchanged from the stub.
COPY src ./src
COPY templates ./templates
COPY assets ./assets
RUN find src templates assets -type f -exec touch {} + \
    && cargo build --release --locked --bin claw

# ── runtime ───────────────────────────────────────────────────────────────
# debian-slim + CA certs (TLS to inference endpoints) + bash/git/curl for the
# coder tool surface. No Node — the whole app is Rust (decision 001).
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash git curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/claw /usr/local/bin/claw

ENV CLAW_DATA_DIR=/data \
    CLAW_PORT=8080
VOLUME ["/data"]
EXPOSE 8080

# A SIGTERM-clean daemon (drain-and-exit); compose/orchestrators stop it cleanly.
ENTRYPOINT ["claw"]
CMD ["serve"]
