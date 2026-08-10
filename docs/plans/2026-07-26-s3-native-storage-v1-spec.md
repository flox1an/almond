# Optional S3-Compatible Native Storage V1

**Status:** Proposed

## Problem

Almond currently stores blobs on the local filesystem and deliberately avoids a
database or external metadata catalogue. That keeps operation simple, but it
also ties native storage capacity to the local disk. Operators who already use
S3-compatible object storage should be able to place Almond's native blobs in a
bucket without changing the public Blossom API or introducing a second metadata
system.

The storage model also needs to preserve the distinction between blobs that
Almond intentionally owns and blobs retained only as an upstream cache. Native
storage consists of uploads and active user-initiated mirrors. Upstream cache
storage consists of automatic local copies fetched while serving a read from an
upstream Blossom server.

## Goals

1. Add an optional S3-compatible backend for native blob storage.
2. Keep file storage as the default and preserve existing deployments when S3 is
   not configured.
3. Avoid any database, separate metadata store, or S3-only metadata dependency.
4. Keep the existing resolver as the central place for blob lookup, fallback,
   and serving decisions.
5. Make GET, HEAD, DELETE, list, filter, stats, cleanup, retention, capacity,
   and GC semantics explicit when more than one native backend exists.
6. Constrain V1 to common S3-compatible operations supported by R2, MinIO,
   Backblaze B2's S3-compatible API, and AWS S3-compatible services.

## Non-goals

- No migration or backfill command in V1.
- No database or separate metadata store.
- No presigned URLs, direct S3 URLs, or redirects to S3.
- No S3 backend for automatic upstream cache storage.
- No provider-specific configuration surface beyond the four required S3
  environment variables.
- No per-provider tuning flags such as `ALMOND_S3_FORCE_PATH_STYLE` in V1.
- No change to public blob URL shapes.

## Terminology

**Native storage** is Almond-owned storage for:

- direct uploads;
- completed chunked uploads;
- active user-initiated mirror requests;
- HLS playlists and descendants fetched as part of an explicit mirror.

**Upstream cache** is storage for blobs automatically fetched while satisfying a
read through configured upstream Blossom servers.

The source of the bytes does not determine the storage domain. Explicit mirrors
are native storage even though the bytes come from a remote server. Automatic
fallback/cache fills are upstream cache entries even though Almond may serve
them later.

## Configuration

S3 is configured only through environment variables.

The minimal V1 configuration is:

```dotenv
ALMOND_S3_ENDPOINT=https://...
ALMOND_S3_BUCKET=...
ALMOND_S3_ACCESS_KEY_ID=...
ALMOND_S3_SECRET_ACCESS_KEY=...
```

Startup behavior:

- if no `ALMOND_S3_*` variables are set, S3 is disabled and Almond uses file
  storage as it does today;
- if all four required variables are set, S3 native storage is active;
- if any `ALMOND_S3_*` variable is set but the required set is incomplete,
  startup fails with a clear configuration error that lists the missing names.

V1 has no optional S3 environment variables.

Internal defaults:

| Setting | V1 value |
|---|---|
| Region | `auto` |
| Object layout | `native/blobs/<h0h1>/<h2h3>/<blob>_<expiration>.<extension>` |
| Force path style | `false` |

If MinIO compatibility requires path-style addressing in practice, a future
version can add `ALMOND_S3_FORCE_PATH_STYLE`. V1 should not expose that setting
preemptively.

## Storage Domains

S3 applies only to native storage in V1.

| Operation source | S3 configured | Write target |
|---|---:|---|
| Upload | No | native file storage |
| Upload | Yes | native S3 storage |
| User-initiated mirror | No | native file storage |
| User-initiated mirror | Yes | native S3 storage |
| Automatic upstream cache fill | No | file-based upstream cache |
| Automatic upstream cache fill | Yes | file-based upstream cache |

Existing native file blobs remain local when S3 is enabled later. New native
writes go to S3 while S3 is configured. V1 does not copy old file blobs into S3
and does not copy S3 blobs back to file storage.

## Object Key Layout

S3 uses the same self-contained key layout as native file storage, rooted under a
native S3 prefix:

```text
native/blobs/<h0h1>/<h2h3>/<blob>_<expiration>.<extension>
```

Where:

- `<h0h1>` is the first two hex characters of the blob hash;
- `<h2h3>` is the next two hex characters of the blob hash;
- `<blob>` is the full blob hash;
- `<expiration>` is the expiration timestamp encoded the same way as the file
  storage name;
- `<extension>` is the extension encoded the same way as the file storage name.

Rationale:

- keeps file and S3 layouts aligned for simple future migration in either
  direction;
- keeps object names self-contained enough for debugging and manual inspection;
- enables targeted prefix lookup by hash prefix;
- works with standard object-listing APIs in conservative S3-compatible
  providers;
- avoids provider-specific metadata requirements.

Consequence: an exact key lookup is not possible from only the blob hash when
the expiration and extension are unknown. Lazy lookup can list the hash prefix
and filter object names by blob ID.

## Architecture

Do not build a second resolver beside the existing one. Extend the current
resolver so a clearer storage layer can sit underneath it.

Suggested responsibilities:

| Component | Responsibility |
|---|---|
| Resolver | Read order, fallback decisions, HTTP error mapping, and serving orchestration. |
| Native storage service | Upload/mirror write target selection plus native file and native S3 read/delete semantics. |
| Upstream cache storage | Separate file-based store for automatic upstream cache entries. |
| Index service | Builds file and S3 native indexes in the background, serves complete-index operations, and acts only as a cache for GET/HEAD. |

The resolver remains the only central blob resolution path for GET and HEAD.
The storage layer should hide object-key/path construction from handlers.

## Write Semantics

Native writes:

1. Validate upload or mirror authorization as today.
2. Hash and validate bytes as today.
3. If S3 is fully configured, publish to native S3 storage.
4. Otherwise publish to native file storage.
5. Insert/update the native index entry when publication succeeds.

Automatic upstream cache writes:

1. Keep the current upstream fallback policy.
2. Publish the completed automatic cache fill only to the file-based upstream
   cache.
3. Do not write automatic upstream cache entries to S3 in V1.

Publication must remain atomic from the resolver's perspective: a blob should
not become visible in the index before its selected backend can serve it.

## Read And Resolver Semantics

GET and HEAD use this order:

1. native file storage;
2. native S3 storage, if configured;
3. upstream cache;
4. upstream remote lookup/fallback.

GET and HEAD must work immediately while indexes are still building. The index
is a cache for single-blob lookup, not the source of truth. On cache miss, the
resolver can probe the relevant storage backends directly.

If native file storage misses and S3 is configured, the resolver should probe S3
for that blob by targeted hash-prefix lookup. If S3 is not configured, S3 is not
a relevant read storage.

## GET/HEAD HTTP Error Semantics

GET and HEAD return `404 Not Found` only when all relevant read storages were
successfully checked and the blob does not exist.

GET and HEAD return `503 Service Unavailable` when Almond cannot safely
determine existence because a relevant storage backend could not be checked.

Examples:

| Condition | Response |
|---|---|
| Native file hit | `200` |
| Native file miss, S3 hit | `200` |
| Native file miss, S3 miss | `404` |
| Native file miss, S3 unavailable | `503` |
| Native file unavailable, S3 miss | `503` if file is a relevant read storage |
| Native file unavailable, S3 hit | `200` |

The last case returns `200` because the requested blob was found and can be
served. Almond does not need to prove absence in every backend once a readable
copy has been found.

## Delete Semantics

DELETE applies to native storage only:

1. Attempt/delete from native file storage.
2. If S3 is configured, attempt/delete from native S3 storage.
3. Treat `not found` in an individual backend as successful confirmation for
   that backend.
4. Return success only when every relevant native backend has either deleted the
   blob or confirmed that it is not present.
5. Return `503 Service Unavailable` when a relevant backend is temporarily
   unreachable or cannot be checked/deleted.

DELETE does not delete automatic upstream cache entries in V1 unless the
existing API already treats them as part of native deletion. The preferred V1
model is native-only deletion.

## Index Model

No metadata store or database is introduced.

The blob index is eventually complete:

- file index entries can be built through the existing filesystem scan model;
- S3 index entries are built in the background with paginated object listing;
- single-blob GET/HEAD may use the index as a cache and then perform direct
  storage probes on miss;
- global operations require a complete index and must not silently return
  partial results.

Minimum internal index states:

| State | Meaning |
|---|---|
| `not_started` | Index work has not begun. |
| `building` | Background scan/listing is in progress. |
| `complete` | Index is complete for the configured backends. |
| `failed` | Background indexing failed and needs retry or operator action. |

Operations requiring a complete index include:

- `/list`;
- `/filter`;
- stats;
- cleanup;
- retention enforcement;
- capacity calculations;
- garbage collection;
- other global or set-based operations.

During `not_started`, `building`, or `failed`, these operations must fail
clearly instead of returning partial data. For `/list`, V1 should return:

```http
HTTP/1.1 503 Service Unavailable
Content-Type: application/json
Retry-After: 5
```

```json
{
  "error": "blob_index_not_ready",
  "message": "Blob index is still building. Retry later.",
  "index_state": "building"
}
```

`Retry-After` is optional, but recommended while the state is `building`.

## Serving Model

Almond remains the proxy/origin for all blob responses in V1.

There are no direct S3 URLs, presigned URLs, or redirects to S3. This accepts the
tradeoff that Almond does not directly use S3/CDN global delivery in V1.

Benefits:

- the existing serve path remains central;
- File, S3, upstream cache, and upstream fallback behavior stay consistent;
- clients do not need API changes;
- authentication, headers, ETags, range handling, and error mapping stay in one
  place.

## S3 Operations

V1 should use only standard S3-compatible operations:

- `PutObject`;
- `GetObject`;
- `HeadObject`;
- `DeleteObject`;
- `ListObjectsV2`.

Primary documentation and test targets:

- Cloudflare R2;
- MinIO;
- Backblaze B2 S3-compatible API.

AWS S3 can be documented as expected-compatible, but it is not the primary V1
test target.

## Documentation Examples

README or deployment docs should include placeholder-only examples for the three
primary targets. They must use only the four required variables and must not
include real secrets.

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

Backblaze B2 S3-compatible API:

```dotenv
ALMOND_S3_ENDPOINT=https://s3.<region>.backblazeb2.com
ALMOND_S3_BUCKET=<bucket-name>
ALMOND_S3_ACCESS_KEY_ID=<b2-key-id>
ALMOND_S3_SECRET_ACCESS_KEY=<b2-application-key>
```

## Implementation Notes

Potential storage abstractions:

```rust
enum StoragePresence {
    Found(BlobHandle),
    NotFound,
    Unavailable(StorageError),
}

enum IndexState {
    NotStarted,
    Building,
    Complete,
    Failed,
}
```

The important contract is not the exact type shape. The important contract is
that storage probes distinguish `not found` from `unavailable`, and that global
operations can tell whether their index is complete.

The native storage service should expose operations close to:

- `put_native_blob`;
- `probe_native_blob`;
- `open_native_blob`;
- `delete_native_blob`;
- `list_native_blobs_for_index`.

The upstream cache storage should remain separate enough that S3 cannot
accidentally become the automatic upstream cache write target.

## Open Risks

- MinIO deployments may require path-style addressing. V1 intentionally starts
  with `forcePathStyle=false`; compatibility testing should decide whether a
  future env var is required.
- Lazy S3 lookup by prefix can become expensive if many keys share the same
  first four hash characters. The two-level hash prefix should be adequate for
  V1, but large deployments should measure this.
- GET/HEAD availability semantics become stricter because an unavailable
  relevant backend can produce `503` instead of masking uncertainty as `404`.
- Global operations will be temporarily unavailable while S3 listing builds the
  complete index after startup.
- S3-compatible providers differ in consistency, error codes, pagination
  behavior, and endpoint conventions. V1 should test all primary targets before
  documenting support as stable.
- Running Almond as the serving origin means S3 does not reduce outbound
  bandwidth from the Almond process in V1.

## Acceptance Criteria

1. With no S3 env vars, Almond behaves as current file-storage deployments do.
2. With all four S3 env vars, new uploads and user-initiated mirrors are stored
   in S3 using the specified key layout.
3. Partial S3 configuration fails startup with a clear error.
4. Automatic upstream cache entries remain file-based with or without S3.
5. GET/HEAD use the specified resolver order and return `404` versus `503`
   according to backend certainty.
6. DELETE removes or confirms absence in all relevant native backends and
   returns `503` when a relevant backend cannot be checked.
7. `/list`, `/filter`, stats, cleanup, retention, capacity, and GC do not return
   partial results while the native index is incomplete.
8. Almond serves all blob responses itself; no S3 redirects or presigned URLs
   are exposed.
