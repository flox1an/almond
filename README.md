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

## Features
 - 🌸 Blossom API (BUD-1, BUD-2, BUD-4)
 - 🌸 Temporary file storage with automatic cleanup, first in; first out
 - 🌸 No ownership, no manual delete
 - 🌸 Filesystem only, no database
 - 🌸 Web of trust authorization 

## Environment Variables

- `BIND_ADDR`: Address to bind the server to (default: "127.0.0.1:3000")
- `PUBLIC_URL`: Public URL for the service (default: "http://127.0.0.1:3000")
- `STORAGE_PATH`: Path where files are stored (default: "./files")
- `MAX_TOTAL_SIZE`: Maximum total storage size in MB (default: 99999999)
- `MAX_TOTAL_FILES`: Maximum number of files (default: 99999999)
- `CLEANUP_INTERVAL_SECS`: Interval for cleanup checks in seconds (default: 30)
- `MAX_FILE_AGE_DAYS`: Maximum age of files in days, 0 for no limit (default: 0)
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback (optional)
- `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`: Maximum size for upstream downloads in MB (default: 100)
- `MAX_CHUNK_SIZE_MB`: Maximum size for individual chunks in chunked uploads in MB (default: 100)
- `ALLOW_WOT`: Enable web of trust (optional)
- `ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys (optional)

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
  -e ALLOW_WOT=true \
  -e ALLOWED_NPUBS=npub1... \
  -e MAX_TOTAL_SIZE=1000 \
  -e MAX_FILE_AGE_DAYS=7 \
  -e UPSTREAM_SERVERS=https://backup1.com,https://backup2.com \
  -e MAX_UPSTREAM_DOWNLOAD_SIZE_MB=500 \
  -e MAX_CHUNK_SIZE_MB=200 \
  almond
```

### Environment Variables

All environment variables can be overridden when running the container:

- `BIND_ADDR`: Server bind address
- `PUBLIC_URL`: Public URL for the service
- `STORAGE_PATH`: Path where files are stored
- `MAX_TOTAL_SIZE`: Maximum storage size in MB
- `MAX_TOTAL_FILES`: Maximum number of files
- `CLEANUP_INTERVAL_SECS`: Cleanup interval in seconds
- `MAX_FILE_AGE_DAYS`: Maximum file age in days
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback
- `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`: Maximum size for upstream downloads in MB
- `ALLOW_WOT`: Enable web of trust
- `ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys

### Volume Mounting

The `/app/files` directory in the container is used for file storage. Mount a host directory to persist files:

```bash
docker run -p 3000:3000 -v /host/path:/app/files almond
```

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