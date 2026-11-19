FROM rust:1.99-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY vendor/zed vendor/zed
COPY crates/sync-proto crates/sync-proto
COPY crates/sync-server crates/sync-server
# Provide minimal stub manifests for other workspace members so Cargo resolves the workspace cleanly.
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/sync-client/Cargo.toml crates/sync-client/Cargo.toml
RUN mkdir -p crates/core/src crates/sync-client/src src \
 && touch crates/core/src/lib.rs crates/sync-client/src/lib.rs src/main.rs \
 && cargo build --release --locked -p quiet_sync_server --bin quiet-sync-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd -r -m -d /data quietsync
COPY --from=builder /app/target/release/quiet-sync-server /usr/local/bin/quiet-sync-server
USER quietsync
VOLUME ["/data"]
EXPOSE 8787
ENV QUIET_SYNC_BIND=0.0.0.0:8787
ENV QUIET_SYNC_DATA_DIR=/data
CMD ["quiet-sync-server"]
