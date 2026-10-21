# Blossom specifications

[Blossom](https://github.com/hzrd149/blossom) is defined by a set of BUDs
(Blossom Upgrade Documents). This page lists which BUDs Almond implements.
Deviations from the spec text are documented in detail under
[Spec deviations](/blossom/compliance).

## Support matrix

| BUD | Title | Almond | Notes |
|---|---|---|---|
| [00](https://github.com/hzrd149/blossom/blob/master/buds/00.md) | Blossom Upgrade Documents | – | Meta document |
| [01](https://github.com/hzrd149/blossom/blob/master/buds/01.md) | Server requirements and blob retrieval | ✅ | `GET`/`HEAD /<sha256>[.ext]`, range requests, `ETag`, `Sunset`. Operator routes are not CORS-public, and redirect targets are not checked for the required headers ([deviations](/blossom/compliance#protocol-deviations)). |
| [02](https://github.com/hzrd149/blossom/blob/master/buds/02.md) | Blob upload and management | ✅ | `PUT /upload`, plus the [chunked upload](/blossom/chunked-upload) extension and the `X-Expiration` header |
| [03](https://github.com/hzrd149/blossom/blob/master/buds/03.md) | User Server List | ✅ | Client-side spec. Almond reads the `?as=` author's kind `10063` list for [upstream lookups](/guide/how-it-works#upstream-lookups). |
| [04](https://github.com/hzrd149/blossom/blob/master/buds/04.md) | Mirroring blobs | ✅ | `PUT /mirror`. Mirroring an HLS playlist also mirrors its segments. No MIME sniffing when the origin omits `Content-Type`. |
| [05](https://github.com/hzrd149/blossom/blob/master/buds/05.md) | Media optimization | ❌ | `/media` is not routed |
| [06](https://github.com/hzrd149/blossom/blob/master/buds/06.md) | Upload requirements | ✅ | `HEAD /upload` |
| [07](https://github.com/hzrd149/blossom/blob/master/buds/07.md) | Payment required | ⚠️ | Cashu, behind the `cashu` feature. The `402` challenge is not NUT-24 `creqA`, and an insufficient payment returns `402` instead of `400`. |
| [08](https://github.com/hzrd149/blossom/blob/master/buds/08.md) | Nostr File Metadata Tags | ✅ | Blob descriptors carry `nip94` |
| [09](https://github.com/hzrd149/blossom/blob/master/buds/09.md) | Blob Report | ⚠️ | `PUT /report`, off by default. Public mode accepts reports without an `x` tag, and no report rules are published. |
| [10](https://github.com/hzrd149/blossom/blob/master/buds/10.md) | Blossom URI Schema | – | Client-side only |
| [11](https://github.com/hzrd149/blossom/blob/master/buds/11.md) | Nostr Authorization | ✅ | Kind `24242` events; `server` tag optionally required (`ALMOND_AUTH_REQUIRE_SERVER_TAG`). Single-use authorizations for delete. |
| [12](https://github.com/hzrd149/blossom/blob/master/buds/12.md) | Blob management endpoints | ⚠️ | `/list` with cursor pagination and `DELETE`. `/list/<pubkey>` is not per-uploader, and deleting an absent blob returns `204`. |

✅ implemented · ⚠️ implemented with deviations · ❌ not implemented · – not applicable to servers

## Almond extensions

| Extension | Description |
|---|---|
| [Chunked uploads](/blossom/chunked-upload) | Resumable `PATCH /upload` in parts |
| [Blob discovery filter](/blossom/filter) | `GET /filter` lets clients check existence locally with Binary Fuse filters, without revealing hashes |
| [Filter JS decoding](/blossom/filter-js-spec) | How clients decode and query the `/filter` response |
| `X-Expiration` | Lets a client shorten its own upload's lifetime |
| [Local Blossom Cache](/deployment/local-cache) | Implements the upstream [local cache](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md) profile |
