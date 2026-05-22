# HLS Recursive Mirror Design

## Problem

When a user mirrors an HLS playlist (`.m3u8`), only the playlist file itself is stored. The referenced child playlists and media segments (`.ts`, `.m4s`) remain on the origin server. This means the mirrored playlist is useless without the origin being available.

## Solution

After mirroring an HLS playlist, automatically detect the m3u8 content type, parse the playlist, and mirror all referenced blobs in the background from the same origin server.

## Reference

Follows the [Blossom HLS Video Formatting spec](https://github.com/hzrd149/blossom/blob/master/implementations/hls-video-formatting.md):
- All playlist references use relative paths: `<sha256>[.<ext>]`
- Playlists served as-is (no URL rewriting)
- Each segment is a separate blob retrievable via `GET /<sha256>`

## Flow

```
Client: PUT /mirror {"url": "https://cdn.example.com/<hash>.m3u8"}
  │
  ├─ 1. Existing mirror flow runs (auth, SSRF check, fetch, hash verify, store)
  ├─ 2. Detect m3u8 content type after finalize_upload
  ├─ 3. Return BlobDescriptor to client immediately (201 Created)
  │
  └─ 4. Background task (tokio::spawn):
       ├─ Parse playlist content from disk
       ├─ Extract referenced SHA-256 hashes (lines matching <hex64>[.<ext>])
       ├─ For each reference:
       │    ├─ Skip if already in file_index
       │    ├─ Fetch from origin: GET https://cdn.example.com/<hash>[.<ext>]
       │    ├─ Stream to temp file with SHA-256 verification
       │    ├─ Finalize (move to storage, add to index)
       │    └─ If fetched blob is itself m3u8 → recursively parse and queue its references
       └─ Log summary (fetched N segments, skipped M existing, F failures)
```

## HLS Playlist Parsing

Lines in an m3u8 that don't start with `#` are references. In Blossom HLS, these are always `<sha256>[.<ext>]` where:
- `<sha256>` is exactly 64 hex characters
- `<ext>` is optional (`.m3u8`, `.ts`, `.m4s`)

Regex: `^([0-9a-fA-F]{64})(?:\.(\w+))?$` applied to each non-comment, non-empty line.

## Design Decisions

1. **No playlist rewriting** -- per spec, playlists use relative paths and are served as-is
2. **Background mirroring** -- return the playlist BlobDescriptor immediately; segments mirror async
3. **Skip existing files** -- check file_index before fetching each segment
4. **Recursive** -- master → child playlists → segments (two levels typically, but no hard limit)
5. **Best-effort** -- background failures are logged, don't affect the mirror response
6. **No additional auth** -- background fetches are plain GET requests to the origin Blossom server
7. **Bounded concurrency** -- configurable parallel segment fetches (default: 4)
8. **Origin derived from mirror URL** -- segments fetched from same server as the playlist

## Future Extension: Upstream Discovery

The segment fetching function accepts a list of candidate servers `Vec<String>` ordered by priority. Currently this is `[origin]`. In a future iteration, this can be extended to:
- Fall back to configured `UPSTREAM_SERVERS`
- Use `?xs=` servers from the original request
- Query other Blossom servers for missing segments

No code changes needed to the core parsing/detection logic; only the server list construction changes.

## New Code

### `src/services/hls.rs` (new module)
- `is_hls_playlist(mime_type: &str) -> bool` -- check MIME type
- `parse_playlist_references(content: &str) -> Vec<HlsReference>` -- extract hash + extension pairs
- `mirror_hls_references(state, origin_base_url, references, concurrency)` -- background mirror logic with bounded concurrency
- `HlsReference { sha256: String, extension: Option<String> }` -- parsed reference

### `src/handlers/upload.rs` (modify `mirror_blob`)
- After `finalize_upload`, check if `is_hls_playlist(mime_type)`
- If so, read the stored file, parse references, derive origin base URL from the mirror request URL
- Spawn background task calling `mirror_hls_references`

### Configuration
- `HLS_MIRROR_CONCURRENCY`: Max parallel segment fetches per HLS mirror (default: `4`)
- Added to `AppState`

## Error Handling

- **Parse failures**: Log warning, no segments mirrored (playlist itself is still stored)
- **Individual segment fetch failures**: Log error, continue with remaining segments
- **Hash mismatch on segment**: Log error, discard temp file, continue
- **Storage full**: Existing cleanup mechanisms apply; segments that can't be stored are logged and skipped
