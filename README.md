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
- Deletion requires a signed, single-use BUD-11 authorization bound to the blob hash.
- The project is best for some specific Blossom usecases:
  - Personal server locked to one or a few users (`ALMOND_ALLOWED_NPUBS`)
  - Public upload server with very limited TTL (`ALMOND_UPLOAD_MAX_AGE`) or limited size (`ALMOND_STORAGE_MAX_SIZE`).
  - Caching edge server that serves content from upstream blossom servers (`ALMOND_UPSTREAM_SERVERS`).
  - [Local Blossom Cache](#local-blossom-cache) on `127.0.0.1:24242` that proxies and caches blobs from remote servers via `?xs=` and `?as=` hints.

## Features
 - 🌸 Blossom API (BUD-01, BUD-02, BUD-04, BUD-06, BUD-11, BUD-12)
 - 🌸 Temporary file storage with automatic cleanup, first in; first out
 - 🌸 Filesystem only, no database
 - 🌸 Web of trust authorization 

## API Endpoints

### File Operations
- `PUT /upload` - Upload a file (BUD-02)
- `HEAD /upload` - Validate upload metadata and discover whether the same admission policy permits an upload (BUD-06)
- `PATCH /upload` - Almond resumable-upload extension
- `GET /:filename` - Download a blob by SHA-256; an optional filename extension is accepted and ignored for lookup (BUD-01)
- `HEAD /:filename` - Get blob metadata (BUD-01)
- `GET /list` / `GET /list/<pubkey>` - List stored files with BUD-12 cursor pagination (`?limit=100&cursor=<last_sha256>`). Optional `?since=` and `?until=` filters are supported but should not be used for pagination.
- `PUT /mirror` - Mirror a file from another server (BUD-04)
- `DELETE /:filename` - Delete a blob with a BUD-11 authorization (BUD-12)

### Blob Delivery Semantics

Blobs are immutable and content-addressed, so `GET`/`HEAD /:filename` serve them
with aggressive, safe caching:

- **`ETag`** — the SHA-256 of the blob, quoted (`"a1b2..."`). A strong validator
  that never needs to touch the file.
- **`If-None-Match`** — a match returns `304 Not Modified` with no body. Handles
  tag lists, weak (`W/"..."`) tags and `*`.
- **`Cache-Control: public, max-age=31536000, immutable`** plus a one-year `Expires`.
- **Range requests** — all three RFC 9110 forms: `bytes=START-END`, `bytes=START-`
  and the suffix form `bytes=-N` (used by MP4 players probing the trailing `moov`
  atom). An `END` past EOF is clamped rather than rejected.
- **`416 Range Not Satisfiable`** with `Content-Range: bytes */SIZE` when the
  requested range lies entirely outside the blob.
- **`If-Range`** — a stale validator falls back to the full `200` body.
- Multi-range requests (`bytes=0-9,20-29`) are answered with the full `200`
  representation; `multipart/byteranges` is not implemented.

When an uploaded blob has a finite retention deadline, `GET` and `HEAD`
(including extension variants, `206` and `304`) return RFC 8594 / BUD-01
`Sunset`, exposed to browsers via `Access-Control-Expose-Headers`. The value is
exactly the deadline the cleanup job uses: the earlier of the persisted
expiration and `created_at + ALMOND_UPLOAD_MAX_AGE` under the current config, so a
lowered or raised limit shows up immediately (uploads stored before v0.4.24 may
carry a persisted server deadline that a raised limit does not extend). The Almond-specific request
header `X-Expiration` (Unix timestamp, not part of any BUD) can only shorten
retention below `ALMOND_UPLOAD_MAX_AGE`; the descriptor `expiration` field carries
the same deadline as `Sunset`. Upstream-cache copies and
upstream redirects carry no `Sunset`: the URL keeps resolving via upstream.
`Sunset` is advisory; an expired blob is served until cleanup removes it.
Blob responses stay `immutable` for a year, so a CDN in front of Almond may hold
stale `Sunset` values or deleted blobs until purged. Report-quarantined hashes
cannot be re-uploaded.

`GET /filter` (BUD-11) is rendered once per index change and served from cache,
so it also carries an `ETag` and answers `If-None-Match` with `304`. The
`timestamp` field is the render time rather than the request time — that is what
makes the body byte-stable and the validator meaningful.

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

## Running / command line

The binary is `almond`. Every setting can come from four sources, highest
precedence first:

1. command-line flag (`--upload-access wot`)
2. process environment (`ALMOND_UPLOAD_ACCESS=wot`)
3. config file (dotenv format, same `ALMOND_*` names)
4. built-in default

```bash
almond --config /etc/almond/almond.env --bind-addr 0.0.0.0:3000
```

- **Config file:** `--config <FILE>` (or `ALMOND_CONFIG`) loads a dotenv file;
  without it, `./.env` is loaded when present. Values in the file never override
  real environment variables. Copy [`.env.example`](.env.example) as a starting point.
- **Name mapping:** env name = `ALMOND_` + flag name upper-cased with `-` → `_`
  (`--chunk-max-sessions-per-pubkey` ↔ `ALMOND_CHUNK_MAX_SESSIONS_PER_PUBKEY`).
  An empty value (`ALMOND_X=`) behaves exactly like an unset one.
  Pass secrets (S3 keys, metrics token) via environment or config file:
  command-line arguments are visible to other users in `ps`.
- **Value formats:**
  - durations: `<n><unit>` with unit `s`, `m`, `h`, `d` (`30s`, `5m`, `24h`, `7d`); bare `0` is allowed, any other number needs a unit
  - sizes: `<n><unit>` with unit `B`, `KiB`, `MiB`, `GiB`, `TiB` (case-insensitive, IEC only; `MB`/`GB` are rejected); bare `0` is allowed
  - booleans: `true`/`false` (also `yes`/`no`, `on`/`off`, `1`/`0`); on the CLI a bare `--tls-enabled` means true, `--list-enabled=false` disables
  - lists: comma-separated; on the CLI either comma-separated or the flag repeated
- **Strict parsing:** unknown enum values, invalid booleans, invalid npubs, and bad
  durations/sizes stop startup with an error instead of falling back to a default.
- `almond --help` lists every flag with its env name and default; `almond --version` prints the version.
- **Process model:** Almond always runs in the foreground (no daemon mode); use a
  service manager. A minimal systemd unit is in [`contrib/almond.service`](contrib/almond.service).
- **Logging:** `RUST_LOG` (e.g. `RUST_LOG=warn`, `RUST_LOG=almond=debug`) controls
  log output (default: `info`); it is also honored when set in the config file.

## Configuration

The full list of settings with comments is in [`.env.example`](.env.example).

### Server Configuration
- `ALMOND_BIND_ADDR`: Address to bind the server to (default: "127.0.0.1:3000")
- `ALMOND_PUBLIC_URL`: Public URL for the service (default: "http://127.0.0.1:3000" or "https://127.0.0.1:3000" if HTTPS enabled)
- `ALMOND_CORS_ORIGINS`: Comma-separated browser origins allowed to read API responses (optional)

### HTTPS/TLS Configuration
- `ALMOND_TLS_ENABLED`: Enable HTTPS with TLS (default: false)
- `ALMOND_TLS_CERT`: Path to TLS certificate file (default: "./cert.pem")
- `ALMOND_TLS_KEY`: Path to TLS private key file (default: "./key.pem")
- `ALMOND_TLS_SELF_SIGNED`: Auto-generate a self-signed certificate if cert/key are missing; development only (default: false)

### Storage Configuration
- `ALMOND_STORAGE_PATH`: Storage root. Completed uploads live under `uploads/`, transparent upstream fills under `upstream-cache/`, and incomplete data under `temp/`.
- `ALMOND_STORAGE_MAX_SIZE`: Maximum aggregate storage size across both completed-blob origins; `0` = unlimited (default: `0`).
- `ALMOND_STORAGE_MAX_FILES`: Maximum aggregate completed-blob count across both origins; `0` = unlimited (default: `0`).
- `ALMOND_STORAGE_MIN_FREE`: Minimum free disk space; uploads get HTTP 507 below it (default: `256MiB`).
- `ALMOND_BLOB_MAX_SIZE`: Absolute per-blob size limit (default: `500MiB`).
- `ALMOND_CLEANUP_INTERVAL`: Expiry and capacity cleanup interval; must be greater than zero (default: `30s`).
- `ALMOND_UPLOAD_MAX_AGE`: Maximum age of uploaded, explicitly mirrored, and HLS-mirrored blobs; `0` disables this policy (default: 0).
- `ALMOND_UPSTREAM_CACHE_TTL`: Maximum age of transparently fetched upstream cache entries; `0` disables this policy (default: `1d`). Serving a cached blob does not refresh this TTL.

Capacity eviction removes the oldest upstream-cache entries before uploaded content. Existing legacy hash trees are migrated into `uploads/` at startup, preserving files by rename.

### Optional S3-Compatible Native Storage
New uploads and explicit mirrors use S3 when all four variables are configured. They remain `Upload`-origin content for retention and collision precedence; automatic upstream cache fills remain local. Almond continues to proxy every blob response.

All four variables are required together. Supplying only a subset fails startup.

```dotenv
ALMOND_S3_ENDPOINT=https://<endpoint>
ALMOND_S3_BUCKET=<bucket-name>
ALMOND_S3_ACCESS_KEY_ID=<access-key-id>
ALMOND_S3_SECRET_ACCESS_KEY=<secret-access-key>
```

Cloudflare R2:
```dotenv
ALMOND_S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
ALMOND_S3_BUCKET=<bucket-name>
ALMOND_S3_ACCESS_KEY_ID=<r2-access-key-id>
ALMOND_S3_SECRET_ACCESS_KEY=<r2-secret-access-key>
```

MinIO:
```dotenv
ALMOND_S3_ENDPOINT=http://localhost:9000
ALMOND_S3_BUCKET=<bucket-name>
ALMOND_S3_ACCESS_KEY_ID=<minio-access-key>
ALMOND_S3_SECRET_ACCESS_KEY=<minio-secret-key>
```

Backblaze B2:
```dotenv
ALMOND_S3_ENDPOINT=https://s3.<region>.backblazeb2.com
ALMOND_S3_BUCKET=<bucket-name>
ALMOND_S3_ACCESS_KEY_ID=<b2-key-id>
ALMOND_S3_SECRET_ACCESS_KEY=<b2-application-key>
```

### Upstream Configuration
- `ALMOND_UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback (optional)
- `ALMOND_UPSTREAM_MODE`: How to handle upstream requests (default: `proxy`)
  - `proxy`: Stream from upstream while saving locally. Client receives data immediately while the file is cached.
  - `redirect`: Issue 302 redirect to upstream. No local caching. Reduces bandwidth/CPU on the Almond server.
  - `redirect-and-cache`: Issue 302 redirect to upstream, but also download in the background for future requests.
- `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE`: Maximum size for upstream downloads (default: `100MiB`).
  Larger blobs are still served, but proxied through without being cached.

### Upload Configuration
- `ALMOND_CHUNK_MAX_SIZE`: Maximum size for individual chunks in chunked uploads; must not exceed `ALMOND_BLOB_MAX_SIZE` (default: `100MiB`)
- `ALMOND_CHUNK_SESSION_TIMEOUT`: Timeout for cleaning up abandoned chunked uploads (default: `30m`)

### Authorization Configuration
- `ALMOND_ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys (optional, used as whitelist with WOT as fallback); an invalid npub stops startup
- `ALMOND_AUTH_MAX_TTL`: Upper bound on authorization token lifetime; must be greater than zero (default: `24h`)
- `ALMOND_AUTH_CLOCK_SKEW`: Tolerated client clock skew (default: `30s`)

### Access Modes and Feature Switches
- `ALMOND_UPLOAD_ACCESS`: Upload endpoint mode - `off`, `wot`, `dvm`, or `public` (default: `public`)
- `ALMOND_MIRROR_ACCESS`: Mirror endpoint mode - `off`, `wot`, `dvm`, or `public` (default: `public`)
- `ALMOND_REPORT_ACCESS`: BUD-09 report endpoint mode - `off`, `wot`, `dvm`, or `public` (default: `off`)
- `ALMOND_LIST_ENABLED`: Enable list endpoint (default: true)
- `ALMOND_CUSTOM_ORIGIN_ACCESS`: Custom upstream origin mode - `off`, `wot`, `dvm`, or `public` (default: `off`)
  - Controls `?origin=`, `?xs=`, and `?as=` URL parameters for upstream lookups
  - In `wot` mode, validates `?as=` author pubkey against Web of Trust
- `ALMOND_HOMEPAGE_ENABLED`: Enable homepage/landing page (default: true)
- `ALMOND_CASHU_PAID`: Comma-separated paid operations - `upload`, `mirror`, `download` (default: none); requires `ALMOND_CASHU_MINT`

**Note:** Web of Trust (WOT) is automatically enabled when any access mode is set to `wot`. WOT is built from your follows (specified in `ALMOND_ALLOWED_NPUBS`) using a 2-hop graph from Nostr relays.

### Migrating from pre-0.5 names

Almond 0.5 renamed every setting to the `ALMOND_*` scheme. The old names below
still work as deprecated aliases: their values are translated (units appended)
and a deprecation warning is logged at startup. If both the old and the new name
are set, the new one wins and a warning names the ignored old one. The old names
will be removed in a later release.

Docker images apply their container defaults (bind address, public URL,
storage path, TLS and wallet paths, ...) only when neither the new nor the old
name is set, so old names passed with `-e` keep working there too.

| Old name | New name | Value conversion |
|---|---|---|
| `BIND_ADDR` | `ALMOND_BIND_ADDR` | – |
| `PUBLIC_URL` | `ALMOND_PUBLIC_URL` | – |
| `CORS_ALLOWED_ORIGINS` | `ALMOND_CORS_ORIGINS` | – |
| `ENABLE_HTTPS` | `ALMOND_TLS_ENABLED` | – |
| `TLS_CERT_PATH` | `ALMOND_TLS_CERT` | – |
| `TLS_KEY_PATH` | `ALMOND_TLS_KEY` | – |
| `TLS_AUTO_GENERATE` | `ALMOND_TLS_SELF_SIGNED` | – ; the default is now `false`, set `true` to keep auto-generation |
| `STORAGE_PATH` | `ALMOND_STORAGE_PATH` | – |
| `MAX_TOTAL_SIZE` | `ALMOND_STORAGE_MAX_SIZE` | MiB: `99999` → `99999MiB` |
| `MAX_TOTAL_FILES` | `ALMOND_STORAGE_MAX_FILES` | – ; `0` now means unlimited |
| `MIN_FREE_DISK_MB` | `ALMOND_STORAGE_MIN_FREE` | `256` → `256MiB` |
| `MAX_BLOB_SIZE_MB` | `ALMOND_BLOB_MAX_SIZE` | `100` → `100MiB` |
| `CLEANUP_INTERVAL_SECS` | `ALMOND_CLEANUP_INTERVAL` | `30` → `30s` |
| `MAX_FILE_AGE_DAYS` | `ALMOND_UPLOAD_MAX_AGE` | `7` → `7d` |
| `MAX_UPSTREAM_CACHE_TTL_DAYS` | `ALMOND_UPSTREAM_CACHE_TTL` | `1` → `1d` |
| `FEATURE_UPLOAD_ENABLED` | `ALMOND_UPLOAD_ACCESS` | – |
| `FEATURE_MIRROR_ENABLED` | `ALMOND_MIRROR_ACCESS` | – |
| `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED` | `ALMOND_CUSTOM_ORIGIN_ACCESS` | – |
| `FEATURE_REPORT_ENABLED` | `ALMOND_REPORT_ACCESS` | – |
| `REPORT_ACTION` | `ALMOND_REPORT_ACTION` | – |
| `FEATURE_LIST_ENABLED` | `ALMOND_LIST_ENABLED` | – |
| `FEATURE_HOMEPAGE_ENABLED` | `ALMOND_HOMEPAGE_ENABLED` | – |
| `ALLOWED_NPUBS` | `ALMOND_ALLOWED_NPUBS` | – |
| `AUTH_MAX_TTL_SECS` | `ALMOND_AUTH_MAX_TTL` | `86400` → `24h`; `0` is now rejected |
| `AUTH_CLOCK_SKEW_SECS` | `ALMOND_AUTH_CLOCK_SKEW` | `30` → `30s` |
| `AUTH_REQUIRE_SERVER_TAG` | `ALMOND_AUTH_REQUIRE_SERVER_TAG` | – |
| `MAX_CHUNK_SIZE_MB` | `ALMOND_CHUNK_MAX_SIZE` | `100` → `100MiB` |
| `CHUNK_CLEANUP_TIMEOUT_MINUTES` | `ALMOND_CHUNK_SESSION_TIMEOUT` | `30` → `30m` |
| `MAX_CHUNK_UPLOAD_SESSIONS` | `ALMOND_CHUNK_MAX_SESSIONS` | – |
| `MAX_CHUNK_UPLOAD_SESSIONS_PER_PUBKEY` | `ALMOND_CHUNK_MAX_SESSIONS_PER_PUBKEY` | – |
| `HLS_MIRROR_CONCURRENCY` | `ALMOND_HLS_MIRROR_CONCURRENCY` | – |
| `UPSTREAM_SERVERS` | `ALMOND_UPSTREAM_SERVERS` | – |
| `UPSTREAM_MODE` | `ALMOND_UPSTREAM_MODE` | `redirect_and_cache` is now spelled `redirect-and-cache` (old spelling still accepted) |
| `MAX_UPSTREAM_DOWNLOAD_SIZE_MB` | `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` | `100` → `100MiB` |
| `METRICS_BEARER_TOKEN` | `ALMOND_METRICS_TOKEN` | – |
| `SERVE_FILES_PATH` | `ALMOND_SERVE_FILES_PATH` | – |
| `SERVE_FILES_MANIFEST_DIR` | `ALMOND_SERVE_FILES_MANIFEST_DIR` | – |
| `SERVE_FILES_MANIFEST_NAME` | `ALMOND_SERVE_FILES_MANIFEST_NAME` | – |
| `SERVE_FILES_REFRESH_INTERVAL_SECS` | `ALMOND_SERVE_FILES_REFRESH_INTERVAL` | `3600` → `3600s` |
| `FEATURE_PAID_UPLOAD`, `FEATURE_PAID_MIRROR`, `FEATURE_PAID_DOWNLOAD` | `ALMOND_CASHU_PAID` | merged into one list: `FEATURE_PAID_UPLOAD=on` + `FEATURE_PAID_MIRROR=on` → `upload,mirror` |
| `CASHU_PRICE_PER_MB` | `ALMOND_CASHU_PRICE_PER_MIB` | – |
| `CASHU_ACCEPTED_MINTS` | `ALMOND_CASHU_MINT` | – (one mint URL) |
| `CASHU_WALLET_PATH` | `ALMOND_CASHU_WALLET_PATH` | – |
| `BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS` | `ALMOND_SERVER_LIST_CACHE_TTL` | `24` → `24h` |
| `FILTER_ALGORITHM` | `ALMOND_FILTER_ALGORITHM` | – |
| `DVM_ALLOWED_KINDS` | `ALMOND_DVM_KINDS` | – |
| `DVM_RELAYS` | `ALMOND_DVM_RELAYS` | – |
| `DVM_REFRESH_INTERVAL_MINS` | `ALMOND_DVM_REFRESH_INTERVAL` | `5` → `5m` |

`ALMOND_S3_*` names are unchanged. Values that were previously accepted
leniently (unknown enum values, invalid npubs, `AUTH_MAX_TTL_SECS=0`) now stop
startup with an error.

## HTTPS Configuration

### Automatic Self-Signed Certificates

With `ALMOND_TLS_SELF_SIGNED=true`, Almond generates a self-signed certificate when HTTPS is enabled and no certificate files are found:

```bash
ALMOND_TLS_ENABLED=true ALMOND_TLS_SELF_SIGNED=true cargo run
```

This will:
1. Generate a self-signed certificate (`cert.pem`) and private key (`key.pem`)
2. Start the server with HTTPS on the configured address
3. Accept connections from `localhost`, `127.0.0.1`, and `::1`

**Note:** Browsers will show a security warning for self-signed certificates. You'll need to manually trust the certificate or use it for development/testing only.

### Using Custom Certificates

To use your own certificates (e.g., from Let's Encrypt):

```bash
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_CERT=/path/to/cert.pem \
ALMOND_TLS_KEY=/path/to/key.pem \
cargo run
```

### Docker with HTTPS

Self-signed (auto-generated):
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_SELF_SIGNED=true \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

With custom certificates:
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -v /path/to/certs:/app/certs \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_CERT=/app/certs/cert.pem \
  -e ALMOND_TLS_KEY=/app/certs/key.pem \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

## Internals
- Completed filesystem blobs are stored below `ALMOND_STORAGE_PATH/uploads/` or `ALMOND_STORAGE_PATH/upstream-cache/` with the existing two-level SHA-256 hierarchy, e.g.
  ```bash
  ./files/uploads/5/3/53860ca3a463ad7170fe1f1e5b08bf4b66422c72b594a329e001a69e07f2e50e.mp4
  ```
- Startup indexes only completed-blob roots; `temp/`, `quarantine/`, and `reports/` are never treated as live blobs. Indexed age is recovered from modification time.
- Every accepted report event is persisted as `ALMOND_STORAGE_PATH/reports/<event-id>.json` (signed event plus `status`, `mode`, `action`, `requested`, `processed`). The record is written before any blob is removed and rewritten afterwards, so a record still showing `"status": "pending"` marks a report whose processing was interrupted.
- When starting `almond`, completed-blob roots are read into memory; filesystem changes outside Almond are not recognized until restart.

## Docker

### Building the Image

```bash
docker build -t almond .
```

### Running the Container

The standard image runs as the fixed, non-root numeric identity `10001:10001`.
This is an image contract, not a host-user name: ownership shown by `ls` is
resolved through the host's `/etc/passwd` and may display a different name.

For a Docker-managed named volume, no host-side ownership setup is needed:

```bash
docker volume create almond-files
docker run --rm -p 3000:3000 \
  -v almond-files:/app/files \
  ghcr.io/flox1an/almond:v0.5.0
```

For a host bind mount, prepare the directory with the image's numeric identity:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  ghcr.io/flox1an/almond:v0.5.0
```

Migrate an existing bind mount once before upgrading to an image using this
contract:

```bash
sudo chown -R 10001:10001 /data/almond
sudo find /data/almond -type d -exec chmod 0750 {} +
sudo find /data/almond -type f -exec chmod 0640 {} +
```

The default image user may be overridden for a host-specific deployment. Docker
does not change bind-mount ownership, so the chosen pair must own the host
directory. This is useful for a local operator account; it is not needed for
the fixed-image default.

```yaml
services:
  almond:
    image: ghcr.io/flox1an/almond:v0.5.0
    user: "${PUID:-10001}:${PGID:-10001}"
    ports:
      - "3000:3000"
    volumes:
      - /data/almond:/app/files
    environment:
      ALMOND_STORAGE_PATH: /app/files
```

```bash
PUID=$(id -u) PGID=$(id -g) docker compose up -d
```

The standard image never recursively changes host-volume ownership at startup.
For optional TLS auto-generation or Cashu payments, mount persistent state
separately and give it the same UID/GID:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond-state
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  -v /data/almond-state:/app/state \
  -e ALMOND_TLS_ENABLED=true \
  ghcr.io/flox1an/almond:v0.5.0
```

### FIPS-enabled Docker Image

This non-root identity contract applies to the standard image only. The FIPS
image currently requires root plus `NET_ADMIN` and `/dev/net/tun` because its
entrypoint configures the TUN interface, DNS, and iptables rules.

GitHub Actions also builds `ghcr.io/flox1an/almond-fips`, a variant that runs
FIPS, dnsmasq, and Almond in the same container. It can serve the same Almond
instance over normal HTTP port publishing and over the FIPS mesh at the same
time.

Run it with the privileges FIPS needs for the TUN interface:

```bash
docker run \
  --cap-add NET_ADMIN \
  --device /dev/net/tun:/dev/net/tun \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -p 3000:3000 \
  -p 2121:2121/udp \
  -v /path/to/files:/app/files \
  -e ALMOND_STORAGE_PATH=/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:3000 \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  -e ALMOND_UPSTREAM_SERVERS=https://npub1upstream....fips \
  ghcr.io/flox1an/almond-fips:main
```

FIPS needs the host TUN device mounted into the container:
`--device /dev/net/tun:/dev/net/tun`. The daemon creates a virtual IPv6 network
interface, usually `fips0`, on top of that device. `--cap-add NET_ADMIN` is
needed so the container can create and configure that interface and install the
small DNS/iptables rules used by the FIPS entrypoint. If `/dev/net/tun` does not
exist on the host, enable the kernel TUN module first, for example with
`sudo modprobe tun` on Linux hosts.

With `ALMOND_BIND_ADDR=0.0.0.0:3000`, Almond is reachable both through Docker's
published HTTP port and through FIPS on `http://<this-node-npub>.fips:3000`
from peered FIPS nodes. For HTTPS over FIPS, enable Almond's normal TLS settings
and use `https://<this-node-npub>.fips:3000`.

FIPS DNS is controlled independently:

- `FIPS_REWRITE_DNS=true` (default): container DNS points to dnsmasq, which
  sends `.fips` names to FIPS and everything else to the original Docker DNS.
- `FIPS_REWRITE_DNS=false`: FIPS still runs and can host Almond on `fips0`, but
  container-wide `.fips` DNS is not installed.
- `FIPS_HOSTS`: optional newline-separated aliases for `/etc/fips/hosts`, e.g.
  `my-upstream npub1...`; then `https://my-upstream.fips` resolves locally.

The FIPS image accepts the same Almond variables as the normal image, plus:

- `FIPS_NSEC`: FIPS node secret key, hex or `nsec1` (required unless mounting a
  full config and setting `FIPS_GENERATE_CONFIG=false`)
- `FIPS_PEER_NPUB`, `FIPS_PEER_ADDR`, `FIPS_PEER_ALIAS`, `FIPS_PEER_TRANSPORT`:
  optional direct peer configuration
- `FIPS_PEERS`: optional multi-peer list, one peer per line in the format
  `npub,addr[,alias[,transport]]`; when set, it overrides single-peer variables
- `FIPS_UDP_BIND`: UDP transport bind address (default: `0.0.0.0:2121`)
- `FIPS_TUN_NAME`, `FIPS_TUN_MTU`: TUN interface settings
- `FIPS_ISOLATE=true`: optional mesh-only mode that blocks non-FIPS egress on
  the physical container interface

## Hosting Almond over FIPS

The FIPS image can publish Almond in two ways at the same time:

- Normal HTTP(S): Docker publishes Almond on the host with `-p 3000:3000`.
- FIPS mesh HTTP(S): peered FIPS nodes reach the same Almond process through
  `fips0` at `http://<this-node-npub>.fips:3000`.

The `fips0` interface is backed by the host TUN device mounted with
`--device /dev/net/tun:/dev/net/tun`. Without that mount, FIPS cannot create the
mesh interface and the container will not be able to route `.fips` traffic.

Bind Almond to all interfaces so both paths work:

```bash
docker run \
  --cap-add NET_ADMIN \
  --device /dev/net/tun:/dev/net/tun \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -p 3000:3000 \
  -p 2121:2121/udp \
  -v /path/to/files:/app/files \
  -e ALMOND_STORAGE_PATH=/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:3000 \
  -e ALMOND_PUBLIC_URL=https://public.example.com \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1gateway... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  ghcr.io/flox1an/almond-fips:main
```

From the public internet, clients use `https://public.example.com`. From FIPS
peers, clients use:

```text
http://<this-node-npub>.fips:3000
```

For TLS inside FIPS, use Almond's normal HTTPS settings:

```bash
-e ALMOND_TLS_ENABLED=true \
-e ALMOND_TLS_CERT=/app/certs/cert.pem \
-e ALMOND_TLS_KEY=/app/certs/key.pem \
-v /path/to/certs:/app/certs
```

Then FIPS peers use `https://<this-node-npub>.fips:3000`. The certificate must
be valid for the hostname clients use, or clients must explicitly trust it.

Optional short names can be provided with `FIPS_HOSTS`, which writes
`/etc/fips/hosts` inside the container:

```bash
-e FIPS_HOSTS='my-almond npub1thisnode...'
```

Peers that also have that hosts mapping can use `http://my-almond.fips:3000`.
The canonical `<npub>.fips` name works without aliases.

### Minimal FIPS Service Settings

For normal operation, prefer the bundled Compose file. It contains the reusable
Docker settings FIPS always needs (`NET_ADMIN`, `/dev/net/tun`, IPv6 sysctl,
ports, storage volume, and DNS defaults), so the per-node configuration stays
small.

Create `.env.fips` from `.env.fips.example` and fill in only your node-specific
values:

```env
ALMOND_PUBLIC_URL=http://<this-node-npub>.fips:3000
ALMOND_UPSTREAM_MODE=proxy
ALMOND_UPSTREAM_SERVERS=

FIPS_NSEC=nsec1...
FIPS_PEER_NPUB=
FIPS_PEER_ADDR=
FIPS_PEER_ALIAS=gateway
FIPS_PEER_TRANSPORT=udp
FIPS_PEERS=
FIPS_HOSTS=
```

`FIPS_PEERS` is the recommended format for production because it allows multiple
gateways. Example:

```env
FIPS_PEERS=npub1aaa...,203.0.113.10:2121,gateway-a,udp
npub1bbb...,198.51.100.20:2121,gateway-b,udp
```

Then start the service:

```bash
docker compose --env-file .env.fips -f docker-compose.fips.yml up -d
```

The reusable Compose service is:

```yaml
services:
  almond-fips:
    image: ghcr.io/flox1an/almond-fips:main
    cap_add:
      - NET_ADMIN
    devices:
      - /dev/net/tun:/dev/net/tun
    sysctls:
      - net.ipv6.conf.all.disable_ipv6=0
    ports:
      - "3000:3000"
      - "2121:2121/udp"
    environment:
      ALMOND_BIND_ADDR: 0.0.0.0:3000
      ALMOND_PUBLIC_URL: http://<this-node-npub>.fips:3000
      ALMOND_STORAGE_PATH: /app/files
      ALMOND_UPLOAD_ACCESS: public
      ALMOND_MIRROR_ACCESS: public
      ALMOND_CUSTOM_ORIGIN_ACCESS: public
      ALMOND_UPSTREAM_MODE: proxy
      FIPS_NSEC: nsec1...
      FIPS_PEER_NPUB: npub1gateway...
      FIPS_PEER_ADDR: 203.0.113.10:2121
      FIPS_PEER_ALIAS: gateway
      FIPS_PEERS: |
        npub1aaa...,203.0.113.10:2121,gateway-a,udp
        npub1bbb...,198.51.100.20:2121,gateway-b,udp
      FIPS_REWRITE_DNS: "true"
    volumes:
      - almond-files:/app/files

volumes:
  almond-files:
```

Add FIPS upstreams by setting `ALMOND_UPSTREAM_SERVERS`:

```yaml
      ALMOND_UPSTREAM_SERVERS: https://npub1upstream....fips,https://media-cache.fips
      FIPS_HOSTS: |
        media-cache npub1upstream...
```

With multiple FIPS gateways (`FIPS_PEERS`), Almond keeps routing even if one
gateway is temporarily unavailable, which is especially useful for larger binary
transfers.

If the service should be public HTTP and FIPS at the same time, keep
`ALMOND_BIND_ADDR=0.0.0.0:3000` and publish `3000:3000`. If it should only be useful
inside the mesh, remove the `3000:3000` port mapping and keep the FIPS transport
port `2121/udp`.

### Coolify Deployment

Use Coolify's Docker Compose deployment mode for Almond FIPS. In Compose mode,
the compose file is the source of truth, so `cap_add`, `devices`, `sysctls`,
ports, volumes, and environment variables stay together in one place.

Before deploying, verify the Coolify target server supports TUN:

```bash
ls -l /dev/net/tun
```

If the device is missing on a Linux host, enable the kernel module:

```bash
sudo modprobe tun
```

Create a new Coolify resource with Docker Compose, paste the
`docker-compose.fips.yml` service, and set these variables in Coolify:

```env
ALMOND_PUBLIC_URL=https://your-public-domain.example
ALMOND_UPSTREAM_MODE=proxy
ALMOND_UPSTREAM_SERVERS=
FIPS_NSEC=nsec1...
FIPS_PEER_NPUB=npub1gateway...
FIPS_PEER_ADDR=203.0.113.10:2121
FIPS_PEER_ALIAS=gateway
FIPS_PEER_TRANSPORT=udp
FIPS_PEERS=
FIPS_HOSTS=
```

For public HTTP(S), assign the Coolify domain to the `almond-fips` service on
container port `3000`. Coolify's proxy can handle the normal web domain, while
the same container also serves FIPS peers on `http://<this-node-npub>.fips:3000`.

Keep the FIPS transport UDP port published:

```yaml
ports:
  - "2121:2121/udp"
```

If Coolify's UI is used in image-only mode instead of Compose mode, the same
runtime settings must go into Custom Docker Options:

```text
--cap-add NET_ADMIN --device /dev/net/tun:/dev/net/tun --sysctl net.ipv6.conf.all.disable_ipv6=0
```

Compose mode is easier because it also carries the UDP port, storage volume,
and `.fips` DNS defaults. If Coolify rejects `devices`, `cap_add`, or `sysctls`
on your hosting provider, FIPS cannot run inside that container. In that case,
run FIPS on the host or choose a VPS/bare-metal server where Docker can access
`/dev/net/tun`.

## Using FIPS Upstreams

Almond can use Blossom upstreams that are reachable only inside the FIPS mesh.
Use the FIPS image and configure upstreams with `.fips` hostnames:

```bash
docker run \
  --cap-add NET_ADMIN \
  --device /dev/net/tun:/dev/net/tun \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -p 3000:3000 \
  -p 2121:2121/udp \
  -v /path/to/files:/app/files \
  -e ALMOND_STORAGE_PATH=/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:3000 \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1gateway... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  -e FIPS_REWRITE_DNS=true \
  -e ALMOND_UPSTREAM_MODE=proxy \
  -e ALMOND_UPSTREAM_SERVERS=https://npub1upstream....fips \
  ghcr.io/flox1an/almond-fips:main
```

`FIPS_REWRITE_DNS=true` is the default and is needed when Almond should resolve
`.fips` upstream names. dnsmasq sends `.fips` DNS queries to the FIPS daemon and
forwards normal DNS to Docker's original resolver.

For friendlier upstream names, provide aliases:

```bash
-e FIPS_HOSTS='media-cache npub1upstream...'
-e ALMOND_UPSTREAM_SERVERS=https://media-cache.fips
```

Custom upstream hints work the same way when enabled:

```bash
-e ALMOND_CUSTOM_ORIGIN_ACCESS=public
```

Then requests may pass `?xs=https://media-cache.fips` or
`?origin=https://media-cache.fips`. Almond keeps SSRF protection enabled: normal
private and local addresses stay blocked, while `.fips` hostnames resolving to
FIPS overlay IPv6 addresses are allowed for upstream fetching.

### Volume Mounting

The `/app/files` directory in the container is used for file storage. Mount a host directory to persist files:

```bash
docker run -p 3000:3000 -v /host/path:/app/files almond
```

## Local Blossom Cache

Almond can be configured as a [Local Blossom Cache](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md) — a local proxy that caches blobs from remote Blossom servers on `127.0.0.1:24242`.

Clients request blobs via `GET /<sha256>` with `?xs=` (server hints) and `?as=` (author pubkey) query parameters. If the blob isn't cached locally, Almond fetches it from the hinted servers (or the author's BUD-03 server list), caches it, and returns it to the client. Uploads and mirrors are disabled since the cache is populated entirely through proxying.

### Configuration

```dotenv
ALMOND_BIND_ADDR=127.0.0.1:24242
ALMOND_PUBLIC_URL=http://127.0.0.1:24242
ALMOND_UPLOAD_ACCESS=off
ALMOND_MIRROR_ACCESS=off
ALMOND_LIST_ENABLED=true
ALMOND_HOMEPAGE_ENABLED=true
ALMOND_CUSTOM_ORIGIN_ACCESS=public
ALMOND_UPSTREAM_MODE=proxy
ALMOND_STORAGE_MAX_SIZE=5000MiB
ALMOND_UPSTREAM_CACHE_TTL=30d
```

### Docker

```bash
docker run -p 24242:24242 \
  -v /path/to/cache:/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:24242 \
  -e ALMOND_PUBLIC_URL=http://127.0.0.1:24242 \
  -e ALMOND_UPLOAD_ACCESS=off \
  -e ALMOND_MIRROR_ACCESS=off \
  -e ALMOND_CUSTOM_ORIGIN_ACCESS=public \
  -e ALMOND_UPSTREAM_MODE=proxy \
  -e ALMOND_STORAGE_MAX_SIZE=5000MiB \
  -e ALMOND_UPSTREAM_CACHE_TTL=30d \
  ghcr.io/flox1an/almond
```

### How it works

1. Client requests `GET /abc123...def.jpg?xs=cdn.example.com&as=<pubkey>`
2. Almond checks the local cache
3. If cached, returns the blob immediately
4. If not cached, tries `xs` server hints first, then fetches the author's BUD-03 server list (kind:10063) from `as` hints
5. Caches the blob locally and returns it to the client
6. Returns `404` if the blob can't be found on any hinted server

Cache eviction is automatic — expired entries are removed by `ALMOND_UPSTREAM_CACHE_TTL`, and capacity pressure evicts the oldest cache entries first.

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
