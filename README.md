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
- `GET /list` / `GET /list/<pubkey>` - List stored files with BUD-12 cursor pagination (`?limit=100&cursor=<last_sha256>`). Optional `?since=` and `?until=` filters are supported but should not be used for pagination.
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
- `CLEANUP_INTERVAL_SECS`: Interval for lightweight cleanup checks in seconds (default: 30). Full storage cleanup runs when changes are pending, plus an occasional full scan.
- `MAX_FILE_AGE_DAYS`: Maximum age of files in days, 0 for no limit (default: 0)

### Upstream Configuration
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback (optional)
- `UPSTREAM_MODE`: How to handle upstream requests (default: `proxy`)
  - `proxy`: Stream from upstream while saving locally. Client receives data immediately while the file is cached.
  - `redirect`: Issue 302 redirect to upstream. No local caching. Reduces bandwidth/CPU on the Almond server.
  - `redirect_and_cache`: Issue 302 redirect to upstream, but also download in the background for future requests.
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
- `FEATURE_P2P_SERVE_ENABLED`: Enable Hashtree P2P serving for locally stored blobs only (default: false)

**Note:** Web of Trust (WOT) is automatically enabled when any feature is set to `wot` mode. WOT is built from your follows (specified in `ALLOWED_NPUBS`) using a 2-hop graph from Nostr relays.

### Hashtree P2P Serving

Almond can optionally join the Hashtree WebRTC mesh as a serving node. This mode only exports blobs that already exist in Almond's local `STORAGE_PATH` index. It does not use P2P as an upstream source for HTTP requests, and P2P misses do not trigger Blossom upstream fetches.

- `FEATURE_P2P_SERVE_ENABLED`: Set to `true` to start the P2P serving worker (default: false)
- `P2P_NSEC`: Nostr secret key used for Hashtree mesh signaling (required when P2P serving is enabled)
- `P2P_RELAYS`: Comma-separated Nostr relays for Hashtree signaling (default: `wss://temp.iris.to,wss://relay.damus.io`)
- `P2P_STUN_SERVERS`: Comma-separated STUN servers for WebRTC ICE (defaults to Hashtree/WebRTC public STUN servers)
- `P2P_REQUEST_TIMEOUT_MS`: Mesh request timeout used by the local store runtime (default: 10000)
- `P2P_HELLO_INTERVAL_MS`: Interval for announcing this Almond node to mesh peers (default: 30000)
- `P2P_DEBUG`: Enable verbose Hashtree P2P logging (default: false)

Example:

```bash
FEATURE_P2P_SERVE_ENABLED=true \
P2P_NSEC=nsec1... \
P2P_RELAYS=wss://temp.iris.to,wss://relay.damus.io \
cargo run
```

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

### FIPS-enabled Docker Image

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
  -e STORAGE_PATH=/app/files \
  -e BIND_ADDR=0.0.0.0:3000 \
  -e PUBLIC_URL=https://your-domain.com \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  -e UPSTREAM_SERVERS=https://npub1upstream....fips \
  ghcr.io/flox1an/almond-fips:main
```

FIPS needs the host TUN device mounted into the container:
`--device /dev/net/tun:/dev/net/tun`. The daemon creates a virtual IPv6 network
interface, usually `fips0`, on top of that device. `--cap-add NET_ADMIN` is
needed so the container can create and configure that interface and install the
small DNS/iptables rules used by the FIPS entrypoint. If `/dev/net/tun` does not
exist on the host, enable the kernel TUN module first, for example with
`sudo modprobe tun` on Linux hosts.

With `BIND_ADDR=0.0.0.0:3000`, Almond is reachable both through Docker's
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
  -e STORAGE_PATH=/app/files \
  -e BIND_ADDR=0.0.0.0:3000 \
  -e PUBLIC_URL=https://public.example.com \
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
-e ENABLE_HTTPS=true \
-e TLS_CERT_PATH=/app/certs/cert.pem \
-e TLS_KEY_PATH=/app/certs/key.pem \
-e TLS_AUTO_GENERATE=false \
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
PUBLIC_URL=http://<this-node-npub>.fips:3000
UPSTREAM_MODE=proxy
UPSTREAM_SERVERS=

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
      BIND_ADDR: 0.0.0.0:3000
      PUBLIC_URL: http://<this-node-npub>.fips:3000
      STORAGE_PATH: /app/files
      FEATURE_UPLOAD_ENABLED: public
      FEATURE_MIRROR_ENABLED: public
      FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED: public
      UPSTREAM_MODE: proxy
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

Add FIPS upstreams by setting `UPSTREAM_SERVERS`:

```yaml
      UPSTREAM_SERVERS: https://npub1upstream....fips,https://media-cache.fips
      FIPS_HOSTS: |
        media-cache npub1upstream...
```

With multiple FIPS gateways (`FIPS_PEERS`), Almond keeps routing even if one
gateway is temporarily unavailable, which is especially useful for larger binary
transfers.

If the service should be public HTTP and FIPS at the same time, keep
`BIND_ADDR=0.0.0.0:3000` and publish `3000:3000`. If it should only be useful
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
PUBLIC_URL=https://your-public-domain.example
UPSTREAM_MODE=proxy
UPSTREAM_SERVERS=
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
  -e STORAGE_PATH=/app/files \
  -e BIND_ADDR=0.0.0.0:3000 \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1gateway... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  -e FIPS_REWRITE_DNS=true \
  -e UPSTREAM_MODE=proxy \
  -e UPSTREAM_SERVERS=https://npub1upstream....fips \
  ghcr.io/flox1an/almond-fips:main
```

`FIPS_REWRITE_DNS=true` is the default and is needed when Almond should resolve
`.fips` upstream names. dnsmasq sends `.fips` DNS queries to the FIPS daemon and
forwards normal DNS to Docker's original resolver.

For friendlier upstream names, provide aliases:

```bash
-e FIPS_HOSTS='media-cache npub1upstream...'
-e UPSTREAM_SERVERS=https://media-cache.fips
```

Custom upstream hints work the same way when enabled:

```bash
-e FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=public
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
