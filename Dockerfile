# Multi-stage build for OxidePM
# Build: docker build -t oxidepm .
# Run:   docker run -d --name oxidepm oxidepm

FROM rust:bookworm AS builder

WORKDIR /build
COPY . .

RUN cargo build --locked --workspace --release

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    minisign \
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
RUN set -eu; \
    mkdir -p /usr/share/oxidepm/adapters; \
    if [ -n "$ADAPTER_BUNDLE_URL" ]; then \
         case "$(dpkg --print-architecture)" in \
           amd64) target=x86_64-unknown-linux-gnu ;; \
           arm64) target=aarch64-unknown-linux-gnu ;; \
           *) echo "no adapter bundle for this architecture" >&2; exit 1 ;; \
         esac; \
         base=${ADAPTER_BUNDLE_URL%/}; \
         tag=${base##*/}; \
         archive="oxidepm-adapters-$target.tar.gz"; \
         work=$(mktemp -d); \
         trap 'rm -rf -- "$work"' EXIT; \
         cd "$work"; \
         curl -fsSL "$base/checksums.txt" -o checksums.txt; \
         curl -fsSL "$base/checksums.txt.minisig" -o checksums.txt.minisig; \
         awk 'NR == 1 && $0 !~ /^untrusted comment: / { exit 1 } END { if (NR != 4) exit 1 }' checksums.txt.minisig; \
         comment=$(minisign -V -H -Q -P 'RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC' \
             -m checksums.txt -x checksums.txt.minisig); \
         if [ "$comment" != "oxidepm release checksums tag=$tag" ]; then \
           echo "Release signature has an unexpected trusted comment for $tag" >&2; exit 1; \
         fi; \
         expected=$(awk -v name="$archive" '$2 == name { print $1 }' checksums.txt); \
         if [ "${#expected}" -ne 64 ]; then \
           echo "Expected one SHA-256 checksum for $archive" >&2; exit 1; \
         fi; \
         curl -fsSL "$base/$archive" -o "$archive"; \
         printf '%s  %s\n' "$expected" "$archive" | sha256sum --check --strict; \
         tar -xzf "$archive" -C /usr/share/oxidepm/adapters; \
         test -f /usr/share/oxidepm/adapters/index.json; \
         (cd /usr/share/oxidepm/adapters && sha256sum -c SHA256SUMS); \
       else \
         echo "ADAPTER_BUNDLE_URL is not set: this image has no adapters; import a runtime with oxidepm adapter import <name> --enable"; \
       fi

# Create non-root user
RUN useradd -m -s /bin/bash oxidepm
USER oxidepm
WORKDIR /home/oxidepm

# Start daemon in foreground
ENTRYPOINT ["oxidepmd"]
