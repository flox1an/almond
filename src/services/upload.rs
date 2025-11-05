use futures_util::StreamExt;
use reqwest::{Client, redirect};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::net::lookup_host;
use tracing::{error, info, warn};

use crate::constants::*;
use crate::error::{AppError, AppResult};
use crate::models::AppState;
use crate::services::file_storage;

/// Check if an IP address is in a private/local range
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
                || octets[0] == 127
        }
        IpAddr::V6(ipv6) => {
            let segments = ipv6.segments();
            ipv6.is_loopback()
                || (segments[0] & 0xffc0 == 0xfe80)
                || (segments[0] & 0xfe00 == 0xfc00)
        }
    }
}

/// Validate URL is safe to fetch (HTTPS only, no private IPs) - SSRF protection
pub async fn validate_url_for_ssrf(url: &str) -> AppResult<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| AppError::BadRequest(format!("Invalid URL format: {}", e)))?;

    if parsed.scheme() != "https" {
        return Err(AppError::BadRequest(format!(
            "Only HTTPS URLs are allowed, got: {}",
            parsed.scheme()
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::BadRequest("URL has no hostname".to_string()))?;

    info!("🔍 Resolving DNS for hostname: {} (timeout: {}s)", host, DNS_LOOKUP_TIMEOUT_SECS);

    // Resolve DNS with timeout
    let dns_future = lookup_host((host, 443));
    let dns_timeout = tokio::time::sleep(Duration::from_secs(DNS_LOOKUP_TIMEOUT_SECS));

    let addrs = tokio::select! {
        result = dns_future => {
            result.map_err(|e| {
                error!("❌ DNS resolution failed for {}: {}", host, e);
                AppError::BadRequest(format!("DNS resolution failed: {}", e))
            })?
        }
        _ = dns_timeout => {
            error!("❌ DNS resolution timeout after {}s for hostname: {}", DNS_LOOKUP_TIMEOUT_SECS, host);
            return Err(AppError::Timeout(format!("DNS resolution timeout for {}", host)));
        }
    };

    let mut has_valid_ip = false;
    let mut resolved_ips = Vec::new();
    for addr in addrs {
        let ip = addr.ip();
        resolved_ips.push(ip);
        if is_private_ip(ip) {
            error!("❌ URL resolves to private/local IP: {} (from hostname: {})", ip, host);
            return Err(AppError::BadRequest(format!(
                "URL resolves to private/local IP: {}",
                ip
            )));
        }
        has_valid_ip = true;
        info!("✅ Resolved {} -> {} (allowed)", host, ip);
    }

    if !has_valid_ip {
        return Err(AppError::BadRequest(format!(
            "No valid IP addresses resolved for hostname: {}",
            host
        )));
    }

    info!("✅ DNS validation passed for {} ({} IP(s) resolved)", host, resolved_ips.len());
    Ok(())
}

/// Create HTTP client with hardened security settings
pub fn create_hardened_http_client() -> AppResult<Client> {
    let redirect_policy = if HTTP_REQUEST_MAX_REDIRECTS == 0 {
        redirect::Policy::none()
    } else {
        redirect::Policy::limited(HTTP_REQUEST_MAX_REDIRECTS as usize)
    };

    Client::builder()
        .redirect(redirect_policy)
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::InternalError(format!("Failed to create HTTP client: {}", e)))
}

/// Stream data from Body to temp file while calculating hash
pub async fn stream_to_temp_file(
    mut body_stream: impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin,
    temp_path: &std::path::Path,
) -> AppResult<(String, u64)> {
    let mut temp_file = File::create(temp_path).await.map_err(|e| {
        error!("Failed to create temp file: {}", e);
        AppError::IoError(format!("Failed to create temp file: {}", e))
    })?;

    let mut hasher = Sha256::new();
    let mut total_bytes = 0u64;
    let mut last_log_time = std::time::Instant::now();

    while let Some(chunk) = body_stream.next().await {
        let data = chunk.map_err(|e| {
            error!("Failed to read chunk: {}", e);
            AppError::BadRequest(format!("Failed to read chunk: {}", e))
        })?;

        for chunk in data.chunks(CHUNK_SIZE) {
            hasher.update(chunk);
            temp_file.write_all(chunk).await.map_err(|e| {
                error!("Failed to write to temp file: {}", e);
                AppError::IoError(format!("Failed to write to temp file: {}", e))
            })?;
            total_bytes += chunk.len() as u64;
        }

        if last_log_time.elapsed() >= LOG_INTERVAL {
            info!(
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

    let sha256 = format!("{:x}", hasher.finalize());
    info!("Upload complete: {} MB total, SHA256: {}", total_bytes / 1_048_576, sha256);

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

        let new_size = body_size + chunk.len() as u64;
        if new_size > max_size_bytes {
            error!("❌ Download exceeded size limit: {} bytes > {} bytes", new_size, max_size_bytes);
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
            info!("📊 Download progress: {} MB / {} MB", 
                  body_size / (1024 * 1024), max_size_bytes / (1024 * 1024));
        }
    }

    info!("✅ Streaming completed: {} chunks, {} bytes total", chunk_count, body_size);

    temp_file.sync_all().await.map_err(|e| {
        error!("❌ Failed to sync temp file: {}", e);
        AppError::IoError(format!("Failed to sync temp file: {}", e))
    })?;

    let sha256 = format!("{:x}", hasher.finalize());
    info!("🔐 Calculated SHA256: {}", sha256);

    Ok((sha256, body_size))
}

/// Finalize uploaded file - move to final location and add to index
pub async fn finalize_upload(
    state: &AppState,
    temp_path: &std::path::Path,
    sha256: &str,
    size: u64,
    extension: Option<String>,
    mime_type: Option<String>,
    expiration: Option<u64>,
) -> AppResult<()> {
    let final_path = file_storage::get_nested_path(
        &state.upload_dir,
        sha256,
        extension.as_deref(),
        expiration,
    );

    file_storage::move_file(temp_path, &final_path).await?;
    file_storage::add_to_index(state, sha256, final_path, extension, mime_type, size, expiration).await?;
    file_storage::mark_changes_pending(state).await;

    Ok(())
}

/// Fetch file from URL and return response (with SSRF protection)
pub async fn fetch_from_url(url: &str) -> AppResult<reqwest::Response> {
    validate_url_for_ssrf(url).await?;

    let client = create_hardened_http_client()?;

    info!("📡 Sending HTTP GET request to: {}", url);
    let response = client.get(url).send().await.map_err(|e| {
        let error_msg = e.to_string();
        if error_msg.contains("timeout") || error_msg.contains("timed out") {
            error!("⏱️  Request timeout after {}s", HTTP_REQUEST_TIMEOUT_SECS);
            AppError::Timeout(format!("Request timeout for URL: {}", url))
        } else if error_msg.contains("connection") || error_msg.contains("connect") {
            error!("🔌 Connection error: {}", error_msg);
            AppError::BadGateway(format!("Connection error: {}", error_msg))
        } else {
            error!("❌ Failed to fetch URL: {}", error_msg);
            AppError::NetworkError(format!("Failed to fetch URL: {}", error_msg))
        }
    })?;

    let status = response.status();
    info!("📥 Received HTTP response: {} for URL: {}", status, url);

    if !status.is_success() {
        warn!("⚠️  HTTP request failed with status {} for URL: {}", status, url);
        return Err(AppError::BadRequest(format!(
            "Upstream returned status: {}",
            status
        )));
    }

    Ok(response)
}

/// Check file size against limit (from Content-Length header)
pub fn check_size_limit(content_length: Option<u64>, max_size_bytes: u64) -> AppResult<()> {
    if let Some(content_length) = content_length {
        info!("📊 Content-Length header present: {} bytes ({} MB)", 
              content_length, content_length / (1024 * 1024));
        
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
        info!("✅ File size check passed: {} bytes within limit", content_length);
    } else {
        warn!("⚠️  No Content-Length header, proceeding with streaming download");
    }
    Ok(())
}
