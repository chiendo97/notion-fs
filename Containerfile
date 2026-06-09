FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libfuse3-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    fuse3 \
    && rm -rf /var/lib/apt/lists/*

# Allow non-root FUSE mounts
RUN sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf || true

WORKDIR /app

COPY --from=builder /build/target/release/notion-fs /usr/local/bin/notion-fs

RUN mkdir -p /mnt/notion

ENTRYPOINT ["notion-fs", "/mnt/notion", "--config", "/config/notion.yaml"]
