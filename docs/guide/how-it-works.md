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

The CDK 0.18 dependency upgrade automatically migrates the wallet database,
including transaction keys. Stop Almond and back up the wallet database and
its seed before the first upgraded start. Do not point an older binary at the
migrated database; a rollback requires restoring the matching pre-upgrade backup.

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
| `proxy` (default) | Streams to the client from a shared cache fill (below) |
| `redirect` | Returns `307` to the upstream; nothing is cached; saves bandwidth and CPU |
| `redirect-and-cache` | Returns `307` and fills the cache in the background |

Upstream fetches are SSRF-protected: every candidate URL must be HTTPS (plain
HTTP only for `.fips` overlay hosts) and resolve to a public address; private
and local addresses are blocked, and redirects are not followed.

### Shared progressive fills (`proxy` mode)

All clients asking for the same blob from the same sources share **one**
upstream transfer. The first request starts a fill: one plain `GET` of the
whole blob, with no `Range`, no client conditional headers and
`Accept-Encoding: identity`. The fill writes to a temporary file under
`temp/` and runs as its own background task, so it keeps going when the client
that started it disconnects, even before the upstream has answered.

Every other request for that blob, whether it arrives while the upstream is
still negotiating or while bytes are flowing, attaches to the fill and reads
from the temporary file at its own pace. A slow client never slows the
upstream transfer or other clients, and an attached request never opens a
second upstream connection, no matter how long it waits.

- **Full `GET`**: streams from byte 0 as soon as the bytes arrive.
- **`HEAD`**: answered from the length and type the upstream announced; it
  takes no reader slot and does not wait for bytes.
- **Range far ahead, or `bytes=-N` (the MP4 tail)**: when the upstream sent a
  `Content-Length`, the `206` headers are sent immediately and the body waits
  until the fill reaches those bytes. A player seeking to the end of a 1 GiB
  video, or probing a trailing `moov` atom, therefore waits roughly as long as
  it takes the single upstream transfer to get there. Encode videos with
  `faststart` (`moov` first) for fast startup on a cold cache.
- **Unknown length** (no upstream `Content-Length`): a `Range` request cannot be
  resolved until the end is known, so it waits for the fill to complete and
  is then answered against the real size. Its response headers are only sent
  then, so Almond's 60-second handler timeout can answer `408` on a slow fill;
  the client should retry, and is then served from the cache.
- **Conditionals** are evaluated by Almond against the blob's own `ETag` (its
  quoted hash) and never forwarded upstream. `If-Range` needs the exact strong
  tag; a weak tag, a date or any other
  value gets the full `200`. `If-Match` also compares strongly (`412` on
  mismatch, weak tags included); `If-None-Match` compares weakly and answers
  `304`. Because a successful `If-Match` or `If-None-Match` asserts that the
  blob has this hash, both wait until the fill is verified (the 60-second
  handler timeout applies to that wait as well). Malformed and multi-range
  requests get the full `200`; a range entirely past a known end gets `416`
  right away.
- **Waiting has no extra timer**: an attached reader waits as long as the fill
  makes progress. A stalled upstream fails the fill after 30 seconds without
  data, and every attached reader then gets an aborted body.

Responses served from a fill carry `Cache-Control: private, no-store`,
because their bytes are not verified yet. When the transfer ends, Almond checks
the length and the SHA-256 against the requested hash before publishing the
blob to the cache index. Only then is it served locally with the normal
immutable headers; later requests do not contact the upstream while the copy
lasts (see [Retention and eviction](#retention-and-eviction)).

**Integrity.** For a known length, the final byte of a full response is
withheld until the hash is verified; if verification fails, or the upstream
sends too few bytes, the client sees an aborted transfer instead of a clean
end, and nothing is cached. Bytes already delivered cannot be taken back, and a
range that ends before the last byte may complete before verification.
Clients that need certainty must verify the hash themselves, as Blossom
intends.

**Source isolation.** A fill is shared by blob hash *and* the sources the
request asked for. Requests that only use `ALMOND_UPSTREAM_SERVERS` share one
fill; a request with `?origin=`, `?xs=` or `?as=` shares only with requests
naming exactly the same sources, so a client-supplied origin cannot supply
unverified bytes to requests that only use the configured upstreams. Capacity
is still shared, see below.

**Resource limits.** At most 8 fills run at once across the whole process (not
per blob or per source), and at most 256 response bodies read from fills at
once (`HEAD`, `304`, `412` and `416` answers do not count). When a new fill or
a reader slot is not available, Almond answers `503 Service Unavailable`
instead of opening an extra upstream transfer. Before a fill starts, its full
size is reserved against `ALMOND_STORAGE_MAX_SIZE`, `ALMOND_STORAGE_MAX_FILES`
and `ALMOND_STORAGE_MIN_FREE`, together with every other in-flight fill; an
unknown-length fill reserves the smaller of `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE`
and `ALMOND_BLOB_MAX_SIZE`. If the reservation fails, the answer is
`507 Insufficient Storage`. Reservations and temporary files are released on
success, failure and cancellation. Upstream failures, including an upstream
that answers with a `Content-Encoding` other than identity, are `502`; only
"no source has this blob" is `404`, and only that is remembered as a miss.

**Size limits.** A fill is cached only if the blob fits both
`ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` (default `100MiB`) and `ALMOND_BLOB_MAX_SIZE`
(default `500MiB`). Larger blobs are still served but are an explicit exception
to the one-transfer rule: the first request gets the upstream response as is
(a `200` with the whole blob, even if it asked for a range), and every
concurrent or later request for that blob proxies its own upstream request,
with its `Range` forwarded, for the next 10 minutes. Nothing is written to
disk for them. An unknown-length fill that grows past the limit fails, and its
readers see an aborted transfer.

To cache videos up to 1 GiB, raise both limits, with some headroom:

```bash
ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE=2GiB
ALMOND_BLOB_MAX_SIZE=2GiB          # also raises the upload limit
# Optional: no age-based expiry for upstream copies.
ALMOND_UPSTREAM_CACHE_TTL=0
```

`ALMOND_BLOB_MAX_SIZE` must stay at least `ALMOND_CHUNK_MAX_SIZE` (default
`100MiB`). Make sure the disk holds the expected working set plus up to eight
concurrent fills plus `ALMOND_STORAGE_MIN_FREE`. Cached copies are never
permanent: with the default `ALMOND_UPSTREAM_CACHE_TTL` (`1d`) they expire a day
after they were filled, and even with `0` they are the first to be evicted
when the storage limits are reached.

**Redirect modes.** `redirect` and `redirect-and-cache` send the client to the
upstream with `307`, so the client downloads from there itself and the
one-transfer guarantee does not apply to it. `redirect-and-cache` starts or
joins the same shared fill in the background (when the upstream's `HEAD`
reports a size within the limits), so concurrent redirects never start
competing cache fills.

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
