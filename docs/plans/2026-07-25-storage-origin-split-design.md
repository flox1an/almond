# Uploaded and Upstream Cache Storage Split

**Status:** Proposed

## Problem

Almond currently writes every completed blob into the same two-level hash tree
under `STORAGE_PATH`:

- uploads and authorized mirrors are finalized by `finalize_upload`
  (`src/services/upload.rs`);
- transparent proxy fills and `redirect_and_cache` downloads are finalized
  directly in `run_download` (`src/handlers/upstream.rs`).

Both paths insert the same `FileMetadata` shape into one `BlobIndex`, so Almond
cannot distinguish content intentionally stored by a client from content retained
only to accelerate a future upstream request. Consequently,
`MAX_FILE_AGE_DAYS`, `MAX_TOTAL_SIZE`, and `MAX_TOTAL_FILES` treat both classes
identically. Upstream cache entries can therefore live as long as uploaded
content and consume capacity that should preferentially protect uploaded content.

The current recursive startup scan also treats every file below `STORAGE_PATH`
as a live blob. Storage roles are encoded only by convention, which allows
special directories such as `quarantine` to be indexed after a restart. File age
is reconstructed from filesystem creation time and falls back to the current time
when creation time is unavailable, which can reset retention after a restart.

## Goals

1. Store uploaded content and transparent upstream cache content in separate,
   explicit directories.
2. Apply independent maximum ages to those two storage classes.
3. Preferentially evict upstream cache entries before uploaded content when the
   shared capacity limit is exceeded.
4. Preserve one lookup interface so serving, filters, P2P, delete, and report
   continue to work regardless of a blob's storage class.
5. Make publication, collision handling, startup reconstruction, and cleanup
   race-safe when the same hash is uploaded while an upstream fetch is running.
6. Migrate existing installations without risking early deletion of existing
   uploaded content.

## Non-goals

- Separate size or file-count quotas for the upstream cache.
- Sliding expiration based on the last request time.
- Changes to HTTP cache headers; blobs remain content-addressed and immutable.
- Changes to public URL shapes or blob descriptors.
- Changes to which locally available hashes are exported through BUD-11 filters
  or P2P serving.
- A database-backed metadata catalogue.

## Terminology

**Uploaded content** consists of blobs intentionally persisted through an
authorized storage operation:

- `PUT /upload`;
- a completed chunked upload;
- an explicit authorized mirror request;
- HLS playlists and descendants fetched as part of an explicit mirror.

**Upstream cache content** consists only of blobs fetched transparently while
handling a read:

- a cache fill in `proxy` upstream mode;
- a background cache fill in `redirect_and_cache` upstream mode.

The source of the bytes is not sufficient to classify a blob. Explicit mirrors
come from remote URLs but are uploaded content because the client requested and
authorized their persistence.

## Storage layout

`STORAGE_PATH` becomes the root of an explicit layout:

```text
STORAGE_PATH/
├── uploads/
│   └── <hash[0]>/<hash[1]>/<hash>[_<expiration>][.<extension>]
├── upstream-cache/
│   └── <hash[0]>/<hash[1]>/<hash>[.<extension>]
├── temp/
│   └── ...
└── quarantine/
    └── ...
```

The existing two-level hash hierarchy and filename formats remain unchanged
inside each blob directory. Temporary upload chunks and in-flight upstream
files may continue sharing `temp`; they already have separate lifecycle rules
and are not part of the completed-blob index.

Path construction must live behind one storage module. Callers select a storage
class but must not construct `uploads` or `upstream-cache` paths directly.

## Configuration

### Existing settings

`STORAGE_PATH`, `MAX_TOTAL_SIZE`, `MAX_TOTAL_FILES`, and
`CLEANUP_INTERVAL_SECS` retain their existing names and units.

`MAX_FILE_AGE_DAYS` is narrowed to uploaded content:

```dotenv
# Maximum age of uploaded, explicitly mirrored, and HLS-mirrored blobs.
# 0 disables age-based deletion for uploaded content.
MAX_FILE_AGE_DAYS=0
```

An explicit `X-Expiration` remains an absolute deadline for uploaded content.
When both `X-Expiration` and `MAX_FILE_AGE_DAYS` apply, the earlier deadline
wins.

### New setting

```dotenv
# Maximum age of transparently fetched upstream cache entries, in days.
# 0 disables age-based deletion for upstream cache content.
MAX_UPSTREAM_CACHE_TTL_DAYS=1
```

`MAX_UPSTREAM_CACHE_TTL_DAYS` follows the existing
`MAX_FILE_AGE_DAYS` naming and units. Its default is one day, matching the
previously proposed 24-hour cache lifetime.

The cache TTL is measured from completion of the upstream download. It is not
refreshed when the blob is served. Avoiding a sliding TTL keeps filesystem writes
and index mutations out of the serving hot path.

All size and file-count limits remain aggregate across both storage classes.

## Data model

Add an explicit origin to indexed metadata:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOrigin {
    Upload,
    UpstreamCache,
}

pub struct FileMetadata {
    // Existing fields...
    pub origin: BlobOrigin,
}
```

Represent derived storage paths as one value rather than independent fields on
`AppState`:

```rust
pub struct StorageLayout {
    pub root: PathBuf,
    pub uploads: PathBuf,
    pub upstream_cache: PathBuf,
    pub temp: PathBuf,
    pub quarantine: PathBuf,
}
```

`AppState` owns the layout and both age settings. `FileMetadata::created_at`
continues to represent the time the local copy became available. Startup
reconstruction must derive it from filesystem modification time, not creation
time. Modification time is portable across the supported deployment filesystems
and survives a rename from the temporary directory.

One `BlobIndex` continues to contain both origins. This preserves the existing
lookup seam for all readers.

## Publication interface

Both write paths must use one storage publication implementation. The interface
may be expressed as:

```rust
publish_blob(
    state,
    temp_path,
    BlobOrigin::Upload,
    blob_metadata,
)

publish_blob(
    state,
    temp_path,
    BlobOrigin::UpstreamCache,
    blob_metadata,
)
```

The storage implementation owns:

1. destination path selection;
2. parent-directory creation;
3. temporary-to-final rename;
4. metadata construction;
5. origin-aware index publication;
6. removal of a superseded physical copy;
7. cleanup notification.

`src/handlers/upstream.rs` must no longer hand-roll final path construction,
rename, and `FileMetadata` insertion.

## Hash collision and precedence rules

Only one entry per SHA-256 hash is visible through `BlobIndex`.

1. Uploaded content always takes precedence over an upstream cache copy of the
   same hash.
2. Publishing an upload replaces the indexed cache entry and removes the
   physical cache copy after the upload is safely indexed.
3. Completing an upstream fetch must not replace an indexed upload.
4. If an equivalent cache entry is already indexed, a later cache publication
   is redundant and its temporary or duplicate final file is removed.
5. Startup reconstruction scans upstream cache content first and uploaded
   content second, producing the same precedence deterministically.
6. Publication and cleanup for the same hash must be serialized. A stale cleanup
   candidate must not delete a concurrently published copy at the same final
   path.

The index needs an atomic, origin-aware mutation rather than a `contains`
followed by `insert`, which would permit a time-of-check/time-of-use race. The
mutation returns the displaced metadata so the storage implementation can remove
only the superseded physical file.

In-flight readers retain their existing `DownloadHandle` behavior. If an upload
wins while an upstream download is completing, the upstream handle finishes
successfully for attached readers but its result is not published over the
upload.

## Startup migration and reconstruction

Existing installations store legacy blobs directly beneath hexadecimal
first-level directories such as `STORAGE_PATH/5/3/...`. Their provenance cannot
be reconstructed reliably.

Migration therefore applies the conservative rule: **all legacy blobs are
uploaded content**. This prevents a newly introduced short cache TTL from
deleting files that may have been explicitly uploaded.

Startup order:

1. Create the explicit storage directories.
2. Clear abandoned non-chunk temporary files using the existing startup policy.
3. Detect legacy top-level hexadecimal hash directories.
4. Move their blob files into `uploads`, preserving the two-level hierarchy and
   filename.
5. Resume safely if a prior migration stopped after moving only part of the
   tree.
6. Build the in-memory index from `upstream-cache` and `uploads` only.
7. Start cleanup and request handling after reconstruction succeeds.

Migration is a same-filesystem rename under the normal layout and should not copy
blob bodies. When a partially migrated destination directory already exists,
migration merges entries rather than replacing the directory. Special
subdirectories (`temp`, `quarantine`, `uploads`, and `upstream-cache`) are never
interpreted as legacy hash directories.

An unrecoverable migration error fails startup with the source and destination
paths in the error. Almond must not continue with a silently incomplete index.

If both roots contain the same hash during startup, the uploaded copy wins.
The cache duplicate is removed after the uploaded entry is indexed; a deletion
failure is logged and retried by reconciliation without making the uploaded copy
unavailable.

## Expiration and capacity cleanup

Cleanup is split into expiry and capacity decisions.

### Expiry sweep

An expiry sweep runs every `CLEANUP_INTERVAL_SECS`, regardless of
`changes_pending`. This prevents an idle cache entry from waiting for the current
occasional full scan.

For each indexed entry:

- `Upload` expires at the earlier of:
  - its explicit `X-Expiration`, when present;
  - `created_at + MAX_FILE_AGE_DAYS`, when the setting is nonzero.
- `UpstreamCache` expires at
  `created_at + MAX_UPSTREAM_CACHE_TTL_DAYS` when the setting is nonzero.

Time arithmetic must use checked or saturating operations. An expiration equal
to the current time is expired.

### Capacity enforcement

After expired entries are excluded, `MAX_TOTAL_SIZE` and `MAX_TOTAL_FILES` are
enforced across both origins:

1. evict the oldest upstream cache entries until both limits are satisfied;
2. if still over limit, evict the oldest uploaded entries.

This implements actual FIFO eviction within each priority class. The current
implementation sorts oldest-first and retains entries until the limit is full,
which keeps old entries and selects newer entries for deletion; the rewrite must
correct that behavior.

### Physical deletion

Cleanup operates on an indexed metadata snapshot but revalidates the exact
indexed entry while holding the per-hash mutation guard before deleting. A
re-upload or origin promotion invalidates the stale candidate. Index removal and
physical deletion must never remove a newer publication.

Empty-directory cleanup runs independently beneath `uploads` and
`upstream-cache`. It must not traverse or remove `STORAGE_PATH` itself.

## Existing behavior preserved

- GET and HEAD serve either origin through the same `BlobIndex` lookup.
- The BUD-11 filter contains hashes from both origins.
- P2P serving exports locally available blobs from both origins.
- `/list` continues to include both origins; this change does not alter its
  response schema.
- Delete and report operations act on the currently indexed copy regardless of
  origin.
- Existing immutable response cache headers remain unchanged.
- Aggregate storage metrics continue to report both origins.

Cleanup logs must break deletions down by origin and reason so operators can
confirm that the cache TTL is working without changing the public metrics
contract.

## Failure handling

- A failed upstream download never publishes a cache entry and removes its
  temporary file.
- A failed final rename leaves the previous indexed entry untouched.
- A failed duplicate-file deletion does not displace the preferred indexed
  entry; reconciliation retries it.
- A failed cleanup deletion leaves or restores the index entry unless a newer
  publication already occupies the hash.
- Restart reconstruction ignores incomplete files under `temp`.
- Cleanup and publication errors include the blob origin and physical path in
  logs.

## Implementation map

| Area | Required change |
|---|---|
| `src/models.rs` | Add `BlobOrigin`, `StorageLayout`, metadata origin, and cache TTL state. |
| `src/main.rs` | Parse `MAX_UPSTREAM_CACHE_TTL_DAYS`; create directories; migrate legacy blobs; scan explicit roots; schedule expiry sweeps. |
| `src/services/file_storage.rs` | Own path selection, publication, precedence, duplicate removal, and storage mutation coordination. |
| `src/services/upload.rs` | Publish uploads and authorized mirrors as `Upload`. |
| `src/handlers/upstream.rs` | Publish transparent fills as `UpstreamCache`; remove direct filesystem/index finalization. |
| `src/services/download.rs` | Resolve upstream temporary paths through `StorageLayout`. |
| `src/handlers/upload.rs` | Resolve upload/chunk temporary paths through `StorageLayout`. |
| `src/handlers/report.rs` | Resolve quarantine paths through `StorageLayout`. |
| `src/services/blob_index.rs` | Add atomic origin-aware publication and exact-entry conditional removal. |
| `src/utils.rs` | Scan both blob roots, migrate timestamps using modification time, and implement origin-aware expiry/capacity cleanup. |
| `.env.example`, `README.md` | Document the new layout and `MAX_UPSTREAM_CACHE_TTL_DAYS`. |

## Verification

### Unit and filesystem tests

1. Upload publication produces a path under `uploads` and metadata origin
   `Upload`.
2. Transparent upstream publication produces a path under `upstream-cache` and
   metadata origin `UpstreamCache`.
3. Explicit mirrors and HLS descendants publish as uploads.
4. Startup scanning indexes both roots but ignores `temp` and `quarantine`.
5. Uploaded content wins a duplicate-hash startup collision.
6. Upload publication replaces a cached copy and removes the cache file.
7. Upstream completion cannot replace a concurrent upload.
8. A stale cleanup candidate cannot delete a republished hash.
9. `MAX_UPSTREAM_CACHE_TTL_DAYS` expires only upstream cache entries.
10. `MAX_FILE_AGE_DAYS` expires only uploaded entries.
11. Explicit `X-Expiration` overrides a later upload age deadline.
12. Zero disables each corresponding age policy.
13. Restart reconstruction uses modification time and does not reset age.
14. Capacity pressure evicts oldest cache entries before uploaded entries.
15. Legacy migration is resumable and classifies migrated blobs as uploads.

Tests should inject the current time or call cleanup with an explicit `now`
value; they must not sleep for day-scale configuration durations.

### End-to-end smoke test

Run Almond against a temporary `STORAGE_PATH` and a local upstream fixture:

1. upload one blob through the normal upload endpoint;
2. fetch a different cold hash through upstream proxy mode;
3. confirm both hashes are served locally after completion;
4. confirm their physical paths are under `uploads` and `upstream-cache`;
5. run cleanup with the cache entry beyond its configured deadline while the
   upload remains within its deadline;
6. confirm the cache file and index entry are gone;
7. confirm the uploaded blob remains servable without an upstream request.

## Acceptance criteria

- Completed uploaded and transparently cached blobs never share a storage
  directory.
- `MAX_UPSTREAM_CACHE_TTL_DAYS` controls only transparent upstream cache
  content, in days, with `0` disabling the policy.
- `MAX_FILE_AGE_DAYS` controls only uploaded content.
- Uploaded content always wins same-hash collisions.
- Existing blobs migrate conservatively without body copies or loss.
- Cleanup removes expired cache content within one cleanup interval and protects
  uploaded content under both TTL and capacity pressure.
- Serving interfaces and public HTTP response shapes remain unchanged.
