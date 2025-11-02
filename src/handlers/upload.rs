use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::StreamExt;
use mime_guess;
use nostr_relay_pool::prelude::*;
use reqwest::{Client, redirect};
use serde_json::{self, Value};
use sha2::{Digest, Sha256};
use std::{
    net::IpAddr,
    time::{SystemTime, UNIX_EPOCH, Duration},
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    net::lookup_host,
};
use tracing::{error, info, warn};
use uuid;

use crate::constants::*;
use crate::helpers::*;
use crate::models::{AppState, BlobDescriptor, ChunkInfo, ChunkUpload, FileMetadata};
use crate::utils::get_nested_path;

/// Handle file uploads
pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    // Check if upload feature is enabled
    if !state.feature_upload_enabled {
        error!("Upload feature is disabled");
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        error!("Missing Authorization header");
        StatusCode::UNAUTHORIZED
    })?;

    let auth_event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            error!("Invalid Authorization header format");
            StatusCode::UNAUTHORIZED
        })?,
        &state,
    )
    .await?;

    let content_type = Some(extract_content_type(&headers));

    let extension = content_type
        .as_ref()
        .and_then(|ct| mime_guess::get_mime_extensions_str(ct))
        .and_then(|mime| mime.first().map(|ext| ext.to_string()));

    let body: Body = req.into_body();
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();

    // Create a temporary file
    let temp_dir = state.upload_dir.join("temp");
    fs::create_dir_all(&temp_dir).await.map_err(|e| {
        error!("Failed to create temp directory: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let temp_path = temp_dir.join(format!("upload_{}", uuid::Uuid::new_v4()));
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    let mut temp_file = File::create(&temp_path).await.map_err(|e| {
        error!("Failed to create temp file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut total_bytes = 0;
    let mut last_log_time = std::time::Instant::now();

    // Stream data to temp file and calculate hash
    while let Some(chunk) = stream.next().await {
        let data = chunk.map_err(|e| {
            error!("Failed to read chunk: {}", e);
            StatusCode::BAD_REQUEST
        })?;

        // Process data in chunks
        for chunk in data.chunks(CHUNK_SIZE) {
            hasher.update(chunk);
            temp_file.write_all(chunk).await.map_err(|e| {
                error!("Failed to write to temp file: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            total_bytes += chunk.len();
        }

        // Log progress every 5 seconds
        if last_log_time.elapsed() >= LOG_INTERVAL {
            info!(
                "Upload progress {}: {} MB received",
                temp_path.display(),
                total_bytes / 1_048_576
            );
            last_log_time = std::time::Instant::now();
        }
    }

    // Ensure all data is written to disk
    temp_file.sync_all().await.map_err(|e| {
        error!("Failed to sync temp file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    drop(temp_file);

    info!("Upload complete: {} MB total", total_bytes / 1_048_576);

    let sha256 = format!("{:x}", hasher.finalize());
    let filepath = get_nested_path(&state.upload_dir, &sha256, extension.as_deref());

    // Check if the x tag matches the expected hash
    let x_tag = auth_event.tags.find(TagKind::x()).ok_or_else(|| {
        error!("No x tag found in event");
        StatusCode::UNAUTHORIZED
    })?;

    if x_tag.content() != Some(&sha256) {
        error!("No matching x tag found for hash {}", sha256);
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Create parent directories if they don't exist
    if let Some(parent) = filepath.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            error!("Failed to create directory: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    // Move temp file to final location
    fs::rename(&temp_path, &filepath).await.map_err(|e| {
        error!("Failed to move temp file to final location: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Get file size
    let size = fs::metadata(&filepath)
        .await
        .map_err(|e| {
            error!("Failed to get file metadata: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .len();

    let key = sha256[..64.min(sha256.len())].to_string();
    state.file_index.write().await.insert(
        key.clone(),
        FileMetadata {
            path: filepath.clone(),
            extension: extension.clone(),
            mime_type: content_type.clone(),
            size,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    );

    // After successful upload queue cleanup job
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    // Track upload statistics
    track_upload_stats(&state, total_bytes as u64).await;

    let descriptor = state.create_blob_descriptor(&sha256, size, content_type);

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

/// Check if an IP address is in a private/local range
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            // Private ranges: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
            // Link-local: 169.254.0.0/16
            // Loopback: 127.0.0.0/8
            octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
                || (octets[0] == 127)
        }
        IpAddr::V6(ipv6) => {
            let segments = ipv6.segments();
            // IPv6 private ranges: ::1 (loopback), fe80::/10 (link-local), fc00::/7 (unique local)
            ipv6.is_loopback()
                || (segments[0] & 0xffc0 == 0xfe80) // fe80::/10 (link-local)
                || (segments[0] & 0xfe00 == 0xfc00) // fc00::/7 (unique local)
        }
    }
}

/// HTTP client timeout constants
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const DNS_LOOKUP_TIMEOUT_SECS: u64 = 5;
const HTTP_REQUEST_MAX_REDIRECTS: u8 = 5;

/// Validate URL is safe to fetch (HTTPS only, no private IPs)
async fn validate_url_for_ssrf(url: &str) -> Result<(), StatusCode> {
    // Parse URL and validate it's HTTPS
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        error!("❌ Invalid URL format: {} - error: {}", url, e);
        StatusCode::BAD_REQUEST
    })?;

    if parsed.scheme() != "https" {
        error!("❌ Only HTTPS URLs are allowed, got: {} for URL: {}", parsed.scheme(), url);
        return Err(StatusCode::BAD_REQUEST);
    }

    // Extract hostname
    let host = parsed.host_str().ok_or_else(|| {
        error!("❌ URL has no hostname: {}", url);
        StatusCode::BAD_REQUEST
    })?;

    info!("🔍 Resolving DNS for hostname: {} (timeout: {}s)", host, DNS_LOOKUP_TIMEOUT_SECS);
    
    // Resolve DNS with timeout
    let dns_future = lookup_host((host, 443));
    let dns_timeout = tokio::time::sleep(Duration::from_secs(DNS_LOOKUP_TIMEOUT_SECS));
    
    let addrs = tokio::select! {
        result = dns_future => {
            result.map_err(|e| {
                error!("❌ DNS resolution failed for {}: {}", host, e);
                StatusCode::BAD_REQUEST
            })?
        }
        _ = dns_timeout => {
            error!("❌ DNS resolution timeout after {}s for hostname: {}", DNS_LOOKUP_TIMEOUT_SECS, host);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let mut has_valid_ip = false;
    let mut resolved_ips = Vec::new();
    for addr in addrs {
        let ip = addr.ip();
        resolved_ips.push(ip);
        if is_private_ip(ip) {
            error!("❌ URL resolves to private/local IP: {} (from hostname: {})", ip, host);
            return Err(StatusCode::BAD_REQUEST);
        }
        has_valid_ip = true;
        info!("✅ Resolved {} -> {} (allowed)", host, ip);
    }

    if !has_valid_ip {
        error!("❌ No valid IP addresses resolved for hostname: {}", host);
        return Err(StatusCode::BAD_REQUEST);
    }

    info!("✅ DNS validation passed for {} ({} IP(s) resolved)", host, resolved_ips.len());
    Ok(())
}

/// Extract expected SHA-256 hash from auth event x tags
fn extract_expected_sha256_from_event(event: &Event) -> Option<String> {
    let x_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    for x_tag in x_tags {
        if let Some(content) = x_tag.content() {
            // Check if content looks like a SHA-256 hash (64 hex characters)
            if content.len() == 64 && content.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(content.to_string());
            }
        }
    }

    None
}

/// Handle blob mirroring
pub async fn mirror_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    // Check if mirror feature is enabled
    if !state.feature_mirror_enabled {
        error!("Mirror feature is disabled");
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        error!("Missing Authorization header");
        StatusCode::UNAUTHORIZED
    })?;

    let auth_event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            error!("Invalid Authorization header format");
            StatusCode::UNAUTHORIZED
        })?,
        &state,
    )
    .await?;

    // Extract expected SHA-256 from auth event x tags
    let expected_sha256 = extract_expected_sha256_from_event(&auth_event).ok_or_else(|| {
        error!("No valid SHA-256 hash found in auth event x tags");
        StatusCode::UNAUTHORIZED
    })?;

    info!("Expected SHA-256 from auth event: {}", expected_sha256);

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;

    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or(StatusCode::BAD_REQUEST)?;

    info!("Starting to mirror blob from URL: {}", url);

    // Validate URL for SSRF protection
    validate_url_for_ssrf(url).await?;

    // Configure redirect policy based on constant
    let redirect_policy = if HTTP_REQUEST_MAX_REDIRECTS == 0 {
        redirect::Policy::none()
    } else {
        redirect::Policy::limited(HTTP_REQUEST_MAX_REDIRECTS as usize)
    };

    let redirect_info = if HTTP_REQUEST_MAX_REDIRECTS == 0 {
        "disabled".to_string()
    } else {
        format!("limited to {}", HTTP_REQUEST_MAX_REDIRECTS)
    };

    info!("🌐 Creating hardened HTTP client (redirects: {}, request timeout: {}s, connect timeout: {}s)",
          redirect_info, HTTP_REQUEST_TIMEOUT_SECS, HTTP_CONNECT_TIMEOUT_SECS);

    // Create a hardened HTTP client with configurable redirects and timeouts
    let client = Client::builder()
        .redirect(redirect_policy)
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|e| {
            error!("❌ Failed to create HTTP client: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    info!("📡 Sending HTTP GET request to: {} (timeout: {}s)", url, HTTP_REQUEST_TIMEOUT_SECS);
    
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| {
            // Distinguish between timeout and other errors
            let error_msg = e.to_string();
            if error_msg.contains("timeout") || error_msg.contains("timed out") {
                error!("⏱️  Request timeout after {}s while fetching URL: {}", HTTP_REQUEST_TIMEOUT_SECS, url);
                StatusCode::REQUEST_TIMEOUT
            } else if error_msg.contains("connection") || error_msg.contains("connect") {
                error!("🔌 Connection error while fetching URL: {} - error: {}", url, error_msg);
                StatusCode::BAD_GATEWAY
            } else {
                error!("❌ Failed to fetch URL: {} - error: {}", url, error_msg);
                StatusCode::BAD_REQUEST
            }
        })?;

    let status = response.status();
    info!("📥 Received HTTP response: {} {} for URL: {}", status.as_u16(), status, url);

    if !status.is_success() {
        warn!("⚠️  HTTP request failed with status {} for URL: {}", status, url);
        return Err(StatusCode::BAD_REQUEST);
    }

    let content_type = extract_content_type_from_response(response.headers());
    let content_length = response.content_length();

    // Check size limit before starting download
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    if let Some(content_length) = content_length {
        info!("📊 Content-Length header present: {} bytes ({} MB)", 
              content_length, content_length / (1024 * 1024));
        
        if content_length > max_size_bytes {
            error!(
                "❌ Blob too large: {} bytes ({} MB) exceeds maximum allowed: {} bytes ({} MB)",
                content_length, 
                content_length / (1024 * 1024),
                max_size_bytes, 
                state.max_upstream_download_size_mb
            );
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        info!(
            "✅ Blob size check passed: {} bytes ({} MB) within limit of {} MB",
            content_length, 
            content_length / (1024 * 1024),
            state.max_upstream_download_size_mb
        );
    } else {
        warn!(
            "⚠️  Blob has no Content-Length header, proceeding with streaming download (limit: {} MB)",
            state.max_upstream_download_size_mb
        );
    }

    // Create temp file for streaming download
    let temp_dir = state.upload_dir.join("temp");
    fs::create_dir_all(&temp_dir).await.map_err(|e| {
        error!("Failed to create temp directory: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let temp_path = temp_dir.join(format!("mirror_{}", uuid::Uuid::new_v4()));
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    info!("💾 Streaming blob to temp file: {}", temp_path.display());

    // Stream download with hashing and size checking
    let mut temp_file = File::create(&temp_path).await.map_err(|e| {
        error!("❌ Failed to create temp file {}: {}", temp_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    info!("🔄 Starting streaming download with size validation and hashing");
    let mut hasher = Sha256::new();
    let mut body_size: u64 = 0;
    let mut chunks = response.bytes_stream();
    let mut chunk_count: u64 = 0;

    while let Some(chunk_result) = chunks.next().await {
        let chunk = chunk_result.map_err(|e| {
            let error_msg = e.to_string();
            if error_msg.contains("timeout") || error_msg.contains("timed out") {
                error!("⏱️  Download timeout after {}s while reading chunk {} ({} bytes received so far)", 
                      HTTP_REQUEST_TIMEOUT_SECS, chunk_count, body_size);
                StatusCode::REQUEST_TIMEOUT
            } else {
                error!("❌ Failed to read chunk {} ({} bytes received so far): {}", 
                      chunk_count, body_size, error_msg);
                StatusCode::BAD_REQUEST
            }
        })?;
        
        chunk_count += 1;

        // Check size limit during download (in case Content-Length was missing or wrong)
        let new_size = body_size + chunk.len() as u64;
        if new_size > max_size_bytes {
            error!(
                "❌ Download exceeded size limit at chunk {}: {} bytes > {} bytes ({} MB limit)",
                chunk_count, new_size, max_size_bytes, max_size_bytes / (1024 * 1024)
            );
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        temp_file.write_all(&chunk).await.map_err(|e| {
            error!("❌ Failed to write chunk {} to temp file {} ({} bytes written so far): {}", 
                  chunk_count, temp_path.display(), body_size, e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        hasher.update(&chunk);
        body_size += chunk.len() as u64;

        // Log progress every 1MB
        if body_size.is_multiple_of(1024 * 1024) {
            let progress_mb = body_size / (1024 * 1024);
            let percent = if let Some(expected) = content_length {
                (body_size as f64 / expected as f64 * 100.0) as u8
            } else {
                0
            };
            if content_length.is_some() {
                info!("📊 Download progress: {} MB / {} MB ({}%) - {} chunks", 
                      progress_mb, state.max_upstream_download_size_mb, percent, chunk_count);
            } else {
                info!("📊 Download progress: {} MB / {} MB limit - {} chunks", 
                      progress_mb, state.max_upstream_download_size_mb, chunk_count);
            }
        }
    }
    
    info!("✅ Streaming completed: {} chunks, {} bytes total", chunk_count, body_size);

    // Ensure all data is written to disk
    info!("💾 Syncing temp file to disk: {}", temp_path.display());
    temp_file.sync_all().await.map_err(|e| {
        error!("❌ Failed to sync temp file {}: {}", temp_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    drop(temp_file);
    info!("✅ Temp file synced successfully");

    let sha256 = format!("{:x}", hasher.finalize());
    info!("🔐 Calculated SHA256: {} (expected from auth event: {})", sha256, expected_sha256);

    // Validate the SHA256 hash matches the one from auth event
    if sha256 != expected_sha256 {
        error!(
            "❌ SHA256 hash mismatch! Expected: {}, Calculated: {}",
            expected_sha256, sha256
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    info!("✅ SHA256 validation passed: {}", sha256);

    // Move temp file to final location
    let extension = content_type.split('/').next_back().unwrap_or("bin");
    let filepath = get_nested_path(&state.upload_dir, &sha256, Some(extension));

    // Create parent directories if they don't exist
    if let Some(parent) = filepath.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            error!("Failed to create directory: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    info!("📦 Moving blob from temp {} to final location: {}", 
          temp_path.display(), filepath.display());

    fs::rename(&temp_path, &filepath).await.map_err(|e| {
        error!("❌ Failed to move temp file {} to final location {}: {}", 
               temp_path.display(), filepath.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Drop temp guard early since we've successfully moved the file
    drop(_temp_guard);

    info!("✅ Blob saved successfully to: {} ({} bytes)", filepath.display(), body_size);

    let content_type_clone = content_type.clone();
    let extension = content_type_clone.split('/').next_back().map(|s| s.to_string());
    let descriptor_content_type = content_type_clone.clone();
    let descriptor = state.create_blob_descriptor(
        &sha256,
        body_size,
        Some(descriptor_content_type),
    );

    state.file_index.write().await.insert(
        sha256.clone(),
        FileMetadata {
            path: filepath.clone(),
            extension,
            mime_type: Some(content_type_clone),
            size: body_size,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    );
    info!("✅ Blob descriptor created and added to index for SHA256: {}", sha256);

    // Track mirror upload statistics
    track_upload_stats(&state, body_size).await;

    // After successful mirroring
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    info!("🎉 Mirror operation completed successfully: {} -> {} ({} bytes, type: {})", 
          url, sha256, body_size, descriptor.r#type.as_ref().unwrap_or(&"unknown".to_string()));

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

/// Handle chunked uploads
pub async fn patch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    // Check if upload feature is enabled
    if !state.feature_upload_enabled {
        error!("Upload feature is disabled");
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Extract required headers
    let sha256 = headers.get(X_SHA_256_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing X-SHA-256 header");
            StatusCode::BAD_REQUEST
        })?;

    let upload_type = headers.get(UPLOAD_TYPE_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing Upload-Type header");
            StatusCode::BAD_REQUEST
        })?;

    let upload_length = headers.get(UPLOAD_LENGTH_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            error!("Missing or invalid Upload-Length header");
            StatusCode::BAD_REQUEST
        })?;

    let content_length = headers.get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            error!("Missing or invalid Content-Length header");
            StatusCode::BAD_REQUEST
        })?;

    let upload_offset = headers.get(UPLOAD_OFFSET_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            error!("Missing or invalid Upload-Offset header");
            StatusCode::BAD_REQUEST
        })?;

    let content_type = extract_content_type(&headers);

    // Validate Content-Type is application/octet-stream
    if content_type != DEFAULT_CONTENT_TYPE {
        error!("Invalid Content-Type: {}", content_type);
        return Err(StatusCode::BAD_REQUEST);
    }

    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        error!("Missing Authorization header");
        StatusCode::UNAUTHORIZED
    })?;

    let auth_event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            error!("Invalid Authorization header format");
            StatusCode::UNAUTHORIZED
        })?,
        &state,
    )
    .await?;

    // Validate authorization event has required tags
    if !validate_chunk_upload_auth(&auth_event, sha256, &content_length.to_string()).await {
        error!("Invalid authorization event for chunk upload");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Create temp directory for chunk files
    let temp_dir = state.upload_dir.join("temp").join("chunks");
    fs::create_dir_all(&temp_dir).await.map_err(|e| {
        error!("Failed to create chunk temp directory: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Create a unique file for this chunk
    let chunk_filename = format!("chunk_{}_{}_{}", sha256, upload_offset, uuid::Uuid::new_v4());
    let chunk_path = temp_dir.join(&chunk_filename);
    info!("💾 Creating chunk file: {} (offset: {}, length: {})", chunk_filename, upload_offset, content_length);
    
    // Stream chunk data directly to file
    let body: Body = req.into_body();
    let mut stream = body.into_data_stream();
    let mut chunk_file = File::create(&chunk_path).await.map_err(|e| {
        error!("Failed to create chunk file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut total_written = 0u64;
    let mut chunk_count = 0;
    while let Some(chunk) = stream.next().await {
        let data = chunk.map_err(|e| {
            error!("Failed to read chunk: {}", e);
            StatusCode::BAD_REQUEST
        })?;
        
        chunk_file.write_all(&data).await.map_err(|e| {
            error!("Failed to write chunk data: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        total_written += data.len() as u64;
        chunk_count += 1;
        
        // Log progress for large chunks
        if data.len() > 1024 * 1024 { // Log for chunks > 1MB
            info!("📊 Chunk streaming progress: {} bytes written ({} sub-chunks)", total_written, chunk_count);
        }
    }
    
    info!("✅ Chunk data written to file: {} bytes in {} sub-chunks", total_written, chunk_count);

    // Ensure all data is written to disk
    chunk_file.sync_all().await.map_err(|e| {
        error!("Failed to sync chunk file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    drop(chunk_file);

    // Validate chunk size matches Content-Length
    if total_written != content_length {
        error!("❌ Chunk size mismatch: expected {}, got {}", content_length, total_written);
        // Clean up the chunk file on error
        let _ = fs::remove_file(&chunk_path).await;
        return Err(StatusCode::BAD_REQUEST);
    }
    
    info!("✅ Chunk validation passed: {} bytes (offset: {})", total_written, upload_offset);

    // Validate chunk size doesn't exceed maximum allowed chunk size
    let max_chunk_size_bytes = state.max_chunk_size_mb * 1024 * 1024;
    if content_length > max_chunk_size_bytes {
        error!("Chunk size {} exceeds maximum allowed chunk size {} MB", 
               content_length, state.max_chunk_size_mb);
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Validate offset + length doesn't exceed upload length
    if upload_offset + content_length > upload_length {
        error!("Chunk exceeds upload length: offset {} + length {} > {}", 
               upload_offset, content_length, upload_length);
        return Err(StatusCode::BAD_REQUEST);
    }

    // Get or create chunk upload
    let mut chunk_uploads = state.chunk_uploads.write().await;
    let is_new_upload = !chunk_uploads.contains_key(sha256);
    let chunk_upload = chunk_uploads.entry(sha256.to_string()).or_insert_with(|| {
        let temp_dir = state.upload_dir.join("temp");
        let temp_path = temp_dir.join(format!("chunk_upload_{}", uuid::Uuid::new_v4()));
        ChunkUpload {
            sha256: sha256.to_string(),
            upload_type: upload_type.to_string(),
            upload_length,
            temp_path,
            chunks: Vec::new(),
            created_at: std::time::Instant::now(),
        }
    });

    if is_new_upload {
        info!("🚀 Starting new chunked upload: {} ({} bytes, type: {})", sha256, upload_length, upload_type);
    } else {
        info!("📦 Adding chunk to existing upload: {} (offset: {}, length: {})", sha256, upload_offset, content_length);
    }

    // Validate upload parameters match
    if chunk_upload.upload_type != upload_type || chunk_upload.upload_length != upload_length {
        error!("Upload parameters mismatch for existing upload");
        return Err(StatusCode::BAD_REQUEST);
    }

    // Check for duplicate chunks with the same offset
    if chunk_upload.chunks.iter().any(|c| c.offset == upload_offset) {
        error!("Duplicate chunk at offset {} for upload {}", upload_offset, sha256);
        // Clean up the chunk file
        let _ = fs::remove_file(&chunk_path).await;
        return Err(StatusCode::BAD_REQUEST);
    }

    // Add chunk info
    let chunk_info = ChunkInfo {
        offset: upload_offset,
        length: content_length,
        chunk_path,
    };

    chunk_upload.chunks.push(chunk_info);

    // Check if upload is complete
    let total_received: u64 = chunk_upload.chunks.iter().map(|c| c.length).sum();
    let progress_percent = (total_received as f64 / upload_length as f64 * 100.0) as u8;
    
    info!("📈 Chunk upload progress: {}/{} bytes ({}%) - {} chunks", 
          total_received, upload_length, progress_percent, chunk_upload.chunks.len());
    
    if total_received >= upload_length {
        // Upload is complete, reconstruct the final blob
        if total_received > upload_length {
            tracing::warn!("⚠️  Received more data than expected: {} > {} (possible overlapping chunks)", total_received, upload_length);
        }
        info!("🎉 Upload complete! Reconstructing final blob for {} (received: {}, expected: {})", sha256, total_received, upload_length);
        
        // Clone the chunk upload data and release the lock before reconstruction
        let chunk_upload_data = chunk_upload.clone();
        drop(chunk_uploads);
        
        let result = reconstruct_final_blob(&state, &chunk_upload_data, sha256).await;
        
        match result {
            Ok(descriptor) => {
                // Remove from chunk uploads after reconstruction
                {
                    let mut chunk_uploads = state.chunk_uploads.write().await;
                    chunk_uploads.remove(sha256);
                }
                
                // Track upload statistics
                track_upload_stats(&state, upload_length).await;

                // Queue cleanup job
                let mut changes_pending = state.changes_pending.write().await;
                *changes_pending = true;

                info!("🎯 Successfully reconstructed blob: {} ({} bytes) - URL: {}", 
                      sha256, descriptor.size, descriptor.url);
                info!("🏁 Chunked upload completed successfully: {} in {} chunks", sha256, chunk_upload_data.chunks.len());
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
                    .unwrap())
            }
            Err(e) => {
                error!("Failed to reconstruct final blob: {}", e);
                {
                    let mut chunk_uploads = state.chunk_uploads.write().await;
                    chunk_uploads.remove(sha256);
                }
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        // Upload not complete, return 204 No Content
        let remaining_bytes = upload_length - total_received;
        let remaining_percent = (remaining_bytes as f64 / upload_length as f64 * 100.0) as u8;
        info!("⏳ Chunk accepted for upload {}: {} bytes remaining ({}%) - {} chunks received", 
              sha256, remaining_bytes, remaining_percent, chunk_upload.chunks.len());
        Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap())
    }
}

/// Validate authorization event for chunk uploads
async fn validate_chunk_upload_auth(event: &Event, sha256: &str, chunk_length: &str) -> bool {
    // Check if the event has a 't' tag set to 'upload'
    let t_tag = event.tags.find(TagKind::Custom("t".into()));
    if t_tag.is_none() || t_tag.unwrap().content() != Some("upload") {
        error!("Missing or invalid 't' tag for chunk upload");
        return false;
    }

    // Check if the event has an 'x' tag with the chunk hash
    let x_tags: Vec<_> = event.tags.iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    if x_tags.is_empty() {
        error!("No 'x' tags found for chunk upload");
        return false;
    }

    // For chunk uploads, we need to validate that the chunk hash is in the x tags
    // and that the final blob hash is also present
    let mut _has_chunk_hash = false;
    let mut has_final_hash = false;

    for x_tag in x_tags {
        if let Some(content) = x_tag.content() {
            if content == chunk_length {
                _has_chunk_hash = true;
            }
            if content == sha256 {
                has_final_hash = true;
            }
        }
    }
 /* ignore chunk x tags in auth for now 
 
    if !_has_chunk_hash {
        error!("Chunk hash {} not found in x tags", chunk_length);
        return false;
    }
*/
    if !has_final_hash {
        error!("Final blob hash {} not found in x tags", sha256);
        return false;
    }

    true
}

/// Reconstruct the final blob from chunks
async fn reconstruct_final_blob(
    state: &AppState,
    chunk_upload: &ChunkUpload,
    expected_sha256: &str,
) -> Result<BlobDescriptor, String> {
    info!("🔧 Starting blob reconstruction for {} ({} chunks, {} bytes)", 
          expected_sha256, chunk_upload.chunks.len(), chunk_upload.upload_length);
    
    // Sort chunks by offset
    let mut sorted_chunks = chunk_upload.chunks.clone();
    sorted_chunks.sort_by_key(|c| c.offset);
    
    info!("📋 Chunk order: {:?}", sorted_chunks.iter().map(|c| (c.offset, c.length)).collect::<Vec<_>>());

    // Validate that chunks cover the entire upload length
    let mut expected_offset = 0;
    for (i, chunk) in sorted_chunks.iter().enumerate() {
        if chunk.offset != expected_offset {
            return Err(format!("Gap in chunk coverage at offset {} (chunk {}: offset {}, length {})", 
                              expected_offset, i, chunk.offset, chunk.length));
        }
        expected_offset += chunk.length;
        info!("✅ Chunk {} validated: offset {}, length {} -> total: {}", 
              i, chunk.offset, chunk.length, expected_offset);
    }

    if expected_offset != chunk_upload.upload_length {
        return Err(format!("Chunks don't cover full upload length: {} vs {}", 
                           expected_offset, chunk_upload.upload_length));
    }
    
    info!("✅ All chunks validated successfully: {} bytes total", expected_offset);

    // Create temp file for reconstruction
    let temp_dir = state.upload_dir.join("temp");
    fs::create_dir_all(&temp_dir).await.map_err(|e| format!("Failed to create temp dir: {}", e))?;

    let temp_path = temp_dir.join(format!("reconstruct_{}", uuid::Uuid::new_v4()));
    info!("📁 Creating reconstruction temp file: {}", temp_path.display());
    let mut temp_file = File::create(&temp_path).await.map_err(|e| format!("Failed to create temp file: {}", e))?;

    // Write chunks to temp file in order
    let mut hasher = Sha256::new();
    let mut total_written = 0u64;
    
    for (i, chunk) in sorted_chunks.iter().enumerate() {
        info!("📖 Reading chunk {} from file: {} (offset: {}, length: {})", 
              i, chunk.chunk_path.display(), chunk.offset, chunk.length);
        
        // Read chunk data from file
        let mut chunk_file = File::open(&chunk.chunk_path).await.map_err(|e| format!("Failed to open chunk file: {}", e))?;
        let mut chunk_data = Vec::with_capacity(chunk.length as usize);
        tokio::io::AsyncReadExt::read_to_end(&mut chunk_file, &mut chunk_data).await.map_err(|e| format!("Failed to read chunk file: {}", e))?;
        
        // Validate chunk file size
        if chunk_data.len() as u64 != chunk.length {
            return Err(format!("Chunk file size mismatch: expected {}, got {}", chunk.length, chunk_data.len()));
        }
        
        temp_file.write_all(&chunk_data).await.map_err(|e| format!("Failed to write chunk: {}", e))?;
        hasher.update(&chunk_data);
        total_written += chunk_data.len() as u64;
        
        info!("✅ Chunk {} written to reconstruction file: {} bytes (total: {})", 
              i, chunk_data.len(), total_written);
    }

    temp_file.sync_all().await.map_err(|e| format!("Failed to sync temp file: {}", e))?;
    drop(temp_file);
    
    info!("📊 Reconstruction complete: {} bytes written to temp file", total_written);

    // Verify SHA256 hash of the reconstructed file matches the expected hash
    let calculated_sha256 = format!("{:x}", hasher.finalize());
    info!("🔍 SHA256 verification: calculated {} vs expected {}", calculated_sha256, expected_sha256);
    
    if calculated_sha256 != expected_sha256 {
        error!("❌ SHA256 mismatch: expected {}, got {}", expected_sha256, calculated_sha256);
        let _ = fs::remove_file(&temp_path).await; // Clean up temp file
        // Clean up chunk files on error
        for chunk in &sorted_chunks {
            let _ = fs::remove_file(&chunk.chunk_path).await;
        }
        return Err(format!("SHA256 mismatch: expected {}, got {}", expected_sha256, calculated_sha256));
    }
    
    info!("✅ SHA256 verification passed: {}", calculated_sha256);

    // Move to final location using the expected SHA256 (which we just verified matches)
    let extension = chunk_upload.upload_type
        .split('/')
        .next_back()
        .map(|s| s.to_string());
    
    let final_path = get_nested_path(&state.upload_dir, expected_sha256, extension.as_deref());
    info!("📁 Moving reconstructed file to final location: {}", final_path.display());
    
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    fs::rename(&temp_path, &final_path).await.map_err(|e| format!("Failed to move temp file: {}", e))?;
    info!("✅ File moved to final location: {}", final_path.display());

    // Clean up chunk files after successful reconstruction
    info!("🧹 Cleaning up {} chunk files", sorted_chunks.len());
    for (i, chunk) in sorted_chunks.iter().enumerate() {
        if let Err(e) = fs::remove_file(&chunk.chunk_path).await {
            error!("Failed to clean up chunk file {}: {}", chunk.chunk_path.display(), e);
        } else {
            info!("🗑️  Cleaned up chunk file {}: {}", i, chunk.chunk_path.display());
        }
    }

    // Add to file index using the expected SHA256 (which we just verified matches)
    let key = expected_sha256[..64.min(expected_sha256.len())].to_string();
    state.file_index.write().await.insert(
        key.clone(),
        FileMetadata {
            path: final_path.clone(),
            extension: extension.clone(),
            mime_type: Some(chunk_upload.upload_type.clone()),
            size: chunk_upload.upload_length,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    );

    // Create blob descriptor using the expected SHA256
    Ok(state.create_blob_descriptor(expected_sha256, chunk_upload.upload_length, Some(chunk_upload.upload_type.clone())))
}

/// Temp file guard for cleanup
struct TempFileGuard {
    path: std::path::PathBuf,
}

impl TempFileGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            if let Err(e) = std::fs::remove_file(&self.path) {
                error!(
                    "Failed to clean up temp file {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}

/// Validate Nostr authentication
async fn validate_nostr_auth(auth: &str, state: &AppState) -> Result<Event, StatusCode> {
    let auth_str = auth.to_string();

    if !auth_str.starts_with("Nostr ") {
        error!("Invalid Authorization header prefix");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let base64_str = &auth_str[6..]; // Remove "Nostr " prefix
    let decoded_bytes = STANDARD.decode(base64_str).map_err(|e| {
        error!("Failed to decode base64: {}", e);
        StatusCode::UNAUTHORIZED
    })?;

    let json_str = String::from_utf8(decoded_bytes).map_err(|e| {
        error!("Failed to convert to UTF-8: {}", e);
        StatusCode::UNAUTHORIZED
    })?;

    let event: Event = serde_json::from_str(&json_str).map_err(|e| {
        error!("Failed to parse event JSON: {}", e);
        StatusCode::UNAUTHORIZED
    })?;

    // Verify the event signature
    if let Err(e) = event.verify() {
        error!("Invalid event signature: {}", e);
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Check if the event is expired
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(expiration) = event.tags.find(TagKind::Expiration) {
        if let Some(exp_time) = expiration.content() {
            if let Ok(exp_time) = exp_time.parse::<u64>() {
                if now > exp_time {
                    error!("Event expired");
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
        }
    }

    // Check if the event kind is correct (24242 for upload)
    if event.kind != Kind::Custom(24242) {
        error!("Invalid event kind");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Check if pubkey is allowed or trusted
    if !state.allowed_pubkeys.is_empty() && !state.allowed_pubkeys.contains(&event.pubkey) {
        let trusted_pubkeys = state.trusted_pubkeys.read().await;
        if !trusted_pubkeys.contains_key(&event.pubkey) {
            error!("Pubkey not authorized");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    Ok(event)
}
