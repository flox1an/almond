use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use futures_util::stream;
use futures_util::StreamExt;
use reqwest::{header as reqwest_header, Client};
use sha2::Digest;
use serde_json::json;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::Notify,
};
use tracing::{error, info, warn};

use crate::constants::*;
use crate::helpers::*;
use crate::models::AppState;

/// Handle upstream servers requests
pub async fn get_upstream(
    State(state): State<AppState>,
    _headers: HeaderMap,
) -> Json<serde_json::Value> {
    let upstream_servers = &state.upstream_servers;
    
    let response = json!({
        "upstream_servers": upstream_servers,
        "count": upstream_servers.len(),
        "max_download_size_mb": state.max_upstream_download_size_mb
    });

    Json(response)
}

/// Try to fetch file from upstream servers, stream it to client and save locally
pub async fn try_upstream_servers(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    custom_servers: Option<&[String]>,
) -> Result<Response, StatusCode> {
    // Forward range requests to upstream servers
    if headers.get(header::RANGE).is_some() {
        info!("Range request detected, forwarding to upstream server");
    }

    // Check if this file is already being downloaded
    if state.ongoing_downloads.read().await.contains_key(filename) {
        info!(
            "File {} is already being downloaded, proxying request to upstream",
            filename
        );

        // Proxy the request to upstream while download is in progress
        return proxy_request_to_upstream(state, filename, headers, custom_servers).await;
    }

    let client = Client::new();

    // Try custom servers first if provided
    if let Some(servers) = custom_servers {
        info!("Trying {} custom server(s) first", servers.len());
        for origin_url in servers {
            info!("Trying custom server: {}", origin_url);
            let file_url = format!("{}/{}", origin_url.trim_end_matches('/'), filename);
            info!("Trying upstream server: {}", file_url);

        // Create request with all relevant headers for upstream servers
        let request = client.get(&file_url);
        let request = copy_headers_to_reqwest(headers, request);

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("Found file on custom origin server: {}", file_url);
                // Get content type from upstream response
                let content_type = extract_content_type_from_response(response.headers());
                // Check if this is a range request
                let has_range_header = headers.get(header::RANGE).is_some();
                
                if has_range_header {
                    info!("Range request detected for non-existent file {}, starting download from byte 0", filename);
                    // For range requests, we need to start a full download in the background
                    // while proxying the range request for immediate response
                    let full_request = client.get(&file_url);
                    let full_request = copy_headers_without_range(headers, full_request);
                    
                    match full_request.send().await {
                        Ok(full_response) if full_response.status().is_success() => {
                            info!("Starting full download from byte 0 for range request: {}", filename);
                            // Prepare download state
                            let (temp_path, _written_len, _notify) = prepare_download_state(state, filename, &content_type).await?;
                            // Start the download in the background
                            let state_clone = state.clone();
                            let file_url_clone = file_url.clone();
                            let filename_clone = filename.to_string();
                            let content_type_clone = content_type.clone();
                            let temp_path_clone = temp_path.clone();
                            tokio::spawn(async move {
                                download_file_from_upstream_background(
                                    &state_clone,
                                    &file_url_clone,
                                    full_response,
                                    &filename_clone,
                                    &content_type_clone,
                                    &temp_path_clone,
                                )
                                .await;
                            });
                            // Proxy the range request to upstream for immediate response
                            info!("Proxying range request to upstream while download starts in background: {}", filename);
                            return proxy_upstream_response(response, &content_type, filename).await;
                        }
                        Ok(_) | Err(_) => {
                            warn!("Failed to start full download for range request, proxying range request only: {}", filename);
                            return proxy_upstream_response(response, &content_type, filename).await;
                        }
                    }
                } else {
                    // For non-range requests, stream and save from upstream
                    info!("Non-range request, starting download and streaming to client: {}", filename);
                    // Prepare download state
                    let (temp_path, written_len, notify) = prepare_download_state(state, filename, &content_type).await?;
                    return stream_and_save_from_upstream(
                        state,
                        &file_url,
                        response,
                        filename,
                        written_len,
                        notify,
                        temp_path,
                    )
                    .await;
                }
            }
            Ok(response) => {
                info!("Custom server {} returned status: {}", file_url, response.status());
            }
            Err(e) => {
                warn!("Failed to fetch from custom server {}: {}", file_url, e);
            }
        }
        }
        // If all custom servers failed, continue to regular upstream servers
        info!("All custom servers failed, trying configured upstream servers");
    }

    // Try each upstream server
    for upstream_url in &state.upstream_servers {
        let file_url = format!("{}/{}", upstream_url.trim_end_matches('/'), filename);
        info!("Trying upstream server: {}", file_url);

        // Create request with all relevant headers for upstream servers
        let request = client.get(&file_url);
        let request = copy_headers_to_reqwest(headers, request);

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("Found file on upstream server: {}", file_url);

                // Get content type from upstream response
                let content_type = extract_content_type_from_response(response.headers());

                // Check if this is a range request
                let has_range_header = headers.get(header::RANGE).is_some();
                
                if has_range_header {
                    info!("Range request detected for non-existent file {}, starting download from byte 0", filename);
                    
                    // For range requests, we need to start a full download in the background
                    // while proxying the range request for immediate response
                    let full_request = client.get(&file_url);
                    let full_request = copy_headers_without_range(headers, full_request);
                    
                    match full_request.send().await {
                        Ok(full_response) if full_response.status().is_success() => {
                            info!("Starting full download from byte 0 for range request: {}", filename);
                            
                            // Prepare download state
                            let (temp_path, _written_len, _notify) = prepare_download_state(state, filename, &content_type).await?;

                            // Start the download in the background
                            let state_clone = state.clone();
                            let file_url_clone = file_url.clone();
                            let filename_clone = filename.to_string();
                            let content_type_clone = content_type.clone();
                            let temp_path_clone = temp_path.clone();
                            tokio::spawn(async move {
                                download_file_from_upstream_background(
                                    &state_clone,
                                    &file_url_clone,
                                    full_response,
                                    &filename_clone,
                                    &content_type_clone,
                                    &temp_path_clone,
                                )
                                .await;
                            });

                            // Proxy the range request to upstream for immediate response
                            info!("Proxying range request to upstream while download starts in background: {}", filename);
                            return proxy_upstream_response(response, &content_type, filename).await;
                        }
                        Ok(_) | Err(_) => {
                            // If we can't get the full file, fall back to proxying the range request
                            warn!("Failed to start full download for range request, proxying range request only: {}", filename);
                            return proxy_upstream_response(response, &content_type, filename).await;
                        }
                    }
                } else {
                    // For non-range requests, stream and save from upstream
                    info!("Non-range request, starting download and streaming to client: {}", filename);
                    
                    // Prepare download state
                    let (temp_path, written_len, notify) = prepare_download_state(state, filename, &content_type).await?;

                    return stream_and_save_from_upstream(
                        state,
                        &file_url,
                        response,
                        filename,
                        written_len,
                        notify,
                        temp_path,
                    )
                    .await;
                }
            }
            Ok(response) => {
                info!(
                    "Upstream server {} returned status: {}",
                    file_url,
                    response.status()
                );
            }
            Err(e) => {
                warn!("Failed to fetch from upstream {}: {}", file_url, e);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Proxy request to upstream server while download is in progress
async fn proxy_request_to_upstream(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    custom_servers: Option<&[String]>,
) -> Result<Response<Body>, StatusCode> {
    info!("Proxying request to upstream for ongoing download: {}", filename);

    let client = Client::new();

    // Try custom servers first if provided
    if let Some(servers) = custom_servers {
        info!("Trying {} custom server(s) for proxying", servers.len());
        for origin_url in servers {
            let file_url = format!("{}/{}", origin_url.trim_end_matches('/'), filename);
            info!("Proxying to custom server: {}", file_url);

        // Create request with all relevant headers
        let request = client.get(&file_url);
        let request = copy_headers_to_reqwest(headers, request);

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("Successfully proxied request to custom origin: {}", file_url);
                
                // Get content type from upstream response
                let content_type = extract_content_type_from_response(response.headers());

                return proxy_upstream_response(response, &content_type, filename).await;
            }
            Ok(response) => {
                info!(
                    "Custom server {} returned status: {}",
                    file_url,
                    response.status()
                );
            }
            Err(e) => {
                warn!("Failed to proxy to custom server {}: {}", file_url, e);
            }
        }
        }
    }
    
    // Try each upstream server
    for upstream_url in &state.upstream_servers {
        let file_url = format!("{}/{}", upstream_url.trim_end_matches('/'), filename);
        info!("Proxying to upstream server: {}", file_url);

        // Create request with all relevant headers
        let request = client.get(&file_url);
        let request = copy_headers_to_reqwest(headers, request);

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("Successfully proxied request to upstream: {}", file_url);
                
                // Get content type from upstream response
                let content_type = extract_content_type_from_response(response.headers());

                return proxy_upstream_response(response, &content_type, filename).await;
            }
            Ok(response) => {
                info!(
                    "Upstream server {} returned status: {}",
                    file_url,
                    response.status()
                );
            }
            Err(e) => {
                warn!("Failed to proxy to upstream {}: {}", file_url, e);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Proxy upstream response directly to client
async fn proxy_upstream_response(
    response: reqwest::Response,
    content_type: &str,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    // Extract range info from upstream response for logging
    let content_range = response.headers().get(reqwest_header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());
    
    info!("Proxying upstream response for: {} (content-type: {}, range: {})", 
          filename, content_type, content_range);
    
    let status = if response.status().is_success() {
        if response.headers().get(reqwest_header::CONTENT_RANGE).is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        }
    } else {
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK)
    };

    // Get all relevant headers before consuming the response
    let content_range = response.headers().get(reqwest_header::CONTENT_RANGE).cloned();
    let content_length = response.headers().get(reqwest_header::CONTENT_LENGTH).cloned();
    let accept_ranges = response.headers().get(reqwest_header::ACCEPT_RANGES).cloned();
    let cache_control = response.headers().get(reqwest_header::CACHE_CONTROL).cloned();
    let etag = response.headers().get(reqwest_header::ETAG).cloned();
    let last_modified = response.headers().get(reqwest_header::LAST_MODIFIED).cloned();

    // Stream the response directly to client
    let body = Body::from_stream(response.bytes_stream());
    let mut response_builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");

    // Copy all relevant headers from upstream
    if let Some(content_range) = content_range {
        response_builder = response_builder.header(header::CONTENT_RANGE, content_range);
    }
    if let Some(content_length) = content_length {
        response_builder = response_builder.header(header::CONTENT_LENGTH, content_length);
    }
    if let Some(accept_ranges) = accept_ranges {
        response_builder = response_builder.header(header::ACCEPT_RANGES, accept_ranges);
    }
    if let Some(cache_control) = cache_control {
        response_builder = response_builder.header(header::CACHE_CONTROL, cache_control);
    }
    if let Some(etag) = etag {
        response_builder = response_builder.header(header::ETAG, etag);
    }
    if let Some(last_modified) = last_modified {
        response_builder = response_builder.header(header::LAST_MODIFIED, last_modified);
    }

    Ok(response_builder.body(body).unwrap())
}

/// Prepare download state and return metadata for either streaming or background download
async fn prepare_download_state(
    state: &AppState,
    filename: &str,
    content_type: &str,
) -> Result<(std::path::PathBuf, Arc<AtomicU64>, Arc<Notify>), StatusCode> {
    // Derive extension from content type
    let file_extension = mime_guess::get_mime_extensions_str(content_type)
        .and_then(|exts| exts.first().map(|ext| format!(".{}", ext)))
        .unwrap_or_default();

    // Create temp file with proper extension derived from content type
    let temp_dir = state.upload_dir.join("temp");
    let temp_filename = format!("upstream_{}{}", uuid::Uuid::new_v4(), file_extension);
    let temp_path = temp_dir.join(temp_filename);

    // Mark this file as being downloaded with shared state
    let written_len = Arc::new(AtomicU64::new(0));
    let notify = Arc::new(Notify::new());
    {
        let mut ongoing_downloads = state.ongoing_downloads.write().await;
        ongoing_downloads.insert(
            filename.to_string(),
            (
                std::time::Instant::now(),
                written_len.clone(),
                notify.clone(),
                temp_path.clone(),
                content_type.to_string(),
            ),
        );
        info!("Marked {} as being downloaded with shared state at {} (content-type: {}, extension: {})", 
              filename, temp_path.display(), content_type, file_extension);
    }

    Ok((temp_path, written_len, notify))
}

/// Stream file from upstream server to client while saving to local storage
async fn stream_and_save_from_upstream(
    state: &AppState,
    file_url: &str,
    upstream_resp: reqwest::Response,
    filename: &str,
    written_len: Arc<AtomicU64>,
    notify: Arc<Notify>,
    temp_path: std::path::PathBuf,
) -> Result<Response<Body>, StatusCode> {
    // ---- Take headers from upstream
    let content_type = upstream_resp
        .headers()
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string();

    let content_length = upstream_resp.content_length();

    // Check size limit before starting download
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024; // Convert MB to bytes
    if let Some(content_length) = content_length {
        if content_length > max_size_bytes {
            error!(
                "Upstream file {} too large: {} bytes (max allowed: {} bytes / {} MB)",
                file_url, content_length, max_size_bytes, state.max_upstream_download_size_mb
            );
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        info!(
            "Upstream file size check passed: {} bytes (limit: {} MB)",
            content_length, state.max_upstream_download_size_mb
        );
    } else {
        warn!(
            "Upstream file {} has no Content-Length header, proceeding with download (limit: {} MB)",
            file_url, state.max_upstream_download_size_mb
        );
    }

    let extension = content_type.split('/').next_back().map(|s| s.to_string());

    info!(
        "Starting download from upstream: {} to temp file: {}",
        file_url,
        temp_path.display()
    );

    // Ensure temp directory exists
    if let Some(parent) = temp_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            error!("create temp dir: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    // two independent handles
    info!("Creating writer file handle...");
    let mut writer = File::create(&temp_path).await.map_err(|e| {
        error!("create temp file: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    info!("✅ Writer file handle created successfully");

    // Check if file was actually created
    if !temp_path.exists() {
        error!("Temp file was not created: {}", temp_path.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    info!(
        "✅ Temp file exists after creation: {}",
        temp_path.display()
    );

    info!("Creating reader file handle...");
    let reader = File::open(&temp_path).await.map_err(|e| {
        error!("open temp file for read: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    info!("✅ Reader file handle created successfully");

    // ---- Shared progress + Notify (passed as parameters)

    // ---- Downloader: reads reqwest stream → writes file, hash, progress++
    let mut hasher = sha2::Sha256::new();
    let mut body_size: u64 = 0;

    let written_len_dl = written_len.clone();
    let notify_dl = notify.clone();

    let mut chunks = upstream_resp.bytes_stream(); // real streaming!

    let max_size_bytes_clone = max_size_bytes;
    let download_task = tokio::spawn(async move {
        info!("Download task started, beginning to read from upstream stream");

        while let Some(next) = chunks.next().await {
            let chunk =
                next.map_err(|e| std::io::Error::other(e.to_string()))?;

            // Check size limit during download (in case Content-Length was missing or wrong)
            let new_size = body_size + chunk.len() as u64;
            if new_size > max_size_bytes_clone {
                error!(
                    "Download exceeded size limit: {} bytes > {} bytes ({} MB limit)",
                    new_size,
                    max_size_bytes_clone,
                    max_size_bytes_clone / (1024 * 1024)
                );
                return Err(std::io::Error::other(format!(
                    "File too large: {} bytes exceeds limit of {} MB",
                    new_size,
                    max_size_bytes_clone / (1024 * 1024)
                )));
            }

            writer.write_all(&chunk).await?;
            hasher.update(&chunk);
            body_size += chunk.len() as u64;

            // publish progress and wake up readers
            written_len_dl.fetch_add(chunk.len() as u64, Ordering::Release);
            notify_dl.notify_waiters();

            // Log progress every 1MB
            if body_size.is_multiple_of(1024 * 1024) {
                info!(
                    "Download progress: {} bytes written to temp file (limit: {} MB)",
                    body_size,
                    max_size_bytes_clone / (1024 * 1024)
                );
            }
        }

        info!("Upstream stream finished, flushing temp file");
        // Important: flush so readers can safely see all bytes
        writer.flush().await?;
        info!(
            "Download completed: {} total bytes, temp file flushed",
            body_size
        );
        std::io::Result::<(String, u64)>::Ok((format!("{:x}", hasher.finalize()), body_size))
    });

    // ---- Streamer: reads the growing file without blocking the downloader
    // Use helper function to create the tailing stream
    let stream = create_tailing_stream(reader, written_len.clone(), notify.clone()).await;

    // ---- Build response (streaming starts immediately)
    info!("🚀 Starting immediate streaming to client (download runs in background)");
    let body = Body::from_stream(stream);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap();

    // Apply streaming headers
    response = apply_streaming_headers(response, &content_type, filename);

    // Add Content-Length if available from upstream
    if let Some(len) = content_length {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
    }

    // ---- Cleanup: complete download, finalize file & index
    let state_clone = state.clone();
    let content_type_clone = content_type.clone();
    let extension_clone = extension.clone();
    let file_url_clone = file_url.to_string();
    let filename_clone = filename.to_string();
    tokio::spawn(async move {
        info!("Waiting for download task to complete...");
        match download_task.await {
            Ok(Ok((sha256, total))) => {
                info!("Download task completed successfully, finalizing file");
                info!("SHA256: {}", sha256);
                info!("Total bytes: {}", total);

                // calculate final path
                let final_path =
                    crate::utils::get_nested_path(&state_clone.upload_dir, &sha256, extension_clone.as_deref());
                info!(
                    "Moving temp file {} to final location: {}",
                    temp_path.display(),
                    final_path.display()
                );

                if let Some(parent) = final_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                if let Err(e) = tokio::fs::rename(&temp_path, &final_path).await {
                    error!("rename temp -> final failed: {e}");
                    // Clean up temp file on error
                    let _ = std::fs::remove_file(&temp_path);
                    return;
                }
                info!("Successfully moved temp file to final location");

                // Index & Stats
                let key = sha256[..sha256.len().min(64)].to_string();
                info!("Adding file to index with key: {}", key);
                state_clone.file_index.write().await.insert(
                    key.clone(),
                    crate::models::FileMetadata {
                        path: final_path,
                        extension: extension_clone,
                        mime_type: Some(content_type_clone),
                        size: total,
                        created_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        pubkey: None,
                    },
                );
                info!("Successfully added file to index");

                // Mark that changes are pending for storage limit enforcement
                let mut changes_pending = state_clone.changes_pending.write().await;
                *changes_pending = true;

                let mut n = state_clone.files_downloaded.write().await;
                *n += 1;
                let mut t = state_clone.download_throughput_data.write().await;
                t.push((std::time::Instant::now(), total));
                if t.len() > MAX_THROUGHPUT_ENTRIES {
                    t.drain(0..THROUGHPUT_CLEANUP_THRESHOLD);
                }

                info!(
                    "✅ UPSTREAM DOWNLOAD COMPLETED: {} -> {} ({} bytes)",
                    file_url_clone, sha256, total
                );

                // Remove from ongoing downloads
                {
                    let mut ongoing_downloads = state_clone.ongoing_downloads.write().await;
                    ongoing_downloads.remove(&filename_clone);
                    info!("Removed {} from ongoing downloads", filename_clone);
                }
            }
            Ok(Err(e)) => {
                error!("❌ Download task failed: {e}");
                // Remove from ongoing downloads on error too
                {
                    let mut ongoing_downloads = state_clone.ongoing_downloads.write().await;
                    ongoing_downloads.remove(&filename_clone);
                    info!(
                        "Removed {} from ongoing downloads due to error",
                        filename_clone
                    );
                }
            }
            Err(e) => {
                error!("❌ Join error: {e}");
                // Remove from ongoing downloads on error too
                {
                    let mut ongoing_downloads = state_clone.ongoing_downloads.write().await;
                    ongoing_downloads.remove(&filename_clone);
                    info!(
                        "Removed {} from ongoing downloads due to join error",
                        filename_clone
                    );
                }
            }
        }
    });

    Ok(response)
}

/// Download file from upstream in background (without streaming to client)
async fn download_file_from_upstream_background(
    state: &AppState,
    file_url: &str,
    upstream_resp: reqwest::Response,
    filename: &str,
    content_type: &str,
    temp_path: &std::path::PathBuf,
) {
    let content_length = upstream_resp.content_length();

    // Check size limit before starting download
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    if let Some(content_length) = content_length {
        if content_length > max_size_bytes {
            error!(
                "Upstream file {} too large: {} bytes (max allowed: {} bytes / {} MB)",
                file_url, content_length, max_size_bytes, state.max_upstream_download_size_mb
            );
            // Remove from ongoing downloads
            let mut ongoing_downloads = state.ongoing_downloads.write().await;
            ongoing_downloads.remove(filename);
            return;
        }
    }

    let extension = content_type.split('/').next_back().map(|s| s.to_string());

    info!(
        "Starting background download from upstream: {} to temp file: {}",
        file_url,
        temp_path.display()
    );

    // Ensure temp directory exists
    if let Some(parent) = temp_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("create temp dir: {e}");
            let mut ongoing_downloads = state.ongoing_downloads.write().await;
            ongoing_downloads.remove(filename);
            return;
        }
    }

    // Create file for writing
    let mut writer = match File::create(temp_path).await {
        Ok(w) => w,
        Err(e) => {
            error!("create temp file: {e}");
            let mut ongoing_downloads = state.ongoing_downloads.write().await;
            ongoing_downloads.remove(filename);
            return;
        }
    };

    // Download and save
    let mut hasher = sha2::Sha256::new();
    let mut body_size: u64 = 0;
    let mut chunks = upstream_resp.bytes_stream();

    while let Some(next) = chunks.next().await {
        let chunk = match next {
            Ok(c) => c,
            Err(e) => {
                error!("Error reading chunk: {e}");
                let mut ongoing_downloads = state.ongoing_downloads.write().await;
                ongoing_downloads.remove(filename);
                return;
            }
        };

        // Check size limit during download
        let new_size = body_size + chunk.len() as u64;
        if new_size > max_size_bytes {
            error!(
                "Download exceeded size limit: {} bytes > {} bytes ({} MB limit)",
                new_size,
                max_size_bytes,
                max_size_bytes / (1024 * 1024)
            );
            let mut ongoing_downloads = state.ongoing_downloads.write().await;
            ongoing_downloads.remove(filename);
            return;
        }

        if let Err(e) = writer.write_all(&chunk).await {
            error!("Error writing chunk: {e}");
            let mut ongoing_downloads = state.ongoing_downloads.write().await;
            ongoing_downloads.remove(filename);
            return;
        }

        hasher.update(&chunk);
        body_size += chunk.len() as u64;
    }

    // Flush and finalize
    if let Err(e) = writer.flush().await {
        error!("Error flushing file: {e}");
        let mut ongoing_downloads = state.ongoing_downloads.write().await;
        ongoing_downloads.remove(filename);
        return;
    }

    let sha256 = format!("{:x}", hasher.finalize());
    info!("Background download completed: {} bytes, SHA256: {}", body_size, sha256);

    // Move to final location
    let final_path = crate::utils::get_nested_path(&state.upload_dir, &sha256, extension.as_deref());
    if let Some(parent) = final_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    if let Err(e) = tokio::fs::rename(temp_path, &final_path).await {
        error!("rename temp -> final failed: {e}");
        let _ = std::fs::remove_file(temp_path);
        let mut ongoing_downloads = state.ongoing_downloads.write().await;
        ongoing_downloads.remove(filename);
        return;
    }

    // Update index
    let key = sha256[..sha256.len().min(64)].to_string();
    state.file_index.write().await.insert(
        key.clone(),
        crate::models::FileMetadata {
            path: final_path,
            extension,
            mime_type: Some(content_type.to_string()),
            size: body_size,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            pubkey: None,
        },
    );

    // Mark that changes are pending for storage limit enforcement
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    // Update stats
    let mut n = state.files_downloaded.write().await;
    *n += 1;
    let mut t = state.download_throughput_data.write().await;
    t.push((std::time::Instant::now(), body_size));
    if t.len() > MAX_THROUGHPUT_ENTRIES {
        t.drain(0..THROUGHPUT_CLEANUP_THRESHOLD);
    }

    info!(
        "✅ BACKGROUND UPSTREAM DOWNLOAD COMPLETED: {} -> {} ({} bytes)",
        file_url, sha256, body_size
    );

    // Remove from ongoing downloads
    let mut ongoing_downloads = state.ongoing_downloads.write().await;
    ongoing_downloads.remove(filename);
}

/// Create a streaming response that reads from a growing file
async fn create_tailing_stream(
    reader: File,
    written_len: Arc<AtomicU64>,
    notify: Arc<Notify>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    stream::unfold((reader, written_len, notify, 0u64), |state| async move {
        let (mut reader, written_len, notify, mut pos) = state;

        loop {
            let available = written_len.load(Ordering::Acquire);
            if pos < available {
                // There are new bytes; read a moderate block
                let to_read = std::cmp::min(64 * 1024, (available - pos) as usize);
                let mut buf = vec![0u8; to_read];

                // Seek to current position and read
                if reader.seek(SeekFrom::Start(pos)).await.is_err() {
                    return None;
                }
                let n = match reader.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return None,
                };

                if n == 0 {
                    // EOF reached, wait for more data
                    notify.notified().await;
                    continue;
                }

                pos += n as u64;
                buf.truncate(n); // Only the actually read bytes

                return Some((
                    Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(buf)),
                    (reader, written_len, notify, pos),
                ));
            } else {
                // Wait until downloader has written more
                notify.notified().await;
            }
        }
    })
}

/// Apply streaming headers to an existing response
fn apply_streaming_headers(
    mut response: Response<Body>,
    content_type: &str,
    filename: &str,
) -> Response<Body> {
    let headers = response.headers_mut();

    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        CACHE_CONTROL_IMMUTABLE.parse().unwrap(),
    );
    headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());

    // Add Content-Disposition header to prevent save dialog
    let filename_display = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename_display);
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition.parse().unwrap(),
    );

    info!(
        "Applied streaming headers: Content-Type={}, Content-Disposition=inline; filename=\"{}\"",
        content_type, filename_display
    );

    response
}
