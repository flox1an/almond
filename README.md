```
        .__                             .___
 _____  |  |   _____   ____   ____    __| _/
 \__  \ |  |  /     \ /  _ \ /    \  / __ | 
  / __ \|  |_|  Y Y  (  <_> )   |  \/ /_/ | 
 (____  /____/__|_|  /\____/|___|  /\____ | 
      \/           \/            \/      \/  
```

**Almond** (Any Large Media ON Demand) is a [Blossom](https://github.com/hzrd149/blossom)
media server for Nostr. It stores blobs on the local filesystem or in an
S3-compatible bucket, expires them automatically, and can cache blobs from other
Blossom servers. Writes are authorized with Nostr signatures: open to everyone,
limited to an npub allowlist, to a web of trust, or to DVMs.

1. [Features](#features)
2. [Deployment variants](#deployment-variants)
3. [Quick start](#quick-start)
4. [How it works](#how-it-works)
5. [Configuration](#configuration)
6. [API](#api)
7. [Deployment guides](#deployment-guides) — Docker, HTTPS, S3, Local Blossom Cache, FIPS, systemd
8. [Embedding as a library](#embedding-as-a-library)
9. [Migrating from pre-0.5 names](#migrating-from-pre-05-names)
10. [Development](#development)

## Features

- **Blossom API**: upload (including resumable chunked uploads), download, mirror,
  list, delete and report. Implements BUD-01, BUD-02, BUD-04, BUD-06, BUD-07,
  BUD-09, BUD-11 and BUD-12.
- **Temporary storage**: filesystem only, no database. Age limits, size and count
  caps, oldest-first eviction. Optional S3-compatible bucket for uploads.
- **Access control**: every write endpoint is `off`, `public`, `wot` (web of
  trust) or `dvm`, plus an optional npub allowlist. Optional Cashu payments per
  operation.
- **Upstream caching**: blobs missing locally are fetched from configured
  upstream servers or from client hints (`?xs=`, `?as=`, `?origin=`), then proxied,
  redirected or cached.
- **Media**: range requests and long-lived caching headers for video players;
  mirroring an HLS playlist also mirrors its segments.
- **Operations**: Prometheus metrics, BUD-11 hash filter, report quarantine,
  static file serving, homepage and config editor, built-in TLS.
- **Packaging**: single binary, Docker image, FIPS mesh image, and a Rust library
  for Axum applications.

## Deployment variants

Pick the variant closest to your goal; the settings combine freely (for example
a personal server with S3 storage that also caches upstream blobs).

| Variant | Purpose | Key settings | Details |
|---|---|---|---|
| Personal server | Only you or a few npubs may upload | `ALMOND_ALLOWED_NPUBS` | [Access control](#access-control) |
| Community server | Uploads for your web of trust | `ALMOND_UPLOAD_ACCESS=wot`, `ALMOND_ALLOWED_NPUBS` as seed | [Access control](#access-control) |
| Public temporary host | Anyone uploads, content expires | `ALMOND_UPLOAD_MAX_AGE`, `ALMOND_STORAGE_MAX_SIZE` | [Retention](#retention-and-eviction) |
| Paid host | Uploads, mirrors or downloads cost sats | `ALMOND_CASHU_PAID`, `ALMOND_CASHU_MINT` | [Payments](#payments-cashu--bud-07) |
| Caching edge | Serves and caches blobs from other servers | `ALMOND_UPSTREAM_SERVERS`, `ALMOND_UPSTREAM_MODE` | [Upstream lookups](#upstream-lookups) |
| Local Blossom Cache | Per-device cache on `127.0.0.1:24242` | uploads off, `ALMOND_CUSTOM_ORIGIN_ACCESS=public` | [Guide](#local-blossom-cache) |
| S3-backed server | Uploads stored in a bucket, served by Almond | `ALMOND_S3_*` | [S3 storage](#s3-storage) |
| FIPS mesh node | Serve or fetch blobs over the FIPS mesh | `almond-fips` image, `FIPS_*` | [Guide](#fips-mesh-image) |
| Embedded | Blossom routes inside your own Axum app | Rust library | [Guide](#embedding-as-a-library) |

## Quick start

With Docker:

```bash
docker volume create almond-files
docker run --rm -p 3000:3000 \
  -v almond-files:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

From source:

```bash
cp .env.example .env    # optional, edit as needed
cargo run --release     # listens on 127.0.0.1:3000
```

Almond is now at `http://localhost:3000`. Out of the box anyone with a valid
Nostr signature may upload and mirror, and nothing expires. Before exposing it,
set at least `ALMOND_PUBLIC_URL` and either `ALMOND_ALLOWED_NPUBS` or a retention
limit:

```dotenv
ALMOND_PUBLIC_URL=https://blossom.example.com
ALMOND_ALLOWED_NPUBS=npub1...
```

## How it works

### Blobs and storage

Blobs are immutable and addressed by their SHA-256. Each stored blob has one of
two origins, which decides where it lives and how it expires:

- **Upload**: authorized uploads, explicit mirrors and HLS mirrors.
- **Upstream cache**: copies filled transparently from upstream servers.

`ALMOND_STORAGE_PATH` (default `./files`) contains these managed directories:

| Directory | Contents |
|---|---|
| `uploads/` | Upload-origin blobs (unless S3 is configured) |
| `upstream-cache/` | Upstream-cache blobs |
| `temp/` | Incomplete uploads and in-flight downloads; never indexed |
| `quarantine/` | BUD-09 quarantined blobs; never indexed |
| `reports/` | Report audit records, one JSON per event; never indexed |

Blob files use a two-level hash hierarchy, e.g.
`./files/uploads/5/3/53860ca3a463ad7170fe1f1e5b08bf4b66422c72b594a329e001a69e07f2e50e.mp4`.
Keep the storage root on one filesystem.

At startup Almond reads the completed-blob directories into an in-memory index
and recovers each blob's age from its modification time. Changes made outside
Almond are not seen until restart. Legacy `<hex>/<hex>/<hash>` trees from older
versions are moved into `uploads/` by rename.

### Retention and eviction

A cleanup job runs every `ALMOND_CLEANUP_INTERVAL` (default `30s`) and applies:

- **Upload age**: uploads older than `ALMOND_UPLOAD_MAX_AGE` are removed (`0`, the
  default, disables this). A client may shorten its own blob's lifetime with the
  Almond-specific `X-Expiration` header (Unix timestamp), never extend it.
- **Cache age**: upstream-cache copies older than `ALMOND_UPSTREAM_CACHE_TTL`
  (default `1d`) are removed. Serving a cached blob does not refresh its age.
- **Capacity**: above `ALMOND_STORAGE_MAX_SIZE` or `ALMOND_STORAGE_MAX_FILES`, the
  oldest upstream-cache entries are evicted first, then the oldest uploads.

Two admission limits apply before anything is stored: `ALMOND_BLOB_MAX_SIZE`
(default `500MiB` per blob) and `ALMOND_STORAGE_MIN_FREE` (default `256MiB`;
below it uploads get `507`).

Clients see an upload's deadline in the `Sunset` header and the descriptor's
`expiration` field; see [Blob delivery](#blob-delivery).

### Access control

Every write carries a Nostr kind `24242` authorization event in the
`Authorization: Nostr <base64>` header. Each write endpoint has its own access
mode:

| Mode | Who is allowed |
|---|---|
| `off` | Nobody; the endpoint is disabled |
| `public` | Any valid signature; only allowlisted npubs if `ALMOND_ALLOWED_NPUBS` is set |
| `wot` | Allowlisted npubs and their web of trust |
| `dvm` | Allowlisted npubs and DVMs announcing one of `ALMOND_DVM_KINDS` on `ALMOND_DVM_RELAYS` |

| Endpoint | Setting | Default |
|---|---|---|
| Upload | `ALMOND_UPLOAD_ACCESS` | `public` |
| Mirror | `ALMOND_MIRROR_ACCESS` | `public` |
| Report (BUD-09) | `ALMOND_REPORT_ACCESS` | `off` |
| Client upstream hints (`?origin=`, `?xs=`, `?as=`) | `ALMOND_CUSTOM_ORIGIN_ACCESS` | `off` |

- **Web of trust** is built automatically when any mode is `wot`: a 2-hop follow
  graph of the `ALMOND_ALLOWED_NPUBS` accounts, fetched from Nostr relays. For
  client hints, `wot` checks the `?as=` author against it.
- **Delete** is never open: only npubs in `ALMOND_ALLOWED_NPUBS` may delete, and
  only with a signed, single-use BUD-11 authorization bound to the blob hash.
  Without an allowlist, delete is refused.
- **Reports** either quarantine the blob (`ALMOND_REPORT_ACTION=quarantine`, the
  default; the hash can then never be uploaded again) or delete the current copy
  (`delete`).
- `/list` and the homepage are switched with booleans (`ALMOND_LIST_ENABLED`,
  `ALMOND_HOMEPAGE_ENABLED`), not access modes.

### Payments (Cashu / BUD-07)

`ALMOND_CASHU_PAID` lists the operations that cost sats (`upload`, `mirror`,
`download`), priced per MiB by `ALMOND_CASHU_PRICE_PER_MIB`. Tokens must come
from the single mint in `ALMOND_CASHU_MINT` and are received into the wallet at
`ALMOND_CASHU_WALLET_PATH`. Payments apply on top of the access mode. Builds
without the `cashu` feature refuse to start with paid operations configured.

### Upstream lookups

When a requested hash is not stored locally, Almond looks for it in this order:

1. `?origin=` from the request
2. `?xs=` server hints from the request
3. `ALMOND_UPSTREAM_SERVERS`
4. the BUD-03 server list (kind `10063`) of the `?as=` author

Steps 1, 2 and 4 require `ALMOND_CUSTOM_ORIGIN_ACCESS` other than `off`. Author
server lists are cached for `ALMOND_SERVER_LIST_CACHE_TTL` (default `24h`).

`ALMOND_UPSTREAM_MODE` decides what happens with a hit:

| Mode | Behavior |
|---|---|
| `proxy` (default) | Streams to the client while saving a cache copy |
| `redirect` | Returns `302` to the upstream; nothing is cached; saves bandwidth and CPU |
| `redirect-and-cache` | Returns `302` and fills the cache in the background |

Blobs larger than `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` (default `100MiB`) are still
served, but proxied without being cached. Upstream fetches are SSRF-protected:
private and local addresses are blocked.

### S3 storage

When all four `ALMOND_S3_ENDPOINT`, `ALMOND_S3_BUCKET`, `ALMOND_S3_ACCESS_KEY_ID`
and `ALMOND_S3_SECRET_ACCESS_KEY` are set, new uploads and explicit mirrors are
stored in the bucket. Supplying only some of them fails startup.

- S3 blobs stay Upload-origin for retention and collision precedence; upstream
  cache fills stay local.
- Almond keeps proxying every blob response; clients never talk to the bucket.
- `ALMOND_S3_REGION` (default `auto`, which suits R2) sets the SigV4 signing
  region. `ALMOND_S3_FORCE_PATH_STYLE=true` addresses buckets as
  `<endpoint>/<bucket>` instead of `<bucket>.<endpoint>`; MinIO and Garage need it.

If the bucket is unreachable at startup, Almond still starts and serves local
blobs. Until the bucket answers, requests for unknown hashes and all uploads
return `503`, and nothing is written to local disk. A background task retries the
listing with backoff (1 s, doubling to 5 min), merges the bucket into the index
(local copies win) and resumes S3 use. Embedders can read `AppState::s3_status()`
(`Available`, or `Unavailable { error, last_attempt }`).

Provider examples are in [S3 providers](#s3-providers).

## Configuration

### Sources and precedence

Every setting can come from four sources, highest precedence first:

1. command-line flag (`--upload-access wot`)
2. process environment (`ALMOND_UPLOAD_ACCESS=wot`)
3. config file (dotenv format, same `ALMOND_*` names)
4. built-in default

```bash
almond --config /etc/almond/almond.env --bind-addr 0.0.0.0:3000
```

- **Config file:** `--config <FILE>` (or `ALMOND_CONFIG`) loads a dotenv file;
  without it, `./.env` is loaded when present. Values in the file never override
  real environment variables. [`.env.example`](.env.example) documents every
  setting and is a good starting point.
- **Name mapping:** env name = `ALMOND_` + flag name upper-cased with `-` → `_`
  (`--chunk-max-sessions-per-pubkey` ↔ `ALMOND_CHUNK_MAX_SESSIONS_PER_PUBKEY`).
  An empty value (`ALMOND_X=`) behaves exactly like an unset one.
- **Secrets** (S3 keys, metrics token) belong in the environment or config file:
  command-line arguments are visible to other users in `ps`.
- **Strict parsing:** unknown enum values, invalid booleans, invalid npubs and bad
  durations or sizes stop startup instead of falling back to a default.
- `almond --help` lists every flag with its env name and default;
  `almond --version` prints the version.
- **Logging:** `RUST_LOG` (e.g. `warn`, `almond=debug`; default `info`), also
  honored in the config file.

### Value formats

| Type | Format |
|---|---|
| Duration | `<n><unit>` with `s`, `m`, `h`, `d` (`30s`, `5m`, `24h`, `7d`); bare `0` allowed, other numbers need a unit |
| Size | `<n><unit>` with `B`, `KiB`, `MiB`, `GiB`, `TiB` (case-insensitive, IEC only; `MB`/`GB` are rejected); bare `0` allowed |
| Boolean | `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`; on the CLI a bare `--tls-enabled` means true, `--list-enabled=false` disables |
| List | Comma-separated; on the CLI also by repeating the flag |
| Access mode | `off`, `public`, `wot`, `dvm` |

### Reference

**Network and TLS**

| Setting | Default | Description |
|---|---|---|
| `ALMOND_BIND_ADDR` | `127.0.0.1:3000` | Listen address; use `0.0.0.0:3000` in a container or behind a proxy |
| `ALMOND_PUBLIC_URL` | `http://127.0.0.1:3000` (`https://` with TLS) | Public origin used in blob descriptors; no trailing slash |
| `ALMOND_CORS_ORIGINS` | – | Browser origins allowed to read discovery and metrics responses; blob `GET`/`HEAD` is always public |
| `ALMOND_TLS_ENABLED` | `false` | Terminate TLS in Almond; leave off behind a TLS proxy |
| `ALMOND_TLS_CERT` / `ALMOND_TLS_KEY` | `./cert.pem` / `./key.pem` | Certificate and private key |
| `ALMOND_TLS_SELF_SIGNED` | `false` | Generate a self-signed pair when the files are missing; development only |

**Storage and retention** (see [Retention and eviction](#retention-and-eviction))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_STORAGE_PATH` | `./files` | Storage root |
| `ALMOND_STORAGE_MAX_SIZE` | `0` (unlimited) | Total size of all completed blobs |
| `ALMOND_STORAGE_MAX_FILES` | `0` (unlimited) | Total number of completed blobs |
| `ALMOND_STORAGE_MIN_FREE` | `256MiB` | Uploads get `507` below this free disk space |
| `ALMOND_BLOB_MAX_SIZE` | `500MiB` | Per-blob size limit |
| `ALMOND_CLEANUP_INTERVAL` | `30s` | Cleanup cadence; must be greater than zero |
| `ALMOND_UPLOAD_MAX_AGE` | `0` (off) | Maximum age of uploads, explicit and HLS mirrors |
| `ALMOND_UPSTREAM_CACHE_TTL` | `1d` | Maximum age of upstream-cache copies; `0` disables |

**S3** (see [S3 storage](#s3-storage))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_S3_ENDPOINT`, `ALMOND_S3_BUCKET` | – | Bucket location; all four S3 credentials and location values are required together |
| `ALMOND_S3_ACCESS_KEY_ID`, `ALMOND_S3_SECRET_ACCESS_KEY` | – | Credentials |
| `ALMOND_S3_REGION` | `auto` | SigV4 signing region |
| `ALMOND_S3_FORCE_PATH_STYLE` | `false` | Path-style bucket addressing (MinIO, Garage) |

**Access and authorization** (see [Access control](#access-control))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_UPLOAD_ACCESS` | `public` | Upload access mode |
| `ALMOND_MIRROR_ACCESS` | `public` | Mirror access mode |
| `ALMOND_REPORT_ACCESS` | `off` | BUD-09 report access mode |
| `ALMOND_REPORT_ACTION` | `quarantine` | `quarantine` or `delete` |
| `ALMOND_CUSTOM_ORIGIN_ACCESS` | `off` | Access mode for client upstream hints |
| `ALMOND_LIST_ENABLED` | `true` | Serve `/list` |
| `ALMOND_HOMEPAGE_ENABLED` | `true` | Serve the homepage |
| `ALMOND_ALLOWED_NPUBS` | – | Allowlist; also the web-of-trust seed; required for delete |
| `ALMOND_AUTH_MAX_TTL` | `24h` | Maximum authorization lifetime; must be greater than zero |
| `ALMOND_AUTH_CLOCK_SKEW` | `30s` | Tolerated client clock skew |
| `ALMOND_AUTH_REQUIRE_SERVER_TAG` | `false` | Require a matching `server` tag in authorization events |
| `ALMOND_DVM_KINDS` | – | DVM kinds admitted in `dvm` mode (e.g. `5207`); required when upload or mirror access is `dvm` |
| `ALMOND_DVM_RELAYS` | – | Relays queried for DVM announcements |
| `ALMOND_DVM_REFRESH_INTERVAL` | `5m` | DVM announcement refresh interval |

**Uploads and mirrors**

| Setting | Default | Description |
|---|---|---|
| `ALMOND_CHUNK_MAX_SIZE` | `100MiB` | Maximum chunk size for resumable uploads; at most `ALMOND_BLOB_MAX_SIZE` |
| `ALMOND_CHUNK_SESSION_TIMEOUT` | `30m` | Abandoned resumable uploads are cleaned up after this |
| `ALMOND_CHUNK_MAX_SESSIONS` | `128` | Concurrent resumable upload sessions |
| `ALMOND_CHUNK_MAX_SESSIONS_PER_PUBKEY` | `8` | Concurrent sessions per pubkey |
| `ALMOND_HLS_MIRROR_CONCURRENCY` | `4` | Parallel segment fetches when mirroring an HLS playlist |

**Upstream** (see [Upstream lookups](#upstream-lookups))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_UPSTREAM_SERVERS` | – | Trusted upstream Blossom origins |
| `ALMOND_UPSTREAM_MODE` | `proxy` | `proxy`, `redirect` or `redirect-and-cache` |
| `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` | `100MiB` | Larger blobs are proxied without caching |
| `ALMOND_SERVER_LIST_CACHE_TTL` | `24h` | Cache time for authors' BUD-03 server lists |

**Payments** (see [Payments](#payments-cashu--bud-07))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_CASHU_PAID` | – | Paid operations: `upload`, `mirror`, `download` |
| `ALMOND_CASHU_PRICE_PER_MIB` | `1` | Sats per MiB |
| `ALMOND_CASHU_MINT` | – | The one accepted mint URL; required when payments are on |
| `ALMOND_CASHU_WALLET_PATH` | `./cashu_wallet.db` | Wallet database |

**Operations**

| Setting | Default | Description |
|---|---|---|
| `ALMOND_METRICS_TOKEN` | – | Bearer token for `/metrics`; the endpoint returns `404` while unset |
| `ALMOND_FILTER_ALGORITHM` | `binary-fuse-16` | BUD-11 filter: `bloom`, `binary-fuse-8`, `binary-fuse-16`, `binary-fuse-32` |
| `ALMOND_SERVE_FILES_PATH` | – | Directory of static files served alongside blobs |
| `ALMOND_SERVE_FILES_MANIFEST_DIR` | `/tmp` | Manifest location when the served directory is not writable |
| `ALMOND_SERVE_FILES_MANIFEST_NAME` | `manifest-sha256.txt` | SHA-256 manifest file name |
| `ALMOND_SERVE_FILES_REFRESH_INTERVAL` | `1h` | Manifest rebuild interval |

## API

### Endpoints

Blossom endpoints:

| Method and path | Purpose |
|---|---|
| `PUT /upload` | Upload a blob (BUD-02) |
| `HEAD /upload` | Check whether an upload would be admitted (BUD-06) |
| `PATCH /upload` | Resumable upload (Almond extension) |
| `GET /<sha256>[.ext]` | Download a blob; the extension is ignored for lookup (BUD-01) |
| `HEAD /<sha256>[.ext]` | Blob metadata (BUD-01) |
| `GET /list`, `GET /list/<pubkey>` | List blobs with BUD-12 cursor pagination (`?limit=100&cursor=<last_sha256>`); `?since=`/`?until=` filter but should not be used for pagination |
| `PUT /mirror` | Mirror a blob from another server (BUD-04) |
| `PUT /report` | Report a blob (BUD-09) |
| `DELETE /<sha256>` | Delete a blob with a BUD-11 authorization (BUD-12) |

Almond pages and diagnostics:

| Method and path | Purpose |
|---|---|
| `GET /` | Homepage |
| `GET /config` | Config editor |
| `GET /filter` | BUD-11 filter over all stored hashes |
| `GET /metrics` | Prometheus metrics; requires `Authorization: Bearer <ALMOND_METRICS_TOKEN>` |
| `GET /_upstream` | Configured upstream servers |
| `GET /_wot` | Bloom filter of the web of trust (`?format=json\|bin`, `?fp=`, `?test=<hex pubkey>`) |

`/_upstream` response:

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

### Blob delivery

Blobs are immutable and content-addressed, so `GET`/`HEAD` serve them with
aggressive, safe caching:

- **`ETag`**: the quoted SHA-256 of the blob, a strong validator that never needs
  to touch the file.
- **`If-None-Match`**: a match returns `304 Not Modified`. Handles tag lists, weak
  (`W/"..."`) tags and `*`.
- **`Cache-Control: public, max-age=31536000, immutable`** plus a one-year `Expires`.
- **Range requests**: all three RFC 9110 forms, `bytes=START-END`, `bytes=START-`
  and the suffix form `bytes=-N` (used by MP4 players probing the trailing `moov`
  atom). An `END` past EOF is clamped rather than rejected.
- **`416 Range Not Satisfiable`** with `Content-Range: bytes */SIZE` when the range
  lies entirely outside the blob.
- **`If-Range`**: a stale validator falls back to the full `200` body.
- Multi-range requests (`bytes=0-9,20-29`) get the full `200` representation;
  `multipart/byteranges` is not implemented.

**`Sunset`.** When an upload has a finite retention deadline, `GET` and `HEAD`
(including extension variants, `206` and `304`) return the RFC 8594 / BUD-01
`Sunset` header, exposed to browsers via `Access-Control-Expose-Headers`.

- The value is the deadline the cleanup job uses: the earlier of the persisted
  expiration and `created_at + ALMOND_UPLOAD_MAX_AGE` under the current config, so
  a changed limit shows up immediately. Uploads stored before v0.4.24 may carry a
  persisted deadline that a raised limit does not extend.
- `X-Expiration` can only shorten retention; the descriptor's `expiration` field
  carries the same deadline as `Sunset`.
- Upstream-cache copies and upstream redirects carry no `Sunset`: the URL keeps
  resolving via upstream.
- `Sunset` is advisory; an expired blob is served until cleanup removes it.
- Blob responses stay `immutable` for a year, so a CDN in front of Almond may hold
  stale `Sunset` values or deleted blobs until purged.
- Report-quarantined hashes cannot be re-uploaded.

**`/filter`** is rendered once per index change and served from cache, so it also
carries an `ETag` and answers `If-None-Match` with `304`. Its `timestamp` field is
the render time rather than the request time, which keeps the body byte-stable.

**Reports.** Every accepted report is persisted as
`ALMOND_STORAGE_PATH/reports/<event-id>.json` (signed event plus `status`, `mode`,
`action`, `requested`, `processed`). The record is written before any blob is
removed and rewritten afterwards, so `"status": "pending"` marks a report whose
processing was interrupted.

## Deployment guides

### Binary and systemd

Almond always runs in the foreground (no daemon mode); use a service manager. A
minimal systemd unit with install instructions is in
[`contrib/almond.service`](contrib/almond.service).

### Docker

The image `ghcr.io/flox1an/almond` sets container defaults: bind `0.0.0.0:3000`,
storage `/app/files`, at most 1,000,000 files, cleanup every `60s`, and TLS and
wallet files under `/app/state`. Build it locally with `docker build -t almond .`.

The image runs as the fixed, non-root numeric identity `10001:10001`. This is an
image contract, not a host-user name: `ls` resolves it through the host's
`/etc/passwd` and may show a different name. The image never changes host-volume
ownership.

A Docker-managed named volume needs no preparation:

```bash
docker volume create almond-files
docker run --rm -p 3000:3000 \
  -v almond-files:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

A host bind mount must be owned by the image identity:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

Migrate an existing bind mount once before upgrading to an image with this
contract:

```bash
sudo chown -R 10001:10001 /data/almond
sudo find /data/almond -type d -exec chmod 0750 {} +
sudo find /data/almond -type f -exec chmod 0640 {} +
```

To run as a different user (for example your own account), override it; the
chosen pair must own the host directory:

```yaml
services:
  almond:
    image: ghcr.io/flox1an/almond:v0.5.1
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

For TLS auto-generation or Cashu payments, mount persistent state separately
with the same UID/GID:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond-state
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  -v /data/almond-state:/app/state \
  -e ALMOND_TLS_ENABLED=true \
  ghcr.io/flox1an/almond:v0.5.1
```

### HTTPS

Behind a TLS-terminating reverse proxy, leave `ALMOND_TLS_ENABLED=false`.

**Your own certificate** (e.g. Let's Encrypt):

```bash
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_CERT=/path/to/cert.pem \
ALMOND_TLS_KEY=/path/to/key.pem \
almond
```

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

**Self-signed, for development:** with `ALMOND_TLS_SELF_SIGNED=true`, Almond
writes `cert.pem` and `key.pem` when they are missing. The certificate is valid
for `localhost`, `127.0.0.1` and `::1`; browsers will warn until it is trusted
manually.

```bash
ALMOND_TLS_ENABLED=true ALMOND_TLS_SELF_SIGNED=true cargo run
```

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_SELF_SIGNED=true \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

### S3 providers

Generic:

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
ALMOND_S3_FORCE_PATH_STYLE=true
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

### Local Blossom Cache

Almond can run as a [Local Blossom Cache](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md):
a local proxy on `127.0.0.1:24242` that caches blobs from remote Blossom servers.
Uploads and mirrors are off; the cache fills only through proxying.

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

With Docker:

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

A request flows like this:

1. Client requests `GET /abc123...def.jpg?xs=cdn.example.com&as=<pubkey>`.
2. A cached blob is returned immediately.
3. Otherwise Almond tries the `xs` hints, then the `as` author's BUD-03 server list
   (kind `10063`), caches the blob and returns it.
4. If no hinted server has it, the answer is `404`.

Expired entries are removed after `ALMOND_UPSTREAM_CACHE_TTL`; under capacity
pressure the oldest entries go first.

### FIPS mesh image

`ghcr.io/flox1an/almond-fips` runs FIPS, dnsmasq and Almond in one container. It
accepts every Almond variable and can, at the same time:

- serve Almond over normal HTTP(S) via Docker port publishing,
- serve the same instance to peered FIPS nodes at `http://<this-node-npub>.fips:3000`,
- fetch from upstream Blossom servers reachable only inside the mesh.

**Requirements.** Unlike the standard image, the FIPS image runs as root and
needs `--cap-add NET_ADMIN` and the host TUN device
(`--device /dev/net/tun:/dev/net/tun`): the entrypoint creates the virtual IPv6
interface (usually `fips0`) and installs small DNS and iptables rules. If
`/dev/net/tun` is missing on a Linux host, run `sudo modprobe tun`.

**FIPS variables:**

| Setting | Description |
|---|---|
| `FIPS_NSEC` | Node secret key, hex or `nsec1`; required unless you mount a full config and set `FIPS_GENERATE_CONFIG=false` |
| `FIPS_PEER_NPUB`, `FIPS_PEER_ADDR`, `FIPS_PEER_ALIAS`, `FIPS_PEER_TRANSPORT` | Optional single direct peer |
| `FIPS_PEERS` | Optional multi-peer list, one `npub,addr[,alias[,transport]]` per line; overrides the single-peer variables |
| `FIPS_UDP_BIND` | UDP transport bind address (default `0.0.0.0:2121`) |
| `FIPS_TUN_NAME`, `FIPS_TUN_MTU` | TUN interface settings |
| `FIPS_REWRITE_DNS` | `true` (default): dnsmasq sends `.fips` names to FIPS and everything else to Docker's DNS. `false`: FIPS still hosts Almond on `fips0`, but container-wide `.fips` DNS is not installed |
| `FIPS_HOSTS` | Optional newline-separated aliases for `/etc/fips/hosts`, e.g. `my-upstream npub1...` makes `https://my-upstream.fips` resolve |
| `FIPS_ISOLATE` | `true` blocks non-FIPS egress on the physical container interface (mesh-only) |

**Run it.** Bind Almond to all interfaces so both the published port and the mesh
reach it:

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

Public clients use `https://public.example.com`; FIPS peers use
`http://<this-node-npub>.fips:3000`. For mesh-only operation, drop the `3000:3000`
mapping and keep the transport port `2121/udp`.

**HTTPS inside the mesh** uses Almond's normal TLS settings; peers then use
`https://<this-node-npub>.fips:3000`. The certificate must be valid for the
hostname clients use, or clients must trust it explicitly.

```bash
-e ALMOND_TLS_ENABLED=true \
-e ALMOND_TLS_CERT=/app/certs/cert.pem \
-e ALMOND_TLS_KEY=/app/certs/key.pem \
-v /path/to/certs:/app/certs
```

**Short names.** `-e FIPS_HOSTS='my-almond npub1thisnode...'` lets peers with the
same mapping use `http://my-almond.fips:3000`. The canonical `<npub>.fips` name
works without aliases.

**FIPS upstreams.** Set `.fips` hostnames as upstreams; this needs
`FIPS_REWRITE_DNS=true` (the default) so Almond can resolve them:

```bash
-e ALMOND_UPSTREAM_SERVERS=https://npub1upstream....fips
# or with an alias:
-e FIPS_HOSTS='media-cache npub1upstream...'
-e ALMOND_UPSTREAM_SERVERS=https://media-cache.fips
```

With `ALMOND_CUSTOM_ORIGIN_ACCESS=public`, requests may also pass
`?xs=https://media-cache.fips` or `?origin=https://media-cache.fips`. SSRF
protection stays on: private and local addresses remain blocked, while `.fips`
hostnames resolving to FIPS overlay IPv6 addresses are allowed.

#### Docker Compose

For normal operation, use the bundled
[`docker-compose.fips.yml`](docker-compose.fips.yml). It carries the settings
FIPS always needs (`NET_ADMIN`, `/dev/net/tun`, IPv6 sysctl, ports, storage
volume, DNS defaults), so per-node configuration stays small. Copy
[`.env.fips.example`](.env.fips.example) to `.env.fips` and fill in your node:

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

`FIPS_PEERS` is recommended in production: with several gateways Almond keeps
routing when one is unavailable, which matters for large transfers.

```env
FIPS_PEERS=npub1aaa...,203.0.113.10:2121,gateway-a,udp
npub1bbb...,198.51.100.20:2121,gateway-b,udp
```

```bash
docker compose --env-file .env.fips -f docker-compose.fips.yml up -d
```

The compose service looks like this:

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

FIPS upstreams in compose:

```yaml
      ALMOND_UPSTREAM_SERVERS: https://npub1upstream....fips,https://media-cache.fips
      FIPS_HOSTS: |
        media-cache npub1upstream...
```

#### Coolify

Use Coolify's Docker Compose deployment mode: the compose file is then the
source of truth, so `cap_add`, `devices`, `sysctls`, ports, volumes and
environment stay together.

1. Check that the target server has `/dev/net/tun` (`ls -l /dev/net/tun`); if not,
   run `sudo modprobe tun` on the Linux host.
2. Create a Docker Compose resource and paste the `docker-compose.fips.yml` service.
3. Set the variables:

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

4. For public HTTP(S), assign the Coolify domain to the `almond-fips` service on
   container port `3000`. Keep `2121:2121/udp` published for the FIPS transport.

In image-only mode instead of Compose mode, put the runtime settings into Custom
Docker Options; the UDP port, volume and DNS defaults then have to be configured
separately:

```text
--cap-add NET_ADMIN --device /dev/net/tun:/dev/net/tun --sysctl net.ipv6.conf.all.disable_ipv6=0
```

If your hosting provider rejects `devices`, `cap_add` or `sysctls`, FIPS cannot
run inside the container. Run FIPS on the host instead, or use a VPS or
bare-metal server where Docker can access `/dev/net/tun`.

## Embedding as a library

Almond can be embedded in an Axum 0.8 application. The standalone `almond`
binary keeps its CLI, defaults, routes, TLS and signal handling.

```toml
[dependencies]
almond = { path = "../almond", default-features = false }
axum = "0.8"
tokio = { version = "1", features = ["full"] }
```

Omit `default-features = false` to include Cashu payments (the standalone
default). Without the `cashu` feature, configuring paid operations fails
startup; it never silently makes them free.

**Setup.** Start with `Config::defaults()`, override fields, and call
`build_state(&cfg)`. It checks cross-field rules before touching storage and
normalizes blank metrics tokens. `Config::validate()` is available to check
configuration separately. The embedding path reads no process arguments,
environment variables or dotenv files; `Config::load()` and
`config::load_env_file()` are standalone helpers and should not be used inside
an already-running host.

**Host responsibilities.** The host owns its runtime, tracing, allocator, rustls
provider, TLS listener and shutdown policy. Before using WoT/DVM access or
custom-origin discovery (`?as=` can query Nostr relays), install a rustls
`CryptoProvider`; `build_state` returns an error if none is installed. For
example, with `rustls = { version = "0.23.46", features = ["aws-lc-rs"] }`, call
`rustls::crypto::aws_lc_rs::default_provider().install_default()` at startup.
Almond never replaces an installed provider. Local/public mode with
custom-origin discovery disabled needs no global provider.

**Wiring.**

- `create_app(state)` returns the Blossom routes, including `/{filename}`. Merge
  it with the host's router; the host can keep its own `/` and `/health`.
- `standalone_extras(state)` optionally adds Almond's homepage, config editor,
  filter and diagnostics. The standalone binary merges both routers.
- Keep the `JoinSet` returned by `spawn_background_tasks(&state, &cfg)` alive while
  serving. It owns periodic cleanup and refresh jobs; dropping it aborts them.
  Use the same config that built the state. Request-triggered upstream and HLS
  jobs keep their own lifecycle.
- Set `public_url` to the host's externally reachable Blossom origin. Initialize
  one state per storage root and clone it for routers: startup clears temporary
  files and migrates and indexes existing blobs.

A runnable host with its own homepage, health route and graceful shutdown is in
[`examples/embedded.rs`](examples/embedded.rs). It listens on `127.0.0.1:3000`
and stores blobs in `./embedded-files`; pass another address after `--`:

```bash
cargo run --example embedded --no-default-features
cargo run --example embedded --no-default-features -- 127.0.0.1:3001
```

## Migrating from pre-0.5 names

Almond 0.5 renamed every setting to the `ALMOND_*` scheme. The old names below
still work as deprecated aliases: their values are translated (units appended)
and a deprecation warning is logged at startup. If both the old and the new name
are set, the new one wins and a warning names the ignored old one. The old names
will be removed in a later release.

Docker images apply their container defaults only when neither the new nor the
old name is set, so old names passed with `-e` keep working there too.

`ALMOND_S3_*` names are unchanged. Values that were previously accepted
leniently (unknown enum values, invalid npubs, `AUTH_MAX_TTL_SECS=0`) now stop
startup with an error.

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

## Development

Prerequisites: Rust 1.85 or later and the OpenSSL development libraries.

```bash
cargo build --release
cargo run --release
```

Before opening a PR, run what CI runs: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`.

## License

MIT
