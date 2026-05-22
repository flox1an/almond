# HLS Recursive Mirror Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** After mirroring an HLS playlist, automatically detect and mirror all referenced child playlists and media segments in the background.

**Architecture:** New `src/services/hls.rs` module handles playlist parsing and recursive background mirroring. The existing `mirror_blob` handler in `src/handlers/upload.rs` is extended with a post-finalization HLS detection step that spawns a `tokio::spawn` background task. Segments are fetched via plain GET from the same origin server with bounded concurrency using `futures::stream::buffer_unordered`.

**Tech Stack:** Rust, Tokio (async runtime), reqwest (HTTP client), regex (playlist parsing), futures (stream concurrency)

**Design doc:** `docs/plans/2026-03-01-hls-mirror-design.md`

---

### Task 1: Add `HLS_MIRROR_CONCURRENCY` to AppState

**Files:**
- Modify: `src/models.rs:153-207` (AppState struct)
- Modify: `src/main.rs` (env var parsing where AppState is constructed)

**Step 1: Add the field to AppState**

In `src/models.rs`, add after the `cashu_wallet` field (line ~206):

```rust
    /// Maximum parallel segment fetches per HLS mirror operation
    pub hls_mirror_concurrency: usize,
```

**Step 2: Parse the env var in main.rs**

Find where AppState is constructed in `src/main.rs`. Add:

```rust
    let hls_mirror_concurrency: usize = env::var("HLS_MIRROR_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
```

And add `hls_mirror_concurrency,` to the AppState constructor.

**Step 3: Verify it compiles**

Run: `cargo build 2>&1 | tail -5`
Expected: Compiles successfully (no errors about missing fields)

**Step 4: Commit**

```bash
git add src/models.rs src/main.rs
git commit -m "feat: add HLS_MIRROR_CONCURRENCY config to AppState"
```

---

### Task 2: Create `src/services/hls.rs` with parsing logic and tests

**Files:**
- Create: `src/services/hls.rs`
- Modify: `src/services/mod.rs` (add `pub mod hls;`)

**Step 1: Write the tests first**

Create `src/services/hls.rs` with the data types, function signatures returning dummy values, and a test module:

```rust
use regex::Regex;
use std::sync::LazyLock;

/// A reference extracted from an HLS playlist (sha256 hash + optional extension)
#[derive(Debug, Clone, PartialEq)]
pub struct HlsReference {
    pub sha256: String,
    pub extension: Option<String>,
}

/// Regex for Blossom HLS references: 64 hex chars with optional .ext
static HLS_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9a-fA-F]{64})(?:\.(\w+))?$").unwrap()
});

/// Check if a MIME type indicates an HLS playlist
pub fn is_hls_playlist(mime_type: &str) -> bool {
    let mime = mime_type.split(';').next().unwrap_or(mime_type).trim();
    matches!(
        mime,
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
    )
}

/// Parse an HLS playlist and extract all Blossom-style references (sha256[.ext])
/// Only non-comment, non-empty lines matching the expected pattern are returned.
pub fn parse_playlist_references(content: &str) -> Vec<HlsReference> {
    content
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            HLS_REF_RE.captures(line).map(|caps| HlsReference {
                sha256: caps[1].to_lowercase(),
                extension: caps.get(2).map(|m| m.as_str().to_string()),
            })
        })
        .collect()
}

/// Extract the base URL (scheme + host) from a full URL.
/// Example: "https://cdn.example.com/abc123.m3u8" -> "https://cdn.example.com"
pub fn extract_origin_base_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{}://{}:{}", scheme, host, port)),
        None => Some(format!("{}://{}", scheme, host)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_hls_playlist tests ---

    #[test]
    fn test_is_hls_playlist_standard_mime() {
        assert!(is_hls_playlist("application/vnd.apple.mpegurl"));
    }

    #[test]
    fn test_is_hls_playlist_x_mpegurl() {
        assert!(is_hls_playlist("application/x-mpegurl"));
    }

    #[test]
    fn test_is_hls_playlist_audio_variants() {
        assert!(is_hls_playlist("audio/mpegurl"));
        assert!(is_hls_playlist("audio/x-mpegurl"));
    }

    #[test]
    fn test_is_hls_playlist_with_charset() {
        assert!(is_hls_playlist("application/vnd.apple.mpegurl; charset=utf-8"));
    }

    #[test]
    fn test_is_hls_playlist_not_hls() {
        assert!(!is_hls_playlist("video/mp4"));
        assert!(!is_hls_playlist("video/mp2t"));
        assert!(!is_hls_playlist("application/octet-stream"));
    }

    // --- parse_playlist_references tests ---

    #[test]
    fn test_parse_master_playlist() {
        let content = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=854x480
a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2560000,RESOLUTION=1280x720
f6e5d4c3b2a1098765432109876543210987654321fedcba0987654321fedcba.m3u8
"#;
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].sha256, "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456");
        assert_eq!(refs[0].extension, Some("m3u8".to_string()));
        assert_eq!(refs[1].sha256, "f6e5d4c3b2a1098765432109876543210987654321fedcba0987654321fedcba");
        assert_eq!(refs[1].extension, Some("m3u8".to_string()));
    }

    #[test]
    fn test_parse_variant_playlist_ts_segments() {
        let content = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.000,
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477.ts
#EXTINF:10.000,
cd2a98d055eef5ec3aca73bd136a40340539138da73144d589d9de5a3a52149a.ts
#EXT-X-ENDLIST
"#;
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].extension, Some("ts".to_string()));
        assert_eq!(refs[1].extension, Some("ts".to_string()));
    }

    #[test]
    fn test_parse_m4s_segments() {
        let content = r#"#EXTM3U
#EXTINF:6.000,
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.m4s
#EXT-X-ENDLIST
"#;
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].extension, Some("m4s".to_string()));
    }

    #[test]
    fn test_parse_hash_without_extension() {
        let content = r#"#EXTM3U
#EXTINF:10.000,
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477
#EXT-X-ENDLIST
"#;
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].extension, None);
    }

    #[test]
    fn test_parse_ignores_non_hash_lines() {
        let content = r#"#EXTM3U
#EXT-X-VERSION:3
not-a-hash.ts
short.ts
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477.ts
"#;
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn test_parse_empty_content() {
        let refs = parse_playlist_references("");
        assert!(refs.is_empty());
    }

    #[test]
    fn test_parse_comments_only() {
        let content = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-ENDLIST\n";
        let refs = parse_playlist_references(content);
        assert!(refs.is_empty());
    }

    #[test]
    fn test_parse_normalizes_hash_to_lowercase() {
        let content = "AABBCCDD11223344556677889900AABBCCDD11223344556677889900AABBCCDD.ts\n";
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].sha256, "aabbccdd11223344556677889900aabbccdd11223344556677889900aabbccdd");
    }

    // --- extract_origin_base_url tests ---

    #[test]
    fn test_extract_origin_simple() {
        assert_eq!(
            extract_origin_base_url("https://cdn.example.com/abc123.m3u8"),
            Some("https://cdn.example.com".to_string())
        );
    }

    #[test]
    fn test_extract_origin_with_port() {
        assert_eq!(
            extract_origin_base_url("https://cdn.example.com:8443/abc123.m3u8"),
            Some("https://cdn.example.com:8443".to_string())
        );
    }

    #[test]
    fn test_extract_origin_with_path() {
        assert_eq!(
            extract_origin_base_url("https://cdn.example.com/some/path/abc123.m3u8"),
            Some("https://cdn.example.com".to_string())
        );
    }

    #[test]
    fn test_extract_origin_invalid_url() {
        assert_eq!(extract_origin_base_url("not a url"), None);
    }
}
```

**Step 2: Register the module**

In `src/services/mod.rs`, add:

```rust
pub mod hls;
```

**Step 3: Run the tests**

Run: `cargo test services::hls --  --nocapture 2>&1 | tail -20`
Expected: All tests pass

**Step 4: Commit**

```bash
git add src/services/hls.rs src/services/mod.rs
git commit -m "feat: add HLS playlist parsing with tests"
```

---

### Task 3: Add background mirror function to `src/services/hls.rs`

**Files:**
- Modify: `src/services/hls.rs` (add `mirror_hls_references` and `mirror_single_reference`)

**Step 1: Add the mirror functions**

Append to `src/services/hls.rs` (after `extract_origin_base_url`, before `#[cfg(test)]`):

```rust
use futures_util::stream::{self, StreamExt};
use tracing::{error, info, warn};

use crate::helpers::get_extension_from_mime;
use crate::models::AppState;
use crate::services::{file_storage, upload};

/// Result of mirroring HLS references
#[derive(Debug)]
pub struct HlsMirrorResult {
    pub fetched: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// Mirror a single Blossom reference from the origin server.
/// Returns Ok(true) if fetched, Ok(false) if skipped (already exists), Err on failure.
async fn mirror_single_reference(
    state: &AppState,
    origin_base_url: &str,
    reference: &HlsReference,
) -> Result<bool, String> {
    // Check if already in index
    let exists = {
        let index = state.file_index.read().await;
        index.contains_key(&reference.sha256)
    };

    if exists {
        return Ok(false);
    }

    // Build fetch URL
    let fetch_url = match &reference.extension {
        Some(ext) => format!("{}/{}.{}", origin_base_url, reference.sha256, ext),
        None => format!("{}/{}", origin_base_url, reference.sha256),
    };

    info!("[HLS] Fetching segment: {}", fetch_url);

    // Fetch (no SSRF check needed - origin was already validated during the playlist mirror)
    let client = upload::create_hardened_http_client()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client.get(&fetch_url).send().await
        .map_err(|e| format!("Failed to fetch {}: {}", fetch_url, e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} for {}", response.status(), fetch_url));
    }

    // Extract content type from response
    let content_type = crate::helpers::extract_content_type_from_response(response.headers());
    let extension = get_extension_from_mime(&content_type);
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;

    // Stream to temp file
    file_storage::ensure_temp_dir(state).await
        .map_err(|e| format!("Failed to ensure temp dir: {}", e))?;
    let temp_path = file_storage::create_temp_path(state, "hls_segment", extension.as_deref());

    let (calculated_sha256, body_size) = upload::stream_response_to_temp_file(
        response, &temp_path, max_size_bytes,
    ).await.map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("Failed to stream {}: {}", fetch_url, e)
    })?;

    // Verify hash
    if calculated_sha256 != reference.sha256 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(format!(
            "SHA256 mismatch for {}: expected {}, got {}",
            fetch_url, reference.sha256, calculated_sha256
        ));
    }

    // Finalize
    upload::finalize_upload(
        state,
        &temp_path,
        &reference.sha256,
        body_size,
        extension,
        Some(content_type),
        None, // no expiration for background-fetched segments
    ).await.map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("Failed to finalize {}: {}", reference.sha256, e)
    })?;

    Ok(true)
}

/// Mirror all HLS references in the background with bounded concurrency.
/// For any mirrored reference that is itself an m3u8, recursively parse and mirror its references.
pub async fn mirror_hls_references(
    state: AppState,
    origin_base_url: String,
    references: Vec<HlsReference>,
    concurrency: usize,
) {
    info!(
        "[HLS] Starting background mirror: {} references from {} (concurrency: {})",
        references.len(),
        origin_base_url,
        concurrency
    );

    let mut all_references = references;
    let mut total_fetched = 0usize;
    let mut total_skipped = 0usize;
    let mut total_failed = 0usize;

    // Process in rounds to handle recursive m3u8 discovery
    let mut round = 0;
    while !all_references.is_empty() {
        round += 1;
        info!("[HLS] Round {}: processing {} references", round, all_references.len());

        let results: Vec<(HlsReference, Result<bool, String>)> = stream::iter(all_references.iter().cloned())
            .map(|reference| {
                let state = state.clone();
                let origin = origin_base_url.clone();
                async move {
                    let result = mirror_single_reference(&state, &origin, &reference).await;
                    (reference, result)
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

        // Collect newly discovered m3u8 playlists for recursive processing
        let mut next_round_references = Vec::new();

        for (reference, result) in &results {
            match result {
                Ok(true) => {
                    total_fetched += 1;
                    // If this was an m3u8, parse it for more references
                    if reference.extension.as_deref() == Some("m3u8") {
                        if let Some(metadata) = file_storage::get_file_metadata(&state, &reference.sha256).await {
                            match tokio::fs::read_to_string(&metadata.path).await {
                                Ok(content) => {
                                    let child_refs = parse_playlist_references(&content);
                                    if !child_refs.is_empty() {
                                        info!(
                                            "[HLS] Found {} child references in {}",
                                            child_refs.len(),
                                            reference.sha256
                                        );
                                        next_round_references.extend(child_refs);
                                    }
                                }
                                Err(e) => {
                                    warn!("[HLS] Failed to read child playlist {}: {}", reference.sha256, e);
                                }
                            }
                        }
                    }
                }
                Ok(false) => {
                    total_skipped += 1;
                    // Even if skipped (already exists), check if it's an m3u8 we need to recurse into
                    if reference.extension.as_deref() == Some("m3u8") {
                        if let Some(metadata) = file_storage::get_file_metadata(&state, &reference.sha256).await {
                            match tokio::fs::read_to_string(&metadata.path).await {
                                Ok(content) => {
                                    let child_refs = parse_playlist_references(&content);
                                    if !child_refs.is_empty() {
                                        info!(
                                            "[HLS] Found {} child references in existing playlist {}",
                                            child_refs.len(),
                                            reference.sha256
                                        );
                                        next_round_references.extend(child_refs);
                                    }
                                }
                                Err(e) => {
                                    warn!("[HLS] Failed to read existing playlist {}: {}", reference.sha256, e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    total_failed += 1;
                    error!("[HLS] Failed to mirror {}: {}", reference.sha256, e);
                }
            }
        }

        all_references = next_round_references;
    }

    info!(
        "[HLS] Background mirror complete: {} fetched, {} skipped (existing), {} failed",
        total_fetched, total_skipped, total_failed
    );
}
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1 | tail -10`
Expected: Compiles successfully

**Step 3: Run existing tests still pass**

Run: `cargo test services::hls 2>&1 | tail -10`
Expected: All tests pass

**Step 4: Commit**

```bash
git add src/services/hls.rs
git commit -m "feat: add HLS background mirror with bounded concurrency and recursion"
```

---

### Task 4: Integrate HLS detection into `mirror_blob` handler

**Files:**
- Modify: `src/handlers/upload.rs:137-250` (mirror_blob function)

**Step 1: Add HLS detection and background spawn after finalize_upload**

In `src/handlers/upload.rs`, in the `mirror_blob` function, after the `track_upload_stats` call (line ~234) and before creating the response descriptor, add the HLS detection logic.

The key change is: after `finalize_upload` succeeds, check if the content_type is HLS. If so, read the finalized file from disk, parse it, and spawn a background task.

Add `use crate::services::hls;` to the imports at the top of the file.

Then insert the following block after `track_upload_stats(&state, body_size).await;` (line 234) and before `// Create response` (line 237):

```rust
    // HLS recursive mirror: if this is a playlist, mirror referenced segments in background
    if hls::is_hls_playlist(&content_type) {
        if let Some(origin_base_url) = hls::extract_origin_base_url(url) {
            // Read the stored playlist to parse references
            let file_metadata = file_storage::get_file_metadata(&state, &expected_sha256).await;
            if let Some(metadata) = file_metadata {
                match tokio::fs::read_to_string(&metadata.path).await {
                    Ok(content) => {
                        let references = hls::parse_playlist_references(&content);
                        if !references.is_empty() {
                            info!(
                                "[HLS] Detected playlist with {} references, spawning background mirror from {}",
                                references.len(),
                                origin_base_url
                            );
                            let state_clone = state.clone();
                            let concurrency = state.hls_mirror_concurrency;
                            tokio::spawn(async move {
                                hls::mirror_hls_references(
                                    state_clone,
                                    origin_base_url,
                                    references,
                                    concurrency,
                                )
                                .await;
                            });
                        }
                    }
                    Err(e) => {
                        warn!("[HLS] Failed to read playlist file for recursive mirror: {}", e);
                    }
                }
            }
        } else {
            warn!("[HLS] Could not extract origin base URL from: {}", url);
        }
    }
```

Also add `warn` to the tracing import at the top of the file if not already there:
```rust
use tracing::{error, info, warn};
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1 | tail -10`
Expected: Compiles successfully

**Step 3: Run all tests**

Run: `cargo test 2>&1 | tail -15`
Expected: All tests pass

**Step 4: Commit**

```bash
git add src/handlers/upload.rs
git commit -m "feat: integrate HLS recursive mirror into mirror_blob handler"
```

---

### Task 5: Final verification and documentation

**Files:**
- Modify: `docs/plans/2026-03-01-hls-mirror-design.md` (mark as implemented)

**Step 1: Full build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Release build succeeds

**Step 2: Run all tests**

Run: `cargo test 2>&1 | tail -15`
Expected: All tests pass

**Step 3: Verify no clippy warnings in new code**

Run: `cargo clippy -- -W clippy::all 2>&1 | grep -E "services/hls|handlers/upload" | head -20`
Expected: No warnings in modified files (or only pre-existing ones)

**Step 4: Commit any fixes**

If clippy found issues, fix and commit:
```bash
git add -A
git commit -m "fix: address clippy warnings in HLS mirror code"
```

---

## Summary of changes

| File | Action | Purpose |
|------|--------|---------|
| `src/models.rs` | Modify | Add `hls_mirror_concurrency` field to AppState |
| `src/main.rs` | Modify | Parse `HLS_MIRROR_CONCURRENCY` env var |
| `src/services/hls.rs` | Create | HLS parsing, detection, and background mirror logic |
| `src/services/mod.rs` | Modify | Register `hls` module |
| `src/handlers/upload.rs` | Modify | Post-mirror HLS detection and background spawn |

## ENV vars added

| Variable | Default | Description |
|----------|---------|-------------|
| `HLS_MIRROR_CONCURRENCY` | `4` | Max parallel segment fetches per HLS mirror |
