# HTTP API

## Endpoints

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
| `GET /favicon.ico` | Almond favicon; disabled together with the homepage |
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
