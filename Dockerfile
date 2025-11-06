# Build stage
FROM rustlang/rust:nightly-bullseye-slim AS builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Copy the Cargo files first to cache dependencies
COPY Cargo.toml Cargo.lock ./

# Create a dummy source structure to build dependencies
# This allows us to cache the dependency compilation layer separately
# Only recompiles dependencies when Cargo.toml/Cargo.lock changes
RUN mkdir -p src/handlers src/services && \
    echo "pub mod constants; pub mod error; pub mod handlers; pub mod helpers; pub mod middleware; pub mod models; pub mod services; pub mod trust_network; pub mod utils; fn main() {}" > src/main.rs && \
    echo "" > src/constants.rs && \
    echo "" > src/error.rs && \
    echo "" > src/helpers.rs && \
    echo "" > src/middleware.rs && \
    echo "" > src/models.rs && \
    echo "" > src/trust_network.rs && \
    echo "" > src/utils.rs && \
    echo "pub mod file_serving; pub mod list; pub mod stats; pub mod upload; pub mod upstream; pub mod bloom; pub mod delete;" > src/handlers/mod.rs && \
    echo "pub mod auth; pub mod file_storage; pub mod upload; pub mod download;" > src/services/mod.rs && \
    touch src/handlers/file_serving.rs src/handlers/list.rs src/handlers/stats.rs src/handlers/upload.rs src/handlers/upstream.rs src/handlers/bloom.rs src/handlers/delete.rs && \
    touch src/services/auth.rs src/services/file_storage.rs src/services/upload.rs src/services/download.rs

# Build dependencies only (this layer will be cached unless Cargo.toml/Cargo.lock changes)
# Use BuildKit cache mounts for faster builds - these persist across builds
# Cache the cargo registry (downloaded crates) and target directory (compiled artifacts)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/app/target \
    cargo build --release

# Copy the actual source code
COPY src ./src

# Build the application (only recompiles our code, not dependencies)
# Use BuildKit cache mounts for faster builds
# Note: We use cache for registry but need target in filesystem for COPY
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