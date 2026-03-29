# Multi-stage build for OxidePM
# Build: docker build -t oxidepm .
# Run:   docker run -d --name oxidepm oxidepm

FROM rust:1.82-bookworm AS builder

WORKDIR /build
COPY . .

RUN cargo build --workspace --release

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/oxidepm /usr/bin/oxidepm
COPY --from=builder /build/target/release/oxidepmd /usr/bin/oxidepmd

# Create non-root user
RUN useradd -m -s /bin/bash oxidepm
USER oxidepm
WORKDIR /home/oxidepm

# Start daemon in foreground
ENTRYPOINT ["oxidepmd"]
