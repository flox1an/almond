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
