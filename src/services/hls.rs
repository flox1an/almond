use futures_util::stream::{self, StreamExt as FuturesStreamExt};
use regex::Regex;
use std::sync::LazyLock;
use tracing::{error, info, warn};

use crate::helpers::get_extension_from_mime;
use crate::models::AppState;
use crate::services::{file_storage, upload};

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


/// Maximum recursion depth for nested HLS playlists (master -> variant -> segments)
const MAX_HLS_RECURSION_DEPTH: usize = 10;

/// Mirror a single Blossom reference from the origin server.
/// Returns Ok(true) if fetched, Ok(false) if skipped (already exists), Err on failure.
async fn mirror_single_reference(
    state: &AppState,
    client: &reqwest::Client,
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
        let _ = tokio::fs::remove_file(&temp_path);
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
        let _ = tokio::fs::remove_file(&temp_path);
        format!("Failed to finalize {}: {}", reference.sha256, e)
    })?;

    Ok(true)
}

/// Try to parse child references from a stored m3u8 playlist.
async fn try_collect_child_references(
    state: &AppState,
    sha256: &str,
) -> Vec<HlsReference> {
    if let Some(metadata) = file_storage::get_file_metadata(state, sha256).await {
        match tokio::fs::read_to_string(&metadata.path).await {
            Ok(content) => {
                let child_refs = parse_playlist_references(&content);
                if !child_refs.is_empty() {
                    info!("[HLS] Found {} child references in {}", child_refs.len(), sha256);
                }
                child_refs
            }
            Err(e) => {
                warn!("[HLS] Failed to read playlist {}: {}", sha256, e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    }
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

    let client = match upload::create_hardened_http_client() {
        Ok(c) => c,
        Err(e) => {
            error!("[HLS] Failed to create HTTP client: {}", e);
            return;
        }
    };

    let mut all_references = references;
    let mut total_fetched = 0usize;
    let mut total_skipped = 0usize;
    let mut total_failed = 0usize;

    // Process in rounds to handle recursive m3u8 discovery
    let mut round = 0;
    while !all_references.is_empty() {
        round += 1;
        if round > MAX_HLS_RECURSION_DEPTH {
            warn!(
                "[HLS] Reached max recursion depth ({}), stopping with {} references remaining",
                MAX_HLS_RECURSION_DEPTH,
                all_references.len()
            );
            break;
        }
        info!("[HLS] Round {}: processing {} references", round, all_references.len());

        let results: Vec<(HlsReference, Result<bool, String>)> = stream::iter(all_references.iter().cloned())
            .map(|reference| {
                let state = state.clone();
                let origin = origin_base_url.clone();
                let client = client.clone();
                async move {
                    let result = mirror_single_reference(&state, &client, &origin, &reference).await;
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
                Ok(fetched) => {
                    if *fetched {
                        total_fetched += 1;
                    } else {
                        total_skipped += 1;
                    }
                    // Whether newly fetched or already existing, recurse into m3u8 playlists
                    if reference.extension.as_deref() == Some("m3u8") {
                        let child_refs = try_collect_child_references(&state, &reference.sha256).await;
                        next_round_references.extend(child_refs);
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
