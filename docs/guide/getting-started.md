# Getting started

**Almond** (Any Large Media ON Demand) is a [Blossom](https://github.com/hzrd149/blossom)
media server for Nostr. It stores blobs on the local filesystem or in an
S3-compatible bucket — no database, no user accounts — and can cache blobs from
other Blossom servers. Blobs are kept permanently unless expiration is enabled.
Writes are authorized with Nostr signatures: open to everyone, limited to an
npub allowlist, to a web of trust, or to DVMs.

## Features

- **Blossom API**: upload (including resumable chunked uploads), download, mirror,
  list, delete and report. Implements BUD-01, BUD-02, BUD-04, BUD-06, BUD-07,
  BUD-09, BUD-11 and BUD-12.
- **Database-free storage**: files on disk or in an S3-compatible bucket, no
  database or user accounts. Optional age limits, size and count caps,
  oldest-first eviction.
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
| Personal server | Only you or a few npubs may upload | `ALMOND_ALLOWED_NPUBS` | [Access control](/guide/how-it-works#access-control) |
| Community server | Uploads for your web of trust | `ALMOND_UPLOAD_ACCESS=wot`, `ALMOND_ALLOWED_NPUBS` as seed | [Access control](/guide/how-it-works#access-control) |
| Public temporary host | Anyone uploads, content expires | `ALMOND_UPLOAD_MAX_AGE`, `ALMOND_STORAGE_MAX_SIZE` | [Retention](/guide/how-it-works#retention-and-eviction) |
| Paid host | Uploads, mirrors or downloads cost sats | `ALMOND_CASHU_PAID`, `ALMOND_CASHU_MINT` | [Payments](/guide/how-it-works#payments-cashu-bud-07) |
| Caching edge | Serves and caches blobs from other servers | `ALMOND_UPSTREAM_SERVERS`, `ALMOND_UPSTREAM_MODE` | [Upstream lookups](/guide/how-it-works#upstream-lookups) |
| Local Blossom Cache | Per-device cache on `127.0.0.1:24242` | uploads off, `ALMOND_CUSTOM_ORIGIN_ACCESS=public` | [Guide](/deployment/local-cache) |
| S3-backed server | Uploads stored in a bucket, served by Almond | `ALMOND_S3_*` | [S3 storage](/guide/how-it-works#s3-storage) |
| FIPS mesh node | Serve or fetch blobs over the FIPS mesh | `docker-compose.fips.yml`, `FIPS_*` | [Guide](/deployment/fips) |
| Embedded | Blossom routes inside your own Axum app | Rust library | [Guide](/library) |

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
