```
        .__                             .___
 _____  |  |   _____   ____   ____    __| _/
 \__  \ |  |  /     \ /  _ \ /    \  / __ | 
  / __ \|  |_|  Y Y  (  <_> )   |  \/ /_/ | 
 (____  /____/__|_|  /\____/|___|  /\____ | 
      \/           \/            \/      \/  
```

Any Large Media ON Demand - A temporary BLOSSOM file storage service with Nostr-based authorization and web of trust support.

## Overview
- Anyone can upload by default, can be locked down by specifying allowed NPUBs or additionally with a web of trust for those NPUBs.
- Ownership of blobs is NOT tracked, that's why deletion is not supported.
- The project is best for some specific Blossom usecases:
  - Personal server locked to one or a few users (`ALLOWED_NPUBS`)
  - Public upload server with very limited TTL (`MAX_FILE_AGE_DAYS`) or limited size (`MAX_TOTAL_SIZE`).
  - Caching edge server that serves content from upstream blossom servers (`UPSTREAM_SERVERS`).
  - [Local Blossom Cache](#local-blossom-cache) on `127.0.0.1:24242` that proxies and caches blobs from remote servers via `?xs=` and `?as=` hints.

## Features
 - 🌸 Blossom API (BUD-1, BUD-2, BUD-4)
 - 🌸 Temporary file storage with automatic cleanup, first in; first out
 - 🌸 No ownership, no manual delete
 - 🌸 Filesystem only, no database
 - 🌸 Web of trust authorization 

## API Endpoints

### File Operations
- `PUT /upload` - Upload a file (BUD-1)
- `PATCH /upload` - Chunked upload (BUD-2) 
- `GET /:filename` - Download a file by SHA256 hash (supports `?origin=` parameter when `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=true`)
- `HEAD /:filename` - Get file metadata
- `GET /list` - List all stored files (supports `?since=` and `?until=` unix timestamp parameters for filtering by upload date, BUD-2)
- `PUT /mirror` - Mirror a file from another server (BUD-4)

### System Information
- `GET /_stats` - Get server statistics and performance metrics
- `GET /_upstream` - Get configured upstream servers information

#### `/_stats` Response
```json
{
  "stats": {
    "files_uploaded": 1234,
    "files_downloaded": 5678,
    "total_files": 90,
    "total_size_bytes": 1048576000,
    "total_size_mb": 1000.0,
    "upload_throughput_mbps": 0.0,
    "download_throughput_mbps": 0.0,
    "max_total_size_mb": 0.0,
    "max_total_files": 0,
    "storage_usage_percent": 0.0
  },
  "upload_throughput": 0,
  "download_throughput": 0
}
```

#### `/_upstream` Response
```json
{
  "upstream_servers": [
    "https://backup1.example.com",
    "https://backup2.example.com"
  ],
  "count": 2,
  "max_download_size_mb": 100
}
```

## Environment Variables

### Server Configuration
- `BIND_ADDR`: Address to bind the server to (default: "127.0.0.1:3000")
- `PUBLIC_URL`: Public URL for the service (default: "http://127.0.0.1:3000" or "https://127.0.0.1:3000" if HTTPS enabled)

### HTTPS/TLS Configuration
- `ENABLE_HTTPS`: Enable HTTPS with TLS (default: false)
- `TLS_CERT_PATH`: Path to TLS certificate file (default: "./cert.pem")
- `TLS_KEY_PATH`: Path to TLS private key file (default: "./key.pem")
- `TLS_AUTO_GENERATE`: Auto-generate self-signed certificate if not found (default: true)

### Storage Configuration
- `STORAGE_PATH`: Path where files are stored (default: "./files")
- `MAX_TOTAL_SIZE`: Maximum total storage size in MB (default: 99999)
- `MAX_TOTAL_FILES`: Maximum number of files (default: 99999999)
- `CLEANUP_INTERVAL_SECS`: Interval for cleanup checks in seconds (default: 30)
- `MAX_FILE_AGE_DAYS`: Maximum age of files in days, 0 for no limit (default: 0)

### Upstream Configuration
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback (optional)
- `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`: Maximum size for upstream downloads in MB (default: 100)

### Upload Configuration
- `MAX_CHUNK_SIZE_MB`: Maximum size for individual chunks in chunked uploads in MB (default: 100)
- `CHUNK_CLEANUP_TIMEOUT_MINUTES`: Timeout for cleaning up abandoned chunked uploads in minutes (default: 30)

### Authorization Configuration
- `ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys (optional, used as whitelist with WOT as fallback)

### Feature Flags
- `FEATURE_UPLOAD_ENABLED`: Upload endpoint mode - `off`, `wot`, or `public` (default: `public`)
- `FEATURE_MIRROR_ENABLED`: Mirror endpoint mode - `off`, `wot`, or `public` (default: `public`)
- `FEATURE_LIST_ENABLED`: Enable list endpoint (default: true)
- `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED`: Custom upstream origin mode - `off`, `wot`, or `public` (default: `off`)
  - Controls `?origin=`, `?xs=`, and `?as=` URL parameters for upstream lookups
  - In `wot` mode, validates `?as=` author pubkey against Web of Trust
- `FEATURE_HOMEPAGE_ENABLED`: Enable homepage/landing page (default: true)

**Note:** Web of Trust (WOT) is automatically enabled when any feature is set to `wot` mode. WOT is built from your follows (specified in `ALLOWED_NPUBS`) using a 2-hop graph from Nostr relays.

## HTTPS Configuration

### Automatic Self-Signed Certificates

By default, if you enable HTTPS and no certificates are found, Almond will automatically generate self-signed certificates:

```bash
ENABLE_HTTPS=true cargo run
```

This will:
1. Generate a self-signed certificate (`cert.pem`) and private key (`key.pem`)
2. Start the server with HTTPS on the configured address
3. Accept connections from `localhost`, `127.0.0.1`, and `::1`

**Note:** Browsers will show a security warning for self-signed certificates. You'll need to manually trust the certificate or use it for development/testing only.

### Using Custom Certificates

To use your own certificates (e.g., from Let's Encrypt):

```bash
ENABLE_HTTPS=true \
TLS_CERT_PATH=/path/to/cert.pem \
TLS_KEY_PATH=/path/to/key.pem \
TLS_AUTO_GENERATE=false \
cargo run
```

### Docker with HTTPS

Self-signed (auto-generated):
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ENABLE_HTTPS=true \
  -e PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

With custom certificates:
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -v /path/to/certs:/app/certs \
  -e ENABLE_HTTPS=true \
  -e TLS_CERT_PATH=/app/certs/cert.pem \
  -e TLS_KEY_PATH=/app/certs/key.pem \
  -e TLS_AUTO_GENERATE=false \
  -e PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

## Internals
- Blobs are stored in `STORAGE_PATH` within a folder structure with a two layer hierarchy of folders with the first and second letter of the SHA256 storage hash, e.g. 
  ```bash
  ./files/5/3/53860ca3a463ad7170fe1f1e5b08bf4b66422c72b594a329e001a69e07f2e50e.mp4
  ```
- File age is tracked by using the file creation date.
- When starting `almond` the folder structure is read into memory, i.e. changes to the folders are not recognized until the app is restarted.

## Docker

### Building the Image

```bash
docker build -t almond .
```

### Running the Container

Basic run:
```bash
docker run -p 3000:3000 -v /path/to/files:/app/files ghcr.io/flox1an/almond
```

With custom storage path:
```bash
docker run -p 3000:3000 -v /custom/storage:/custom/storage -e STORAGE_PATH=/custom/storage ghcr.io/flox1an/almond
```

With custom configuration:
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e STORAGE_PATH=/app/files \
  -e PUBLIC_URL=https://your-domain.com \
  -e FEATURE_UPLOAD_ENABLED=wot \
  -e FEATURE_MIRROR_ENABLED=wot \
  -e ALLOWED_NPUBS=npub1... \
  -e MAX_TOTAL_SIZE=1000 \
  -e MAX_FILE_AGE_DAYS=7 \
  -e UPSTREAM_SERVERS=https://backup1.com,https://backup2.com \
  -e MAX_UPSTREAM_DOWNLOAD_SIZE_MB=500 \
  -e MAX_CHUNK_SIZE_MB=200 \
  -e CHUNK_CLEANUP_TIMEOUT_MINUTES=60 \
  almond
```

### Volume Mounting

The `/app/files` directory in the container is used for file storage. Mount a host directory to persist files:

```bash
docker run -p 3000:3000 -v /host/path:/app/files almond
```

## Local Blossom Cache

Almond can be configured as a [Local Blossom Cache](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md) — a local proxy that caches blobs from remote Blossom servers on `127.0.0.1:24242`.

Clients request blobs via `GET /<sha256>` with `?xs=` (server hints) and `?as=` (author pubkey) query parameters. If the blob isn't cached locally, Almond fetches it from the hinted servers (or the author's BUD-03 server list), caches it, and returns it to the client. Uploads and mirrors are disabled since the cache is populated entirely through proxying.

### Configuration

```bash
BIND_ADDR=127.0.0.1:24242
PUBLIC_URL=http://127.0.0.1:24242
FEATURE_UPLOAD_ENABLED=off
FEATURE_MIRROR_ENABLED=off
FEATURE_LIST_ENABLED=true
FEATURE_HOMEPAGE_ENABLED=true
FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=public
UPSTREAM_MODE=proxy
MAX_TOTAL_SIZE=5000
MAX_FILE_AGE_DAYS=30
```

### Docker

```bash
docker run -p 24242:24242 \
  -v /path/to/cache:/app/files \
  -e BIND_ADDR=0.0.0.0:24242 \
  -e PUBLIC_URL=http://127.0.0.1:24242 \
  -e FEATURE_UPLOAD_ENABLED=off \
  -e FEATURE_MIRROR_ENABLED=off \
  -e FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=public \
  -e UPSTREAM_MODE=proxy \
  -e MAX_TOTAL_SIZE=5000 \
  -e MAX_FILE_AGE_DAYS=30 \
  ghcr.io/flox1an/almond
```

### How it works

1. Client requests `GET /abc123...def.jpg?xs=cdn.example.com&as=<pubkey>`
2. Almond checks the local cache
3. If cached, returns the blob immediately
4. If not cached, tries `xs` server hints first, then fetches the author's BUD-03 server list (kind:10063) from `as` hints
5. Caches the blob locally and returns it to the client
6. Returns `404` if the blob can't be found on any hinted server

Cache eviction is automatic — oldest files are removed when `MAX_TOTAL_SIZE` or `MAX_FILE_AGE_DAYS` limits are reached.

## Development

### Prerequisites

- Rust 1.76 or later
- OpenSSL development libraries

### Building

```bash
cargo build --release
```

### Running

```bash
cargo run --release
```

## License

MIT 