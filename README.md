# Rugpull Rodeo

A Nostr-based file storage service that allows authorized users to upload and share files.

## Overview

Rugpull Rodeo is a file storage service that:
- Accepts file uploads with Nostr-based authorization
- Stores files in a nested directory structure based on SHA256 hashes
- Enforces storage limits and cleanup policies
- Provides file listing and retrieval capabilities
- Supports file mirroring from other sources

## Build

```bash
# Clone the repository
git clone https://github.com/yourusername/rugpull-rodeo.git
cd rugpull-rodeo

# Build the project
cargo build --release
```

## Run

```bash
# Run the server
cargo run --release
```

## Settings

The following environment variables can be used to configure the service:

| Variable | Description | Default |
|----------|-------------|---------|
| `MAX_TOTAL_SIZE` | Maximum total storage size in MB | 99999 |
| `MAX_TOTAL_FILES` | Maximum number of files allowed | 1000000 |
| `BIND_ADDR` | Address and port to bind the server to | 127.0.0.1:3000 |
| `PUBLIC_URL` | Public URL of the service | http://127.0.0.1:3000 |
| `CLEANUP_INTERVAL_SECS` | Interval in seconds between cleanup runs | 30 |
| `ALLOWED_NPUBS` | Comma-separated list of allowed Nostr public keys (npubs) | `` |
| `MAX_FILE_AGE_DAYS` | Maximum age of files in days before deletion (0 = no age limit) | `0` |
| `ALLOW_WOT` | Enable Web of Trust for additional pubkey authorization | `false` |

### Example Configuration

```bash
export MAX_TOTAL_SIZE=1000
export MAX_TOTAL_FILES=1000
export BIND_ADDR=0.0.0.0:3000
export PUBLIC_URL=https://your-domain.com
export CLEANUP_INTERVAL_SECS=60
export ALLOWED_NPUBS=npub1abc...,npub1def...
export MAX_FILE_AGE_DAYS=30
``` 