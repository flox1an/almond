# almond

All Large Media ON Demand - A temporary file storage service with Nostr-based authorization and web of trust support.

## Features

- Temporary file storage with automatic cleanup
- Nostr-based authorization
- Web of trust support for trusted pubkeys
- Automatic trust network refresh every 4 hours
- File size and count limits
- Automatic file expiration

## Environment Variables

- `BIND_ADDR`: Address to bind the server to (default: "127.0.0.1:3000")
- `PUBLIC_URL`: Public URL for the service (default: "http://127.0.0.1:3000")
- `MAX_TOTAL_SIZE`: Maximum total storage size in MB (default: 99999)
- `MAX_TOTAL_FILES`: Maximum number of files (default: 1000000)
- `CLEANUP_INTERVAL_SECS`: Interval for cleanup checks in seconds (default: 30)
- `MAX_FILE_AGE_DAYS`: Maximum age of files in days, 0 for no limit (default: 0)
- `ALLOW_WOT`: Enable web of trust (optional)
- `ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys (optional)

## Docker

### Building the Image

```bash
docker build -t almond .
```

### Running the Container

Basic run:
```bash
docker run -p 3000:3000 -v /path/to/files:/app/files almond
```

With custom configuration:
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e PUBLIC_URL=https://your-domain.com \
  -e ALLOW_WOT=true \
  -e ALLOWED_NPUBS=npub1... \
  -e MAX_TOTAL_SIZE=1000 \
  -e MAX_FILE_AGE_DAYS=7 \
  almond
```

### Environment Variables

All environment variables can be overridden when running the container:

- `BIND_ADDR`: Server bind address
- `PUBLIC_URL`: Public URL for the service
- `MAX_TOTAL_SIZE`: Maximum storage size in MB
- `MAX_TOTAL_FILES`: Maximum number of files
- `CLEANUP_INTERVAL_SECS`: Cleanup interval in seconds
- `MAX_FILE_AGE_DAYS`: Maximum file age in days
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