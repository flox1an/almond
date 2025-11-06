# Build stage
FROM rustlang/rust:nightly-bullseye-slim AS builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
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
FROM debian:bullseye-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y ca-certificates libssl1.1 && \
    rm -rf /var/lib/apt/lists/*

# Copy the binary from builder (copied to /tmp during build to escape cache mount)
COPY --from=builder /tmp/almond /app/almond

# Copy entrypoint script
COPY docker-entrypoint.sh /app/docker-entrypoint.sh

# Ensure binary and entrypoint are executable and verify binary exists
RUN chmod +x /app/almond /app/docker-entrypoint.sh && \
    ls -lah /app/almond

# Create directory for files
RUN mkdir -p /app/files

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

# Use entrypoint script for better logging and debugging
ENTRYPOINT ["/app/docker-entrypoint.sh"] 