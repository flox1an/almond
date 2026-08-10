use futures_util::StreamExt;
use reqwest::{redirect, Client};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::net::lookup_host;
use tracing::{debug, error, info, warn};

use crate::constants::{
    CHUNK_SIZE, DNS_LOOKUP_TIMEOUT_SECS, HTTP_CONNECT_TIMEOUT_SECS, HTTP_REQUEST_TIMEOUT_SECS,
    LOG_INTERVAL, UPSTREAM_POOL_IDLE_TIMEOUT_SECS, UPSTREAM_POOL_MAX_IDLE_PER_HOST,
    UPSTREAM_READ_TIMEOUT_SECS, UPSTREAM_TCP_KEEPALIVE_SECS,
};
use crate::error::{AppError, AppResult};
use crate::models::AppState;
use crate::services::file_storage;

/// True for every address that is not globally routable enough for an
/// untrusted fetch target.  Rejecting documentation and reserved ranges too
/// keeps this policy fail-closed when an address is repurposed.
#[must_use]
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 0 || b == 168))
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && (b == 18 || b == 19 || b == 51))
                || (a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xffc0 == 0xfe80) // link-local
                || (segments[0] & 0xfe00 == 0xfc00) // unique-local
                || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        }
    }
}

struct ResolvedTarget {
    url: reqwest::Url,
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Parse and resolve a fetch URL exactly once.  The resulting addresses are
/// installed into the request client, so the connection cannot be rebound to
/// a different address after validation.
async fn resolve_public_target(url: &str) -> AppResult<ResolvedTarget> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| AppError::BadRequest("Invalid URL format".to_string()))?;
    if parsed.scheme() != "https" {
        return Err(AppError::BadRequest(
            "Only HTTPS URLs are allowed".to_string(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::BadRequest("URL has no hostname".to_string()))?
        .to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addresses = tokio::time::timeout(
        Duration::from_secs(DNS_LOOKUP_TIMEOUT_SECS),
        lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| AppError::Timeout(format!("DNS resolution timeout for {host}")))?
    .map_err(|_| AppError::BadRequest(format!("DNS resolution failed for {host}")))?
    .filter(|address| !is_private_ip(address.ip()))
    .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(AppError::BadRequest(
            "URL does not resolve to a publicly routable address".to_string(),
        ));
    }
    Ok(ResolvedTarget {
        url: parsed,
        host,
        addresses,
    })
}

/// Validate a URL under the same strict policy used by the pinned fetch path.
pub async fn validate_url_for_ssrf(url: &str) -> AppResult<()> {
    resolve_public_target(url).await.map(|_| ())
}

/// Create HTTP client with hardened security settings.
pub fn create_hardened_http_client() -> AppResult<Client> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|error| AppError::InternalError(format!("Failed to create HTTP client: {error}")))
}

/// Build the pooled client for configured upstreams.  Redirects remain
/// disabled: each URL must be independently validated before it is fetched.
pub fn create_upstream_client() -> AppResult<Client> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .read_timeout(Duration::from_secs(UPSTREAM_READ_TIMEOUT_SECS))
        .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(UPSTREAM_TCP_KEEPALIVE_SECS))
        .tcp_nodelay(true)
        .build()
        .map_err(|error| {
            AppError::InternalError(format!("Failed to create upstream HTTP client: {error}"))
        })
}

/// Stream data from Body to temp file while calculating hash
pub async fn stream_to_temp_file(
    mut body_stream: impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin,
    temp_path: &std::path::Path,
    max_bytes: u64,
) -> AppResult<(String, u64)> {
    let mut temp_file = File::create(temp_path).await.map_err(|e| {
        error!("Failed to create temp file: {}", e);
        AppError::IoError(format!("Failed to create temp file: {}", e))
    })?;

    let mut hasher = Sha256::new();
    let mut total_bytes = 0u64;
    let mut last_log_time = std::time::Instant::now();

    while let Some(chunk) = body_stream.next().await {
        let data = chunk.map_err(|_| {
            AppError::PayloadTooLarge(
                "Request body exceeded the configured upload limit".to_string(),
            )
        })?;

        for chunk in data.chunks(CHUNK_SIZE) {
            let next_total = total_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AppError::PayloadTooLarge("Upload size overflow".to_string()))?;
            if next_total > max_bytes {
                return Err(AppError::PayloadTooLarge(format!(
                    "Upload exceeds configured maximum of {max_bytes} bytes"
                )));
            }
            hasher.update(chunk);
            temp_file.write_all(chunk).await.map_err(|e| {
                error!("Failed to write to temp file: {}", e);
                AppError::IoError(format!("Failed to write to temp file: {}", e))
            })?;
            total_bytes = next_total;
        }

        if last_log_time.elapsed() >= LOG_INTERVAL {
            debug!(
                "Upload progress {}: {} MB received",
                temp_path.display(),
                total_bytes / 1_048_576
            );
            last_log_time = std::time::Instant::now();
        }
    }

    temp_file.sync_all().await.map_err(|e| {
        error!("Failed to sync temp file: {}", e);
        AppError::IoError(format!("Failed to sync temp file: {}", e))
    })?;

    let sha256 = hex::encode(hasher.finalize());
    info!(
        "Upload complete: {} MB total, SHA256: {}",
        total_bytes / 1_048_576,
        sha256
    );

    Ok((sha256, total_bytes))
}

/// Stream data from reqwest response to temp file while calculating hash
pub async fn stream_response_to_temp_file(
    response: reqwest::Response,
    temp_path: &std::path::Path,
    max_size_bytes: u64,
) -> AppResult<(String, u64)> {
    let mut temp_file = File::create(temp_path).await.map_err(|e| {
        error!("Failed to create temp file: {}", e);
        AppError::IoError(format!("Failed to create temp file: {}", e))
    })?;

    let mut hasher = Sha256::new();
    let mut body_size: u64 = 0;
    let mut chunks = response.bytes_stream();
    let mut chunk_count: u64 = 0;

    while let Some(chunk_result) = chunks.next().await {
        let chunk = chunk_result.map_err(|e| {
            let error_msg = e.to_string();
            if error_msg.contains("timeout") || error_msg.contains("timed out") {
                error!("⏱️  Download timeout after {}s", HTTP_REQUEST_TIMEOUT_SECS);
                AppError::Timeout("Download timeout".to_string())
            } else {
                error!("❌ Failed to read chunk: {}", error_msg);
                AppError::NetworkError(format!("Failed to read chunk: {}", error_msg))
            }
        })?;

        chunk_count += 1;

        let new_size = body_size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| AppError::PayloadTooLarge("Download size overflow".to_string()))?;
        if new_size > max_size_bytes {
            error!(
                "❌ Download exceeded size limit: {} bytes > {} bytes",
                new_size, max_size_bytes
            );
            return Err(AppError::PayloadTooLarge(format!(
                "File too large: {} bytes exceeds limit of {} MB",
                new_size,
                max_size_bytes / (1024 * 1024)
            )));
        }

        temp_file.write_all(&chunk).await.map_err(|e| {
            error!("❌ Failed to write chunk to temp file: {}", e);
            AppError::IoError(format!("Failed to write chunk: {}", e))
        })?;

        hasher.update(&chunk);
        body_size += chunk.len() as u64;

        if body_size.is_multiple_of(1024 * 1024) {
            debug!(
                "📊 Download progress: {} MB / {} MB",
                body_size / (1024 * 1024),
                max_size_bytes / (1024 * 1024)
            );
        }
    }

    info!(
        "✅ Streaming completed: {} chunks, {} bytes total",
        chunk_count, body_size
    );

    temp_file.sync_all().await.map_err(|e| {
        error!("❌ Failed to sync temp file: {}", e);
        AppError::IoError(format!("Failed to sync temp file: {}", e))
    })?;

    let sha256 = hex::encode(hasher.finalize());
    info!("🔐 Calculated SHA256: {}", sha256);

    Ok((sha256, body_size))
}

/// Finalize an authorized upload or mirror as uploaded content.
pub async fn finalize_upload(
    state: &AppState,
    temp_path: &std::path::Path,
    sha256: &str,
    size: u64,
    extension: Option<String>,
    mime_type: Option<String>,
    expiration: Option<u64>,
) -> AppResult<()> {
    file_storage::publish_blob(
        state,
        temp_path,
        file_storage::BlobPublication {
            sha256: sha256.to_owned(),
            origin: crate::models::BlobOrigin::Upload,
            extension,
            mime_type,
            size,
            expiration,
        },
    )
    .await?;
    Ok(())
}

/// Fetch a URL using the exact public address set that passed validation.
pub async fn fetch_from_url(url: &str) -> AppResult<reqwest::Response> {
    let target = resolve_public_target(url).await?;
    let mut builder = Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS));
    for address in &target.addresses {
        builder = builder.resolve(&target.host, *address);
    }
    let client = builder.build().map_err(|error| {
        AppError::InternalError(format!("Failed to build pinned HTTP client: {error}"))
    })?;
    let response = client
        .get(target.url.clone())
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                AppError::Timeout("Upstream request timed out".to_string())
            } else {
                AppError::BadGateway("Failed to fetch upstream URL".to_string())
            }
        })?;
    if !response.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "Upstream returned status {}",
            response.status()
        )));
    }
    Ok(response)
}

/// Validate an upstream server URL for SSRF protection
/// Normalizes the URL (adds https:// if missing) and validates against private IPs
/// Returns the normalized URL if valid, or an error if the URL is invalid or resolves to a private IP
pub async fn validate_upstream_url(server_url: &str) -> AppResult<String> {
    let normalized = crate::helpers::normalize_server_url(server_url);
    validate_url_for_ssrf(&normalized).await?;
    Ok(normalized)
}

/// Check file size against limit (from Content-Length header)
pub fn check_size_limit(content_length: Option<u64>, max_size_bytes: u64) -> AppResult<()> {
    if let Some(content_length) = content_length {
        info!(
            "📊 Content-Length header present: {} bytes ({} MB)",
            content_length,
            content_length / (1024 * 1024)
        );

        if content_length > max_size_bytes {
            error!(
                "❌ File too large: {} bytes ({} MB) exceeds maximum: {} bytes ({} MB)",
                content_length,
                content_length / (1024 * 1024),
                max_size_bytes,
                max_size_bytes / (1024 * 1024)
            );
            return Err(AppError::PayloadTooLarge(format!(
                "File too large: {} MB exceeds limit of {} MB",
                content_length / (1024 * 1024),
                max_size_bytes / (1024 * 1024)
            )));
        }
        info!(
            "✅ File size check passed: {} bytes within limit",
            content_length
        );
    } else {
        warn!("⚠️  No Content-Length header, proceeding with streaming download");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn non_public_ipv4_ranges_are_rejected() {
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(172, 16, 0, 1),
            Ipv4Addr::new(192, 168, 0, 1),
            Ipv4Addr::new(198, 18, 0, 1),
        ] {
            assert!(is_private_ip(IpAddr::V4(address)), "{address}");
        }
    }

    #[test]
    fn loopback_and_mapped_ipv6_are_rejected() {
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_ip(IpAddr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        )));
        assert!(is_private_ip(IpAddr::V6("fe80::1".parse().unwrap())));
    }
    #[tokio::test]
    async fn streaming_upload_stops_at_its_explicit_byte_limit() {
        let path = std::env::temp_dir().join(format!("almond-limit-{}", uuid::Uuid::new_v4()));
        let stream = futures_util::stream::iter(vec![Ok::<_, axum::Error>(
            axum::body::Bytes::from_static(b"oversized"),
        )]);
        let result = stream_to_temp_file(stream, &path, 4).await;
        assert!(matches!(result, Err(AppError::PayloadTooLarge(_))));
        let _ = tokio::fs::remove_file(path).await;
    }
}
