# Gemini Project Context: Almond

Almond (Any Large Media ON Demand) is a high-performance, temporary Blossom file storage service designed for the Nostr ecosystem. It serves as a lightweight alternative to permanent storage solutions, focusing on use cases like personal servers, public scratch space, or caching edge nodes.

## Project Overview

- **Purpose**: A Blossom server (BUD-1, BUD-2, BUD-4, BUD-7, BUD-10) with Nostr-based authorization and Web of Trust (WoT) support.
- **Core Technology**: Rust (Axum, Tokio, Nostr-RS).
- **Architecture**: Service-oriented architecture with a clear separation between HTTP handlers and business logic. Uses filesystem-only storage with a two-level nested directory structure based on SHA-256 hashes.
- **Key Features**:
  - Temporary storage with automatic FIFO cleanup (Age-based, Size-based, or Expiration-based).
  - No database required; metadata is stored in-memory and synced from the filesystem on startup.
  - Advanced Auth: Nostr Kind 24242 events, whitelisting, and 2-hop Web of Trust.
  - Upstream Fallback: Proxy, Redirect, or "Redirect and Cache" modes for edge serving.
  - Cashu Payments (BUD-07): Support for paid uploads/downloads via NUT-24 tokens.
  - Metrics: Built-in Prometheus exporter and `/ _stats` endpoint.

## Building and Running

### Prerequisites
- Rust 1.76+
- OpenSSL development libraries

### Key Commands
- **Run in Development**: `cargo run`
- **Build Release Binary**: `cargo build --release`
- **Run Tests**: `cargo test`
- **Check Code Quality**: `cargo check` and `cargo fmt`
- **Docker Build**: `docker build -t almond .`
- **Docker Run**: `docker run -p 3000:3000 -v $(pwd)/files:/app/files almond`

### Environment Configuration
The project uses `dotenvy` for configuration. Key variables include:
- `STORAGE_PATH`: Directory for file storage (default: `./files`).
- `FEATURE_UPLOAD_ENABLED`: `public`, `wot`, `dvm`, or `off`.
- `ALLOWED_NPUBS`: Whitelist of Nostr public keys.
- `UPSTREAM_SERVERS`: Comma-separated list of Blossom servers for fallback.
- `ENABLE_HTTPS`: Set to `true` for automatic self-signed or custom TLS.

## Development Conventions

- **Module Structure**:
  - `src/handlers/`: Axum request handlers (HTTP specific).
  - `src/services/`: Core business logic (Storage, Auth, Upload/Download).
  - `src/models.rs`: Centralized data structures and `AppState`.
  - `src/trust_network.rs`: Logic for building the Web of Trust and DVM lists.
- **Error Handling**: Uses a custom `AppError` (in `src/error.rs`) that maps directly to HTTP status codes.
- **Async/Await**: The entire codebase is asynchronous, leveraging Tokio.
- **Security**: 
  - Strict SSRF protection for mirroring.
  - Private IP blocking for upstream requests.
  - Streaming-first approach to minimize memory footprint.
- **Naming**: Follows standard Rust conventions (`snake_case` for variables/functions, `PascalCase` for types).

## Important Files
- `CONTEXT.md`: Detailed technical overview and API specifications.
- `Cargo.toml`: Project dependencies and binary targets.
- `src/main.rs`: Entry point, routing, and background job management.
- `.env.example`: Template for all supported configuration options.
