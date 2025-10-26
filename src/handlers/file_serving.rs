use axum::{
    body::Body,
    extract::{Path as AxumPath, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use futures_util::stream;
use futures_util::StreamExt;
use reqwest::{header as reqwest_header, Client};
use sha2::Digest;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::Notify,
};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, warn};

use crate::constants::*;
use crate::helpers::*;
use crate::models::AppState;
use crate::utils::{find_file, parse_range_header};

/// Handle file requests (GET/HEAD)
pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    // Extract range header for logging
    let range_header = req.headers().get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());

    info!("GET request for url: {} (range: {})", filename, range_header);

    if let Some(filename) = crate::utils::get_sha256_hash_from_filename(&filename) {
        debug!("Found file hash: {}", filename);

        match find_file(&state.file_index, &filename).await {
            Some(file_metadata) => {
                if req.method() == Method::HEAD {
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            header::CONTENT_TYPE,
                            file_metadata
                                .mime_type
                                .unwrap_or_else(|| DEFAULT_MIME_TYPE.into()),
                        )
                        .header(header::CONTENT_LENGTH, file_metadata.size)
                        .body(Body::empty())
                        .unwrap())
                } else {
                    // Track download statistics
                    track_download_stats(&state, file_metadata.size).await;
                    serve_file_with_range(file_metadata.path, req.headers().clone()).await
                }
            }
            None => {
                // File not found locally, check if we've already tried upstream servers recently
                let failed_lookups = state.failed_upstream_lookups.read().await;
                if let Some(failed_time) = failed_lookups.get(&filename) {
                    let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
                    if *failed_time > one_hour_ago {
                        debug!(
                            "File {} not found in upstream servers recently (cached), returning 404",
                            filename
                        );
                        return Err(StatusCode::NOT_FOUND);
                    }
                }
                drop(failed_lookups); // Release the read lock

                // File not found locally, try upstream servers
                debug!(
                    "File not found locally, checking upstream servers for: {}",
                    filename
                );
                match try_upstream_servers(&state, &filename, req.headers()).await {
                    Ok(response) => Ok(response),
                    Err(_) => {
                        // Add to failed lookups cache
                        let mut failed_lookups = state.failed_upstream_lookups.write().await;
                        failed_lookups.insert(filename.clone(), std::time::Instant::now());
                        debug!("Added {} to failed upstream lookups cache", filename);
                        Err(StatusCode::NOT_FOUND)
                    }
                }
            }
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Try to fetch file from upstream servers, stream it to client and save locally
async fn try_upstream_servers(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
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
        return proxy_request_to_upstream(state, filename, headers).await;
    }

    let client = Client::new();

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

                // Check if this is a range request - if so, proxy it directly
                if headers.get(header::RANGE).is_some() {
                    info!("Proxying range request directly from upstream for: {}", filename);
                    return proxy_upstream_response(response, &content_type, filename).await;
                }

                // For non-range requests, start the download process
                // Derive extension from content type
                let file_extension = mime_guess::get_mime_extensions_str(&content_type)
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
                            content_type.clone(),
                        ),
                    );
                    info!("Marked {} as being downloaded with shared state at {} (content-type: {}, extension: {})", filename, temp_path.display(), content_type, file_extension);
                }

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
) -> Result<Response<Body>, StatusCode> {
    info!("Proxying request to upstream for ongoing download: {}", filename);
    
    let client = Client::new();
    
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
    // ---- Header vom Upstream übernehmen
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

    // zwei unabhängige Handles
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

    // ---- Shared Fortschritt + Notify (passed as parameters)

    // ---- Downloader: liest reqwest-Stream → schreibt Datei, hash, progress++
    let mut hasher = sha2::Sha256::new();
    let mut body_size: u64 = 0;

    let written_len_dl = written_len.clone();
    let notify_dl = notify.clone();

    let mut chunks = upstream_resp.bytes_stream(); // echtes Streaming!

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

            // Fortschritt publizieren und Leser wecken
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
        // Wichtig: flushen, damit Leser alle Bytes sicher sieht
        writer.flush().await?;
        info!(
            "Download completed: {} total bytes, temp file flushed",
            body_size
        );
        std::io::Result::<(String, u64)>::Ok((format!("{:x}", hasher.finalize()), body_size))
    });

    // ---- Streamer: liest die wachsende Datei ohne den Downloader zu blocken
    // Use helper function to create the tailing stream
    let stream = create_tailing_stream(reader, written_len.clone(), notify.clone()).await;

    // ---- Response bauen (Streaming startet sofort)
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

    // ---- Nachlauf: Download abschließen, Datei finalisieren & indexieren
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

                // final path berechnen
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
                    },
                );
                info!("Successfully added file to index");

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
                // Es gibt neue Bytes; lese einen moderaten Block
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
                    // EOF erreicht, warten auf mehr Daten
                    notify.notified().await;
                    continue;
                }

                pos += n as u64;
                buf.truncate(n); // Nur die tatsächlich gelesenen Bytes

                return Some((
                    Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(buf)),
                    (reader, written_len, notify, pos),
                ));
            } else {
                // Warten bis Downloader mehr geschrieben hat
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

/// Serve file with range support
async fn serve_file_with_range(path: std::path::PathBuf, headers: HeaderMap) -> Result<Response, StatusCode> {
    use axum::http::header::RANGE;
    let range_header = headers.get(RANGE).and_then(|r| r.to_str().ok());
    
    debug!("Serving file: {} (range: {})", path.display(), range_header.unwrap_or("none"));

    let expires_dt = chrono::Utc::now() + chrono::Duration::days(365);
    let expires_str = expires_dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let expires_header = hyper::http::HeaderValue::from_str(&expires_str).unwrap();

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename);

    let mut file = File::open(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_size = metadata.len();

    if let Some(range_header) = range_header {
        if let Some(range) = parse_range_header(range_header, total_size) {
            let (start, end) = range;
            let length = end - start + 1;
            debug!("Serving range: bytes {}-{}/{} (length: {})", start, end, total_size, length);
            
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            let mime = mime_guess::from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or(DEFAULT_MIME_TYPE.into());

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
                .header(axum::http::header::EXPIRES, expires_header.clone())
                .header(header::CONTENT_DISPOSITION, content_disposition.clone())
                .body(body)
                .unwrap());
        }
    }

    info!("Serving full file: {} (size: {} bytes)", path.display(), total_size);
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            mime_guess::from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or(DEFAULT_MIME_TYPE.into()),
        )
        .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .header(axum::http::header::EXPIRES, expires_header.clone())
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .body(body)
        .unwrap())
}
