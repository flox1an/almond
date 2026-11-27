# HTTP API

## Endpoints

Blossom endpoints:

| Method and path | Purpose |
|---|---|
| `PUT /upload` | Upload a blob (BUD-02) |
| `HEAD /upload` | Check whether an upload would be admitted (BUD-06) |
| `HEAD /retain/<sha256>`, `PUT /retain/<sha256>` | Quote or purchase additional retention for an existing blob (Almond extension) |
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
| `GET /favicon.ico` | Almond favicon; disabled together with the homepage |
| `GET /config` | Config editor |
| `GET /filter` | BUD-11 filter over all stored hashes |
| `GET /metrics` | Prometheus metrics; requires `Authorization: Bearer <ALMOND_METRICS_TOKEN>` |
| `GET /admin/wallet` | Operator wallet balance; requires admin NIP-98 authorization |
| `POST /admin/withdraw`, `GET /admin/withdraw/<request-id>`, `POST /admin/withdraw/<request-id>/reclaim` | Export, inspect or reclaim operator Cashu; require admin NIP-98 authorization |
| `GET /_upstream` | Configured upstream servers |
| `GET /_wot` | Bloom filter of the web of trust (`?format=json\|bin`, `?fp=`, `?test=<hex pubkey>`) |

Wallet authorization is a breaking cutover: `Authorization: Bearer` and the
storage-directory `admin-token` credential no longer grant access. Configure
`ALMOND_ADMIN_NPUBS` (`--admin-npubs`) and `ALMOND_PUBLIC_URL` (`--public-url`),
then sign each request with a fresh NIP-98 event in
`Authorization: Nostr <base64-event>`. The event binds the exact absolute URL
(including query), HTTP method and, for requests with a body, its SHA-256 payload. Mutations require
the server challenge from `GET /admin/api/challenge` and fresh signatures even
on retries; obtain a new challenge after a server restart.

`POST /admin/withdraw` accepts `{"amount":123,"request_id":"..."}`. Amount
is a positive integer in sats; request IDs are 1–64 ASCII letters, digits,
hyphens or underscores. Save the ID and amount before sending; after a lost
response, inspect the same ID or retry the same body with a fresh signature,
never a new request ID. Reusing an ID with a different amount returns a
conflict. An interrupted withdrawal can report unavailable until startup wallet
recovery; restart and retry the same ID rather than creating another.

The response includes `id`, `operation_id`, `amount`, `fee`, `token` and
`status`; status lookup returns the same fields. Reclaim returns `amount` after
mint fees and only succeeds for an outstanding, unredeemed token. If a reclaim
outcome is uncertain, check its status before retrying; an interrupted reclaim
can be unknown. Treat exported tokens as bearer money and do not log them.
Responses are not cacheable. No-Cashu builds do not expose the
wallet routes; an uninitialized wallet reports service unavailable.

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

## Blob delivery

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
- **`If-Range`**: only the exact strong `ETag` honours the range; a weak tag, a
  date or any other value falls back to the full `200` body.
- Multi-range requests (`bytes=0-9,20-29`) get the full `200` representation;
  `multipart/byteranges` is not implemented.

**Blobs still arriving from upstream.** A blob that is not stored yet is served
from a shared cache fill (see [Upstream lookups](/guide/how-it-works#upstream-lookups)).
The same range and conditional rules apply, with these differences:

- **`Cache-Control: private, no-store`** on every response served before the
  blob's SHA-256 has been verified, so no CDN or browser keeps unverified
  bytes. Once the blob is cached, the immutable headers above apply.
- **Waiting instead of re-fetching**: a range the fill has not reached yet,
  including `bytes=-N`, gets its `206` headers at once (when the upstream
  declared a length) and its body as the bytes arrive. Without a declared
  length, range requests are answered after the fill completes.
- **`If-None-Match`** (weak comparison) that matches gets `304`, and
  **`If-Match`** (strong comparison) gets `412` or the blob, only after
  verification. `If-Range` follows the rule above. `416` is returned as soon
  as the length is known.
- **Failed verification** (wrong hash or too few bytes) aborts the response
  body instead of ending it cleanly, and nothing is cached.
- **`503 Service Unavailable`** when the fill or reader ceilings are reached,
  **`507 Insufficient Storage`** when the blob cannot be reserved against the
  storage limits, **`502 Bad Gateway`** when the upstream fails. These are not
  cached as misses; only `404` is.
- Blobs above `ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE` or `ALMOND_BLOB_MAX_SIZE` are
  proxied without caching; the first request may get a `200` with the whole
  blob even if it asked for a range.

**`Sunset`.** When an upload has a finite retention deadline, `GET` and `HEAD`
(including extension variants, `206` and `304`) return the RFC 8594 / BUD-01
`Sunset` header, exposed to browsers via `Access-Control-Expose-Headers`.

- For unpaid uploads, the deadline is the earlier of the persisted expiration and
  `created_at + ALMOND_UPLOAD_MAX_AGE` under the current configuration.
- Paid uploads additionally have a durable `paid_until` deadline in the wallet
  database; ordinary cleanup does not evict them before that deadline.
- For paid uploads, `X-Expiration` requests a retention target; for unpaid
  uploads it can only shorten retention. The descriptor's `expiration` field
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
