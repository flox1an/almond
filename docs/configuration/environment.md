# Environment variables

Every flag has an environment variable: `ALMOND_` + the flag name upper-cased
with `-` → `_`. The same names work in the process environment, in a dotenv
config file (`--config <FILE>`, else `./.env`) and in Docker `-e` options:

```dotenv
# /etc/almond/almond.env
ALMOND_BIND_ADDR=0.0.0.0:3000
ALMOND_PUBLIC_URL=https://blossom.example.com
ALMOND_ALLOWED_NPUBS=npub1you...
ALMOND_UPLOAD_MAX_AGE=7d
ALMOND_METRICS_TOKEN=change-me
```

Command-line flags override the environment, which overrides the config file;
see [Configuration overview](/configuration/).
[`.env.example`](https://github.com/flox1an/almond/blob/main/.env.example) documents every variable with comments.

## Value formats


| Type | Format |
|---|---|
| Duration | `<n><unit>` with `s`, `m`, `h`, `d` (`30s`, `5m`, `24h`, `7d`); bare `0` allowed, other numbers need a unit |
| Size | `<n><unit>` with `B`, `KiB`, `MiB`, `GiB`, `TiB` (case-insensitive, IEC only; `MB`/`GB` are rejected); bare `0` allowed |
| Boolean | `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`; on the CLI a bare `--tls-enabled` means true, `--list-enabled=false` disables |
| List | Comma-separated; on the CLI also by repeating the flag |
| Access mode | `off`, `public`, `wot`, `dvm` |

## Reference


**Network and TLS**

| Setting | Default | Description |
|---|---|---|
| `ALMOND_BIND_ADDR` | `127.0.0.1:3000` | Listen address; use `0.0.0.0:3000` in a container or behind a proxy |
| `ALMOND_PUBLIC_URL` | `http://127.0.0.1:3000` (`https://` with TLS) | Public origin used in blob descriptors; no trailing slash |
| `ALMOND_CORS_ORIGINS` | – | Browser origins allowed to read discovery and metrics responses; blob `GET`/`HEAD` is always public |
| `ALMOND_TLS_ENABLED` | `false` | Terminate TLS in Almond; leave off behind a TLS proxy |
| `ALMOND_TLS_CERT` / `ALMOND_TLS_KEY` | `./cert.pem` / `./key.pem` | Certificate and private key |
| `ALMOND_TLS_SELF_SIGNED` | `false` | Generate a self-signed pair when the files are missing; development only |

**Storage and retention** (see [Retention and eviction](/guide/how-it-works#retention-and-eviction))

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

**S3** (see [S3 storage](/guide/how-it-works#s3-storage))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_S3_ENDPOINT`, `ALMOND_S3_BUCKET` | – | Bucket location; all four S3 credentials and location values are required together |
| `ALMOND_S3_ACCESS_KEY_ID`, `ALMOND_S3_SECRET_ACCESS_KEY` | – | Credentials |
| `ALMOND_S3_REGION` | `auto` | SigV4 signing region |
| `ALMOND_S3_FORCE_PATH_STYLE` | `false` | Path-style bucket addressing (MinIO, Garage) |

**Access and authorization** (see [Access control](/guide/how-it-works#access-control))

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

**Upstream** (see [Upstream lookups](/guide/how-it-works#upstream-lookups))

| Setting | Default | Description |
|---|---|---|
| `ALMOND_UPSTREAM_SERVERS` | – | Trusted upstream Blossom origins |
| `ALMOND_UPSTREAM_MODE` | `proxy` | `proxy`, `redirect` or `redirect-and-cache` |
| `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` | `100MiB` | Larger blobs are proxied without caching |
| `ALMOND_SERVER_LIST_CACHE_TTL` | `24h` | Cache time for authors' BUD-03 server lists |

**Payments** (see [Payments](/guide/how-it-works#payments-cashu-bud-07))

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

## FIPS mesh

These variables configure the FIPS container of the `ghcr.io/flox1an/almond-fips`
image, not Almond itself: they are read by the image's entrypoint and are not
command-line flags. See [FIPS mesh](/deployment/fips) for the deployment layouts.

**Identity and peers**

| Variable | Default | Description |
|---|---|---|
| `FIPS_NSEC` | – | Node secret key, hex or `nsec1`; required unless a full config is mounted and `FIPS_GENERATE_CONFIG=false` |
| `FIPS_PEERS` | – | Peers, one `npub,addr[,alias[,transport]]` per line; overrides the single-peer variables |
| `FIPS_PEER_NPUB`, `FIPS_PEER_ADDR` | – | A single direct peer: its npub and `host:port` |
| `FIPS_PEER_ALIAS` | `peer` | Name of the single peer in logs and `fipsctl`; also the default for `FIPS_PEERS` lines without one |
| `FIPS_PEER_TRANSPORT` | `udp` | Transport of the single peer; also the default for `FIPS_PEERS` lines without one |
| `FIPS_HOSTS` | – | `.fips` aliases, one `name npub1...` per line; `media-cache npub1...` makes `media-cache.fips` resolve |

**Transport and interface**

| Variable | Default | Description |
|---|---|---|
| `FIPS_UDP_BIND` | `0.0.0.0:2121` | UDP transport listen address; publish this port |
| `FIPS_UDP_PORT` | port of `FIPS_UDP_BIND` | Transport port that `FIPS_ISOLATE` keeps open |
| `FIPS_TCP_BIND` | – | Optional TCP transport listen address |
| `FIPS_TUN_NAME` | `fips0` | Name of the mesh interface |
| `FIPS_TUN_MTU` | `1280` | MTU of the mesh interface |

**Container behavior**

| Variable | Default | Description |
|---|---|---|
| `FIPS_RUN_ALMOND` | `true` | `true`: run Almond in the same container. `false`: run only FIPS, for the pod layout |
| `FIPS_REWRITE_DNS` | `true` | Point the container's (in a pod: the shared) `/etc/resolv.conf` at a local dnsmasq that sends `.fips` names to FIPS and everything else to the original resolver. `false` leaves DNS alone; Almond then cannot resolve `.fips` upstreams |
| `FIPS_ISOLATE` | `false` | `true` drops all traffic on `FIPS_POD_IFACE` except the FIPS transport: mesh-only egress. In a pod this applies to Almond too |
| `FIPS_POD_IFACE` | `eth0` | Physical container interface that `FIPS_ISOLATE` restricts |
| `FIPS_GENERATE_CONFIG` | `true` | Write the FIPS config from these variables at every start; `false` uses a mounted config if it exists |
| `FIPS_CONFIG_PATH` | `/etc/fips/fips.yaml` | Location of the FIPS config |

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
