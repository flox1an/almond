# Build stage
# Bookworm is a supported Debian release and provides the current OpenSSL ABI.
FROM debian:bookworm-slim AS builder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:$PATH

WORKDIR /usr/src/app

# Install build dependencies and the pinned Rust nightly (last known-good before the ICE).
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates curl gcc libc6-dev pkg-config libssl-dev && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- -y --default-toolchain nightly-2026-07-23 --profile minimal && \
    rustup default nightly-2026-07-23 && \
    rm -rf /var/lib/apt/lists/*

# Copy dependency files for caching
COPY Cargo.toml Cargo.lock ./

# Copy the actual source code
COPY src ./src

# Build the application
# Use BuildKit cache mounts for faster builds - these persist across builds
# Cache the cargo registry (downloaded crates) and target directory (compiled artifacts)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/app/target \
    cargo build --release && \
    cp /usr/src/app/target/release/almond /tmp/almond

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

# Copy the binary from builder (copied to /tmp during build to escape cache mount)
COPY --from=builder /tmp/almond /app/almond

# Copy entrypoint script
COPY docker-entrypoint.sh /app/docker-entrypoint.sh

# The runtime only needs its binary and the explicitly writable blob volume.
RUN chmod +x /app/almond /app/docker-entrypoint.sh && \
    addgroup --system almond && \
    adduser --system --ingroup almond --home /app --no-create-home almond && \
    mkdir -p /app/files && \
    chown -R almond:almond /app

# Set environment variables
ENV RUST_LOG=info
ENV BIND_ADDR=0.0.0.0:3000
ENV PUBLIC_URL=http://localhost:3000
ENV MAX_TOTAL_SIZE=99999
ENV MAX_TOTAL_FILES=1000000
ENV CLEANUP_INTERVAL_SECS=60
ENV MAX_FILE_AGE_DAYS=0

# Expose the port
EXPOSE 3000
USER almond

# Use entrypoint script for better logging and debugging
ENTRYPOINT ["/app/docker-entrypoint.sh"] 