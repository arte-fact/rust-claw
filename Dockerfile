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

# ── mcp web-search server (M12) ─────────────────────────────────────────────
# A separate Rust binary built from a pinned upstream commit. claw spawns it over
# stdio and exposes its fetch/search/screenshot/interact tools to every agent.
# Still 100% Rust (decision 001) — the cost is shipping Chromium in the runtime.
FROM rust:1.96-slim-bookworm AS mcp-builder
WORKDIR /mcp
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Pin the commit so the image is reproducible (supply-chain).
ARG MCP_WEB_SEARCH_REF=fe1ad7c6cac21dedb1b96540371ca16927adb413
RUN git clone https://github.com/arte-fact/mcp-web-search-hacks . \
    && git checkout "${MCP_WEB_SEARCH_REF}" \
    && cargo build --release -p mcp-web-search-stdio

# ── runtime ───────────────────────────────────────────────────────────────
# debian-slim + CA certs (TLS to inference endpoints) + bash/git/curl for the
# coder tool surface + chromium (the web-search MCP server drives it headless).
# No Node — the whole app is Rust (decision 001).
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash git curl chromium \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/claw /usr/local/bin/claw
COPY --from=mcp-builder /mcp/target/release/mcp-web-search-stdio /usr/local/bin/mcp-web-search-stdio

# headless_chrome finds the browser here; it already launches with sandbox off,
# so running as root inside the container is fine.
ENV CHROME_PATH=/usr/bin/chromium \
    CLAW_DATA_DIR=/data \
    CLAW_PORT=8080
VOLUME ["/data"]
EXPOSE 8080

# A SIGTERM-clean daemon (drain-and-exit); compose/orchestrators stop it cleanly.
ENTRYPOINT ["claw"]
CMD ["serve"]
