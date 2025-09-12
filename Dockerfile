# Build stage
FROM rust:1.86-slim-bullseye AS builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Copy the Cargo files first to cache dependencies
COPY Cargo.toml Cargo.lock ./

# Copy the actual source code
COPY . .

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bullseye-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y ca-certificates libssl1.1 && \
    rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/almond /app/almond

# Ensure binary is executable
RUN chmod +x /app/almond

# Create directory for files
RUN mkdir -p /app/files

# Set environment variables
ENV RUST_LOG=info
ENV BIND_ADDR=0.0.0.0:3000
ENV PUBLIC_URL=http://localhost:3000
ENV MAX_TOTAL_SIZE=99999
ENV MAX_TOTAL_FILES=1000000
ENV CLEANUP_INTERVAL_SECS=30
ENV MAX_FILE_AGE_DAYS=0

# Expose the port
EXPOSE 3000

# Run the binary
CMD ["/app/almond"] 