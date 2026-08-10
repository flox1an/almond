use std::collections::HashSet;
use std::sync::LazyLock;

use futures_util::stream::{self, StreamExt as FuturesStreamExt};
use regex::Regex;
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
static HLS_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9a-fA-F]{64})(?:\.(\w+))?$").unwrap());

/// Check if a MIME type indicates an HLS playlist
#[must_use]
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
#[must_use]
pub fn parse_playlist_references(content: &str) -> Vec<HlsReference> {
    content
        .lines()
        .map(str::trim)
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
/// Example: "<https://cdn.example.com/abc123.m3u8>" -> "<https://cdn.example.com>"
#[must_use]
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
const MAX_HLS_REFERENCES_PER_ROUND: usize = 128;
const MAX_HLS_REFERENCES_TOTAL: usize = 1024;

/// Mirror one reference with the same pinned SSRF policy as the playlist.
async fn mirror_single_reference(
    state: &AppState,
    origin_base_url: &str,
    reference: &HlsReference,
) -> Result<bool, String> {
    if state.file_index.contains(&reference.sha256).await {
        return Ok(false);
    }
    let fetch_url = match &reference.extension {
        Some(extension) => format!("{}/{}.{}", origin_base_url, reference.sha256, extension),
        None => format!("{}/{}", origin_base_url, reference.sha256),
    };
    let response = upload::fetch_from_url(&fetch_url)
        .await
        .map_err(|error| format!("Failed to fetch segment: {error}"))?;
    let content_type = crate::helpers::extract_content_type_from_response(response.headers());
    let extension = get_extension_from_mime(&content_type);
    let max_size_bytes =
        crate::services::intake::size_limit(state, crate::services::intake::Intake::UpstreamFetch);
    file_storage::ensure_storage_capacity(
        state,
        response.content_length().unwrap_or(max_size_bytes),
    )
    .await
    .map_err(|error| format!("Insufficient storage for segment: {error}"))?;
    file_storage::ensure_temp_dir(state)
        .await
        .map_err(|error| format!("Failed to ensure temp dir: {error}"))?;
    let temp = file_storage::TempBlob::reserve(state, "hls_segment", extension.as_deref());
    // Every early return below drops `temp`, which unlinks the partial segment.
    let (calculated_sha256, body_size) =
        match upload::stream_response_to_temp_file(response, temp.path(), max_size_bytes).await {
            Ok(result) => result,
            Err(error) => return Err(format!("Failed to stream segment: {error}")),
        };
    if calculated_sha256 != reference.sha256 {
        return Err(format!("SHA256 mismatch for segment {}", reference.sha256));
    }
    if let Err(error) = upload::finalize_upload(
        state,
        temp,
        &reference.sha256,
        body_size,
        extension,
        Some(content_type),
        None,
    )
    .await
    {
        return Err(format!("Failed to finalize {}: {error}", reference.sha256));
    }
    Ok(true)
}

/// Try to parse child references from a stored m3u8 playlist.
/// Read a stored playlist and parse the blobs it references.
///
/// Shared with the mirror handler, which used to keep a byte-identical copy of
/// the read-and-parse rather than call the function that already existed.
pub async fn collect_playlist_references(state: &AppState, sha256: &str) -> Vec<HlsReference> {
    let Some(metadata) = file_storage::get_file_metadata(state, sha256).await else {
        return Vec::new();
    };
    let playlist = file_storage::read_text(state, &metadata)
        .await
        .map_err(|error| error.to_string());
    match playlist {
        Ok(content) => {
            let child_refs = parse_playlist_references(&content);
            if !child_refs.is_empty() {
                info!(
                    "[HLS] Found {} child references in {}",
                    child_refs.len(),
                    sha256
                );
            }
            child_refs
        }
        Err(error) => {
            warn!("[HLS] Failed to read playlist {}: {}", sha256, error);
            Vec::new()
        }
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

    let mut all_references = references;
    let mut seen = HashSet::new();
    all_references.retain(|reference| seen.insert(reference.sha256.clone()));
    all_references.truncate(MAX_HLS_REFERENCES_PER_ROUND);
    let mut total_fetched = 0usize;
    let mut total_skipped = 0usize;
    let mut total_failed = 0usize;

    for round in 1..=MAX_HLS_RECURSION_DEPTH {
        if all_references.is_empty() || seen.len() > MAX_HLS_REFERENCES_TOTAL {
            break;
        }
        all_references.truncate(MAX_HLS_REFERENCES_PER_ROUND);
        let results: Vec<(HlsReference, Result<bool, String>)> =
            stream::iter(all_references.iter().cloned())
                .map(|reference| {
                    let state = state.clone();
                    let origin = origin_base_url.clone();
                    async move {
                        let result = mirror_single_reference(&state, &origin, &reference).await;
                        (reference, result)
                    }
                })
                .buffer_unordered(concurrency.max(1))
                .collect()
                .await;

        let mut next_round = Vec::new();
        for (reference, result) in results {
            match result {
                Ok(true) => total_fetched += 1,
                Ok(false) => total_skipped += 1,
                Err(error) => {
                    total_failed += 1;
                    error!("[HLS] Failed to mirror {}: {}", reference.sha256, error);
                    continue;
                }
            }
            if reference.extension.as_deref() == Some("m3u8") {
                for child in collect_playlist_references(&state, &reference.sha256).await {
                    if seen.len() >= MAX_HLS_REFERENCES_TOTAL {
                        break;
                    }
                    if seen.insert(child.sha256.clone()) {
                        next_round.push(child);
                    }
                }
            }
        }
        info!(
            "[HLS] Completed round {} with {} queued references",
            round,
            next_round.len()
        );
        all_references = next_round;
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
        assert!(is_hls_playlist(
            "application/vnd.apple.mpegurl; charset=utf-8"
        ));
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
        let content = r"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=854x480
a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2560000,RESOLUTION=1280x720
f6e5d4c3b2a1098765432109876543210987654321fedcba0987654321fedcba.m3u8
";
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].sha256,
            "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456"
        );
        assert_eq!(refs[0].extension, Some("m3u8".to_string()));
        assert_eq!(
            refs[1].sha256,
            "f6e5d4c3b2a1098765432109876543210987654321fedcba0987654321fedcba"
        );
        assert_eq!(refs[1].extension, Some("m3u8".to_string()));
    }

    #[test]
    fn test_parse_variant_playlist_ts_segments() {
        let content = r"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.000,
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477.ts
#EXTINF:10.000,
cd2a98d055eef5ec3aca73bd136a40340539138da73144d589d9de5a3a52149a.ts
#EXT-X-ENDLIST
";
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].extension, Some("ts".to_string()));
        assert_eq!(refs[1].extension, Some("ts".to_string()));
    }

    #[test]
    fn test_parse_m4s_segments() {
        let content = r"#EXTM3U
#EXTINF:6.000,
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.m4s
#EXT-X-ENDLIST
";
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].extension, Some("m4s".to_string()));
    }

    #[test]
    fn test_parse_hash_without_extension() {
        let content = r"#EXTM3U
#EXTINF:10.000,
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477
#EXT-X-ENDLIST
";
        let refs = parse_playlist_references(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].extension, None);
    }

    #[test]
    fn test_parse_ignores_non_hash_lines() {
        let content = r"#EXTM3U
#EXT-X-VERSION:3
not-a-hash.ts
short.ts
b82fcf4dbcec2d8fab7d94bdd48b070aa6e74d7240b1965a0b28c128d6858477.ts
";
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
        assert_eq!(
            refs[0].sha256,
            "aabbccdd11223344556677889900aabbccdd11223344556677889900aabbccdd"
        );
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
