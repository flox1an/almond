# How it works

## Blobs and storage

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

## Retention and eviction

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
`expiration` field; see [Blob delivery](/reference/api#blob-delivery).

## Access control

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

## Payments (Cashu / BUD-07)

`ALMOND_CASHU_PAID` lists the operations that cost sats (`upload`, `mirror`,
`download`), priced per MiB by `ALMOND_CASHU_PRICE_PER_MIB`. Tokens must come
from the single mint in `ALMOND_CASHU_MINT` and are received into the wallet at
`ALMOND_CASHU_WALLET_PATH`. Payments apply on top of the access mode. Builds
without the `cashu` feature refuse to start with paid operations configured.

## Upstream lookups

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

## S3 storage

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

Provider examples are in [S3 providers](/deployment/s3).
