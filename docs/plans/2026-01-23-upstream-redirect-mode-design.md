# Upstream Redirect Mode Design

## Overview

Add a new upstream mode that redirects clients to upstream servers instead of proxying through Almond. This reduces bandwidth and CPU usage on the Almond server while optionally caching files in the background for future requests.

## Configuration

**New environment variable:**

```
UPSTREAM_MODE=proxy|redirect|redirect_and_cache
```

| Value | Behavior |
|-------|----------|
| `proxy` | Current behavior. Stream from upstream while saving locally. **(Default)** |
| `redirect` | Issue 302 redirect to upstream. No local caching. |
| `redirect_and_cache` | Issue 302 redirect to upstream. Download in background for future requests. |

**Scope:** Only applies when a file is not found locally. Local files are always served directly.

## Redirect Flow

When `UPSTREAM_MODE=redirect` or `redirect_and_cache`:

```
1. Client requests GET /<hash>.mp4
2. Almond checks local file index -> not found
3. Almond iterates through upstream sources in priority order:
   - custom_origin (if ?origin= provided)
   - xs_servers (if ?xs= provided)
   - UPSTREAM_SERVERS (configured)
   - user_servers via BUD-03 (if ?as= provided)
4. For each server: send HEAD request to verify file exists
5. First server returning 2xx -> build redirect URL
6. Return 302 Found with Location header
7. If no server has the file -> return 404 Not Found
```

**Response headers for redirect:**

```
HTTP/1.1 302 Found
Location: https://upstream.example.com/abc123.mp4
Cache-Control: private, no-store
```

The `no-store` prevents CDNs from caching the redirect itself, ensuring future requests come back to Almond to check if the file is now available locally.

## Background Caching (`redirect_and_cache` mode)

After sending the 302 redirect:

```
1. Check ongoing_downloads for this file hash
2. If already downloading -> do nothing (skip duplicate)
3. If not downloading:
   a. Add file hash to ongoing_downloads
   b. Spawn background task
   c. GET full file from the same upstream server we redirected to
   d. Stream to temp file, compute SHA256
   e. Verify hash matches filename
   f. Move to final storage location
   g. Add to file_index
   h. Remove from ongoing_downloads
```

**Size limits:** Background downloads respect `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`. Files exceeding this limit are redirected but not cached.

## Code Changes

### Files to modify

| File | Changes |
|------|---------|
| `src/main.rs` | Parse new `UPSTREAM_MODE` env var, add to `AppState` |
| `src/models.rs` | Add `UpstreamMode` enum and field to `AppState` |
| `src/handlers/upstream.rs` | Add `try_upstream_redirect()` function with HEAD checks and 302 response |
| `src/handlers/file_serving.rs` | Branch on `state.upstream_mode` to call redirect vs proxy logic |

### New types

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpstreamMode {
    #[default]
    Proxy,           // Current behavior
    Redirect,        // 302 redirect, no caching
    RedirectAndCache // 302 redirect + background download
}
```

### New function

```rust
/// Try to find file on upstream servers and return a redirect response
/// Prioritization: custom_origin -> xs_servers -> UPSTREAM_SERVERS -> user servers
pub async fn try_upstream_redirect(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&PublicKey>,
    cache_in_background: bool,
) -> Result<Response, StatusCode>
```

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| All HEAD requests fail | Return 404, add to `failed_upstream_lookups` cache |
| HEAD succeeds but file too large | Redirect (client gets file), skip background cache |
| Background download fails mid-way | Clean up temp file, remove from `ongoing_downloads`, next request tries again |
| File already being downloaded when redirect requested | Still redirect (client gets file immediately), skip duplicate download |
| Range request with redirect | 302 redirect as normal, client re-sends Range header to upstream |

## Backwards Compatibility

Default value is `proxy`, preserving current behavior for existing deployments.
