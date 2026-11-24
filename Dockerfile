# Multi-stage build for OxidePM
# Build: docker build -t oxidepm .
# Run:   docker run -d --name oxidepm oxidepm

FROM rust:bookworm AS builder

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
COPY --from=builder /build/target/release/oxidepm-sandbox /usr/bin/oxidepm-sandbox

# The runtimes (node, npm, pnpm, yarn, python, go, rust, cargo) are adapters. A release carries
# them with SHA256SUMS under share/oxidepm/adapters. Build with the release download URL to
# have them in the image:
#   docker build --build-arg ADAPTER_BUNDLE_URL=https://.../download/v0.4.0 -t oxidepm .
# The image has no interpreter or toolchain of its own: add the ones your apps need. Then, once:
#   docker exec <container> oxidepm adapter import --bundled --tag runtime --enable --yes
ARG ADAPTER_BUNDLE_URL=""
RUN mkdir -p /usr/share/oxidepm/adapters \
    && if [ -n "$ADAPTER_BUNDLE_URL" ]; then \
         case "$(dpkg --print-architecture)" in \
           amd64) target=x86_64-unknown-linux-gnu ;; \
           arm64) target=aarch64-unknown-linux-gnu ;; \
           *) echo "no adapter bundle for this architecture" >&2; exit 1 ;; \
         esac \
         && curl -fsSL "$ADAPTER_BUNDLE_URL/oxidepm-adapters-$target.tar.gz" | tar -xz -C /usr/share/oxidepm/adapters \
         && test -f /usr/share/oxidepm/adapters/index.json \
         && (cd /usr/share/oxidepm/adapters && sha256sum -c SHA256SUMS); \
       else \
         echo "ADAPTER_BUNDLE_URL is not set: this image has no adapters; import a runtime with oxidepm adapter import <name> --enable"; \
       fi

# Create non-root user
RUN useradd -m -s /bin/bash oxidepm
USER oxidepm
WORKDIR /home/oxidepm

# Start daemon in foreground
ENTRYPOINT ["oxidepmd"]
