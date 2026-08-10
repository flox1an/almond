use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use reqwest::header as reqwest_header;
use std::path::Path;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};
use tracing::info;

use crate::constants::{DEFAULT_CONTENT_TYPE, DEFAULT_MIME_TYPE, X_EXPIRATION_HEADER};
use crate::models::AppState;

/// Get MIME type from file path with proper handling for HLS and other media types
#[must_use]
pub fn get_mime_type(path: &Path) -> String {
    // Check for extensions that need explicit handling
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_lowercase().as_str() {
            // HLS playlist
            "m3u8" => return "application/vnd.apple.mpegurl".to_string(),
            // MPEG-TS segments
            "ts" => return "video/mp2t".to_string(),
            // DASH manifest
            "mpd" => return "application/dash+xml".to_string(),
            _ => {}
        }
    }

    // Fall back to mime_guess for other types
    mime_guess::from_path(path).first().map_or_else(
        || DEFAULT_MIME_TYPE.to_string(),
        |m| m.essence_str().to_string(),
    )
}

/// Get file extension from MIME type with proper handling for HLS and other media types
#[must_use]
pub fn get_extension_from_mime(content_type: &str) -> Option<String> {
    // Strip any parameters (e.g., charset, codecs)
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    // Check for MIME types that need explicit handling
    match mime_type {
        // HLS playlist
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl" => {
            return Some("m3u8".to_string());
        }
        // MPEG-TS segments
        "video/mp2t" => return Some("ts".to_string()),
        // DASH manifest
        "application/dash+xml" => return Some("mpd".to_string()),
        // Generic binary — mime_guess returns None for this, so provide a sensible default
        "image/jpeg" | "image/pjpeg" => return Some("jpg".to_string()),
        "application/octet-stream" => return Some("bin".to_string()),
        _ => {}
    }

    // Fall back to mime_guess for other types
    mime_guess::get_mime_extensions_str(mime_type)
        .and_then(|exts| exts.first().map(ToString::to_string))
}

/// Extract content type from headers with fallback
pub fn extract_content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string()
}

/// Extract content type from reqwest response headers
pub fn extract_content_type_from_response(headers: &HeaderMap) -> String {
    headers
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string()
}

/// Extract expiration timestamp from X-Expiration header
#[must_use]
pub fn extract_expiration(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(X_EXPIRATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// How often the cached `Expires` value is re-rendered.
const EXPIRES_REFRESH: Duration = Duration::from_secs(60);

static EXPIRES_CACHE: LazyLock<RwLock<(Instant, HeaderValue)>> =
    LazyLock::new(|| RwLock::new((Instant::now(), render_immutable_expires())));

fn render_immutable_expires() -> HeaderValue {
    let expires = chrono::Utc::now() + chrono::Duration::days(365);
    HeaderValue::from_str(&expires.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .expect("an RFC 7231 date is always a valid header value")
}

/// One-year `Expires` value for immutable blobs.
///
/// Recomputed at most once a minute. Formatting a date and allocating a
/// `HeaderValue` on every response is pure overhead when the timestamp is a
/// year out and the content is content-addressed. `HeaderValue` is
/// `Bytes`-backed, so the returned clone is a refcount bump.
pub fn immutable_expires_header() -> HeaderValue {
    {
        let cached = EXPIRES_CACHE
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.0.elapsed() < EXPIRES_REFRESH {
            return cached.1.clone();
        }
    }

    let mut cached = EXPIRES_CACHE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cached.0.elapsed() >= EXPIRES_REFRESH {
        *cached = (Instant::now(), render_immutable_expires());
    }
    cached.1.clone()
}

/// Track download statistics
pub fn track_download_stats(state: &AppState, size: u64) {
    state.metrics.track_download(size);
}

/// Track upload statistics
pub fn track_upload_stats(state: &AppState) {
    state.metrics.track_upload();
}

/// Create a simple error response
#[allow(dead_code)]
pub fn create_error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(message))
        .expect("Failed to build error response")
}

/// Create a JSON response
#[allow(dead_code)]
pub fn create_json_response<T: serde::Serialize>(data: T) -> Result<Response<Body>, StatusCode> {
    let body = serde_json::to_string(&data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Copy relevant headers from one `HeaderMap` to a reqwest request builder
pub fn copy_headers_to_reqwest(
    headers: &HeaderMap,
    mut request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        request = request.header(header::USER_AGENT, user_agent);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        request = request.header(header::ACCEPT, accept);
    }
    if let Some(range) = headers.get(header::RANGE) {
        request = request.header(header::RANGE, range);
        info!(
            "Forwarding range request: {}",
            range.to_str().unwrap_or("invalid")
        );
    }
    if let Some(if_range) = headers.get(header::IF_RANGE) {
        request = request.header(header::IF_RANGE, if_range);
    }
    if let Some(if_match) = headers.get(header::IF_MATCH) {
        request = request.header(header::IF_MATCH, if_match);
    }
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        request = request.header(header::IF_NONE_MATCH, if_none_match);
    }
    if let Some(if_modified_since) = headers.get(header::IF_MODIFIED_SINCE) {
        request = request.header(header::IF_MODIFIED_SINCE, if_modified_since);
    }
    if let Some(if_unmodified_since) = headers.get(header::IF_UNMODIFIED_SINCE) {
        request = request.header(header::IF_UNMODIFIED_SINCE, if_unmodified_since);
    }
    request
}

/// Copy relevant headers from one `HeaderMap` to a reqwest request builder, excluding the Range header
pub fn copy_headers_without_range(
    headers: &HeaderMap,
    mut request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        request = request.header(header::USER_AGENT, user_agent);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        request = request.header(header::ACCEPT, accept);
    }
    // Explicitly skip RANGE header
    if let Some(if_range) = headers.get(header::IF_RANGE) {
        request = request.header(header::IF_RANGE, if_range);
    }
    if let Some(if_match) = headers.get(header::IF_MATCH) {
        request = request.header(header::IF_MATCH, if_match);
    }
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        request = request.header(header::IF_NONE_MATCH, if_none_match);
    }
    if let Some(if_modified_since) = headers.get(header::IF_MODIFIED_SINCE) {
        request = request.header(header::IF_MODIFIED_SINCE, if_modified_since);
    }
    if let Some(if_unmodified_since) = headers.get(header::IF_UNMODIFIED_SINCE) {
        request = request.header(header::IF_UNMODIFIED_SINCE, if_unmodified_since);
    }
    request
}

/// Copy relevant headers from reqwest response to axum response builder
pub fn copy_headers_from_reqwest(
    response: &reqwest::Response,
    mut response_builder: axum::http::response::Builder,
) -> axum::http::response::Builder {
    // Copy range-related headers from upstream
    if let Some(content_range) = response.headers().get(reqwest_header::CONTENT_RANGE) {
        response_builder = response_builder.header(header::CONTENT_RANGE, content_range);
    }
    if let Some(content_length) = response.headers().get(reqwest_header::CONTENT_LENGTH) {
        response_builder = response_builder.header(header::CONTENT_LENGTH, content_length);
    }
    if let Some(accept_ranges) = response.headers().get(reqwest_header::ACCEPT_RANGES) {
        response_builder = response_builder.header(header::ACCEPT_RANGES, accept_ranges);
    }
    if let Some(cache_control) = response.headers().get(reqwest_header::CACHE_CONTROL) {
        response_builder = response_builder.header(header::CACHE_CONTROL, cache_control);
    }
    if let Some(etag) = response.headers().get(reqwest_header::ETAG) {
        response_builder = response_builder.header(header::ETAG, etag);
    }
    if let Some(last_modified) = response.headers().get(reqwest_header::LAST_MODIFIED) {
        response_builder = response_builder.header(header::LAST_MODIFIED, last_modified);
    }
    response_builder
}

/// Normalize server URL by adding https:// if no protocol is specified
#[must_use]
pub fn normalize_server_url(url: &str) -> String {
    let url = url.trim();

    // Check if URL already has a protocol
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        // Add https:// if no protocol is present
        format!("https://{}", url)
    }
}

/// Return HTTPS then HTTP candidates for a scheme-less BUD-10 `xs` server hint.
///
/// BUD-10 says `xs` SHOULD be a domain name only and clients SHOULD prefer HTTPS
/// but fall back to HTTP. An explicit scheme is an instruction, not a fallback.
#[must_use]
pub fn server_url_candidates(url: &str) -> Vec<String> {
    let url = url.trim();

    if url.starts_with("http://") || url.starts_with("https://") {
        vec![url.to_string()]
    } else {
        vec![format!("https://{}", url), format!("http://{}", url)]
    }
}

/// Build a public blob URL without duplicating the separator slash.
#[must_use]
pub fn build_public_blob_url(public_url: &str, sha256: &str, extension: Option<&str>) -> String {
    let base_url = public_url.trim_end_matches('/');

    match extension {
        Some(ext) => format!("{}/{}.{}", base_url, sha256, ext),
        None => format!("{}/{}", base_url, sha256),
    }
}

/// Combine and normalize server lists from multiple sources
/// Priority order: `xs_servers` (highest) -> `as_servers` -> `default_servers` (lowest)
/// Returns a deduplicated, normalized list of servers
/// URLs are normalized (https:// prefix added if missing) and deduplicated
/// Deduplication is case-insensitive and ignores trailing slashes
#[must_use]
pub fn combine_server_lists(
    xs_servers: Option<&[String]>,
    as_servers: Option<&[String]>,
    default_servers: &[String],
) -> Vec<String> {
    use std::collections::HashSet;

    let mut combined = Vec::new();
    let mut seen = HashSet::new();

    // Helper to normalize URL for comparison (case-insensitive, no trailing slash)
    let normalize_for_comparison =
        |url: &str| -> String { url.trim_end_matches('/').to_lowercase() };

    // Helper to add servers to combined list while deduplicating
    let mut add_servers = |servers: &[String]| {
        for server in servers {
            let normalized = normalize_server_url(server);
            // Normalize for comparison (remove trailing slashes, lowercase comparison)
            let key = normalize_for_comparison(&normalized);
            if seen.insert(key) {
                // Store the normalized URL (preserving original case, but with protocol)
                combined.push(normalized);
            }
        }
    };

    // Add servers in priority order: xs first, then as, then default
    if let Some(xs) = xs_servers {
        add_servers(xs);
    }

    if let Some(as_servers) = as_servers {
        add_servers(as_servers);
    }

    if !default_servers.is_empty() {
        add_servers(default_servers);
    }

    combined
}

#[cfg(test)]
mod tests {
    use super::{build_public_blob_url, server_url_candidates};

    #[test]
    fn build_public_blob_url_removes_duplicate_separator() {
        let url = build_public_blob_url(
            "http://npub1080hnas4cuhp7cwty4cayhfftvgtadueem9kwygu88mjy7ksgpgsswkgud.fips/",
            "77bb8bda6cc05efcbb8ee46840d7010df22f4379834ee817f22650ffa41c567e",
            Some("mp4"),
        );

        assert_eq!(
            url,
            "http://npub1080hnas4cuhp7cwty4cayhfftvgtadueem9kwygu88mjy7ksgpgsswkgud.fips/77bb8bda6cc05efcbb8ee46840d7010df22f4379834ee817f22650ffa41c567e.mp4"
        );
    }

    #[test]
    fn build_public_blob_url_preserves_scheme_separator() {
        let url = build_public_blob_url("https://example.com", "abc123", None);

        assert_eq!(url, "https://example.com/abc123");
    }

    #[test]
    fn server_url_candidates_falls_back_to_http_for_scheme_less_hint() {
        assert_eq!(
            server_url_candidates("media.example.fips"),
            ["https://media.example.fips", "http://media.example.fips"]
        );
    }

    #[test]
    fn server_url_candidates_preserves_explicit_scheme() {
        assert_eq!(
            server_url_candidates("http://media.example.fips"),
            ["http://media.example.fips"]
        );
    }
}
