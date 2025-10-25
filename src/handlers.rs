use axum::body::to_bytes;
use axum::http::header::{CACHE_CONTROL, EXPIRES};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use chrono::{Duration, Utc};
use futures_util::stream;
use futures_util::StreamExt;
use hyper::http::HeaderValue;
use mime_guess::from_path;
use nostr_relay_pool::prelude::*;
use reqwest::{header as reqwest_header, Client};
use serde_json::{self, Value};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::Notify,
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};
use uuid;

use crate::models::{AppState, BlobDescriptor, ChunkInfo, ChunkUpload, FileMetadata, ListQuery, Stats};
use crate::utils::{find_file, get_nested_path, get_sha256_hash_from_filename, parse_range_header};

pub async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<BlobDescriptor>>, (StatusCode, String)> {
    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header".to_string(),
        )
    })?;

    let _event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "Invalid Authorization header format".to_string(),
            )
        })?,
        &state,
    )
    .await
    .map_err(|e| (e, "Invalid Nostr authorization".to_string()))?;

    let index = state.file_index.read().await;
    let mut blobs = Vec::new();

    for (sha256, metadata) in index.iter() {
        let timestamp = metadata.created_at;

        if let Some(since) = params.since {
            if timestamp < since {
                continue;
            }
        }
        if let Some(until) = params.until {
            if timestamp > until {
                continue;
            }
        }

        let url = match metadata.extension.clone() {
            Some(ext) => format!("{}/{}.{}", state.public_url, sha256, ext),
            None => format!("{}/{}", state.public_url, sha256),
        };

        blobs.push(BlobDescriptor {
            url,
            sha256: sha256.clone(),
            size: metadata.size,
            r#type: metadata.mime_type.clone(),
            uploaded: timestamp,
        });
    }

    Ok(Json(blobs))
}

pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    info!("get for url: {}", filename);

    if let Some(filename) = get_sha256_hash_from_filename(&filename) {
        info!("Found file: {}", filename);

        match find_file(&state.file_index, &filename).await {
            Some(file_metadata) => {
                if req.method() == Method::HEAD {
                    return Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            header::CONTENT_TYPE,
                            file_metadata
                                .mime_type
                                .unwrap_or_else(|| "application/octet-stream".into()),
                        )
                        .header(header::CONTENT_LENGTH, file_metadata.size)
                        .body(Body::empty())
                        .unwrap());
                } else {
                    // Track download statistics
                    {
                        let mut files_downloaded = state.files_downloaded.write().await;
                        *files_downloaded += 1;

                        // Track download throughput
                        let mut download_throughput_data =
                            state.download_throughput_data.write().await;
                        download_throughput_data
                            .push((std::time::Instant::now(), file_metadata.size));

                        // Keep only last 1000 entries to prevent memory bloat
                        if download_throughput_data.len() > 1000 {
                            download_throughput_data.drain(0..100);
                        }
                    }
                    return serve_file_with_range(file_metadata.path, req.headers().clone()).await;
                }
            }
            None => {
                // File not found locally, try upstream servers
                info!(
                    "File not found locally, checking upstream servers for: {}",
                    filename
                );
                match try_upstream_servers(&state, &filename, req.headers()).await {
                    Ok(response) => Ok(response),
                    Err(_) => Err(StatusCode::NOT_FOUND),
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
    // For upstream servers, ignore range requests and return full file
    if headers.get(header::RANGE).is_some() {
        info!("Range request detected for upstream server, ignoring range and returning full file");
    }

    // Check if this file is already being downloaded
    if let Some((written_len, notify, temp_path, content_type)) = {
        let ongoing_downloads = state.ongoing_downloads.read().await;
        ongoing_downloads
            .get(filename)
            .map(|(_, written_len, notify, temp_path, content_type)| {
                (
                    written_len.clone(),
                    notify.clone(),
                    temp_path.clone(),
                    content_type.clone(),
                )
            })
    } {
        info!(
            "File {} is already being downloaded, joining the stream immediately",
            filename
        );

        // Join the existing download stream immediately
        return stream_from_ongoing_download(
            state,
            filename,
            headers,
            written_len,
            notify,
            temp_path,
            content_type,
        )
        .await;
    }

    let client = Client::new();

    // Try each upstream server
    for upstream_url in &state.upstream_servers {
        let file_url = format!("{}/{}", upstream_url.trim_end_matches('/'), filename);
        info!("Trying upstream server: {}", file_url);

        // Create request without range headers for upstream servers
        let mut request = client.get(&file_url);

        // Copy relevant headers from original request, but exclude range headers
        if let Some(user_agent) = headers.get(header::USER_AGENT) {
            request = request.header(header::USER_AGENT, user_agent);
        }
        if let Some(accept) = headers.get(header::ACCEPT) {
            request = request.header(header::ACCEPT, accept);
        }
        // Explicitly exclude RANGE header - we want the full file

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("Found file on upstream server: {}", file_url);

                // Get content type from upstream response
                let content_type = response
                    .headers()
                    .get(reqwest_header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_string();

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

/// Stream from an ongoing download (for subsequent requests)
async fn stream_from_ongoing_download(
    _state: &AppState,
    filename: &str,
    _headers: &HeaderMap,
    written_len: Arc<AtomicU64>,
    notify: Arc<Notify>,
    temp_path: PathBuf,
    content_type: String,
) -> Result<Response<Body>, StatusCode> {
    info!(
        "Streaming immediately from ongoing download for: {}",
        filename
    );

    // Create a reader for the temp file
    let reader = File::open(&temp_path).await.map_err(|e| {
        error!("Failed to open temp file for streaming: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Create streaming response using helper function
    let stream = create_tailing_stream(reader, written_len, notify).await;

    info!(
        "Using stored content type: {} for ongoing download: {}",
        content_type, filename
    );

    // Build response with proper headers using the stored content type
    let body = Body::from_stream(stream);
    let mut response = build_streaming_response_headers(&content_type, filename);

    // Replace the placeholder body with the actual stream
    *response.body_mut() = body;

    info!(
        "🚀 Started immediate streaming from ongoing download for: {}",
        filename
    );
    Ok(response)
}

/// Stream file from upstream server to client while saving to local storage
async fn stream_and_save_from_upstream(
    state: &AppState,
    file_url: &str,
    upstream_resp: reqwest::Response,
    filename: &str,
    written_len: Arc<AtomicU64>,
    notify: Arc<Notify>,
    temp_path: PathBuf,
) -> Result<Response<Body>, StatusCode> {
    // ---- Header vom Upstream übernehmen
    let content_type = upstream_resp
        .headers()
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let content_length = upstream_resp.content_length();

    // Check size limit before starting download
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024; // Convert MB to bytes
//TODO this check should already be done in the try_upstream_servers function
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

    let extension = content_type.split('/').last().map(|s| s.to_string());

    info!(
        "Starting download from upstream: {} to temp file: {}",
        file_url,
        temp_path.display()
    );

    // Ensure temp directory exists
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
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
    let mut hasher = Sha256::new();
    let mut body_size: u64 = 0;

    let written_len_dl = written_len.clone();
    let notify_dl = notify.clone();

    let mut chunks = upstream_resp.bytes_stream(); // echtes Streaming!

    let max_size_bytes_clone = max_size_bytes;
    let download_task = tokio::spawn(async move {
        info!("Download task started, beginning to read from upstream stream");

        while let Some(next) = chunks.next().await {
            let chunk =
                next.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            // Check size limit during download (in case Content-Length was missing or wrong)
            let new_size = body_size + chunk.len() as u64;
            if new_size > max_size_bytes_clone {
                error!(
                    "Download exceeded size limit: {} bytes > {} bytes ({} MB limit)",
                    new_size,
                    max_size_bytes_clone,
                    max_size_bytes_clone / (1024 * 1024)
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "File too large: {} bytes exceeds limit of {} MB",
                        new_size,
                        max_size_bytes_clone / (1024 * 1024)
                    ),
                ));
            }

            writer.write_all(&chunk).await?;
            hasher.update(&chunk);
            body_size += chunk.len() as u64;

            // Fortschritt publizieren und Leser wecken
            written_len_dl.fetch_add(chunk.len() as u64, Ordering::Release);
            notify_dl.notify_waiters();

            // Log progress every 1MB
            if body_size % (1024 * 1024) == 0 {
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
                    get_nested_path(&state_clone.upload_dir, &sha256, extension_clone.as_deref());
                info!(
                    "Moving temp file {} to final location: {}",
                    temp_path.display(),
                    final_path.display()
                );

                if let Some(parent) = final_path.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                if let Err(e) = fs::rename(&temp_path, &final_path).await {
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
                    FileMetadata {
                        path: final_path,
                        extension: extension_clone,
                        mime_type: Some(content_type_clone),
                        size: total,
                        created_at: SystemTime::now()
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
                if t.len() > 1000 {
                    t.drain(0..100);
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

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
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

    // info!("Decoded Nostr event JSON: {}", json_str);

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

pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
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

    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

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
    const LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks

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
    {
        let mut files_uploaded = state.files_uploaded.write().await;
        *files_uploaded += 1;

        let mut upload_throughput_data = state.upload_throughput_data.write().await;
        upload_throughput_data.push((std::time::Instant::now(), total_bytes as u64));

        // Keep only last 1000 entries to prevent memory bloat
        if upload_throughput_data.len() > 1000 {
            upload_throughput_data.drain(0..100);
        }
    }

    let descriptor = state.create_blob_descriptor(&sha256, size, content_type);

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

pub async fn mirror_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        error!("Missing Authorization header");
        StatusCode::UNAUTHORIZED
    })?;

    let _event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            error!("Invalid Authorization header format");
            StatusCode::UNAUTHORIZED
        })?,
        &state,
    )
    .await?;

    let body_bytes = to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;

    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or(StatusCode::BAD_REQUEST)?;

    // Extract expected SHA256 from the URL (assuming it's part of the URL)
    let expected_sha256 = url
        .split('/')
        .last()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");

    info!("Starting to mirror blob from URL: {}", url);

    let client = Client::new();

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    if !response.status().is_success() {
        warn!("Failed to download blob, status: {}", response.status());
        return Err(StatusCode::BAD_REQUEST);
    }

    info!("Successfully downloaded blob from URL: {}", url);

    let content_type = response
        .headers()
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let blob_bytes = response
        .bytes()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut hasher = Sha256::new();
    hasher.update(&blob_bytes);
    let sha256 = format!("{:x}", hasher.finalize());

    // Validate the SHA256 hash
    if sha256 != expected_sha256 {
        error!(
            "SHA256 mismatch: expected {}, got {}",
            expected_sha256, sha256
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!("Calculated SHA256: {}", sha256);

    let extension = content_type.split('/').last().unwrap_or("bin");
    let filepath = get_nested_path(&state.upload_dir, &sha256, Some(extension));

    // Create parent directories if they don't exist
    if let Some(parent) = filepath.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            error!("Failed to create directory: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    info!("Saving blob to: {}", filepath.display());

    let mut file = File::create(&filepath)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.write_all(&blob_bytes)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    info!("Blob saved successfully to: {}", filepath.display());

    let content_type_clone = content_type.clone();
    let extension = content_type_clone.split('/').last().map(|s| s.to_string());
    let descriptor_content_type = content_type_clone.clone();
    let descriptor = state.create_blob_descriptor(
        &sha256,
        blob_bytes.len() as u64,
        Some(descriptor_content_type),
    );

    state.file_index.write().await.insert(
        sha256.clone(),
        FileMetadata {
            path: filepath.clone(),
            extension,
            mime_type: Some(content_type_clone),
            size: blob_bytes.len() as u64,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    );
    info!("Blob descriptor created for SHA256: {}", sha256);

    // Track mirror upload statistics
    {
        let mut files_uploaded = state.files_uploaded.write().await;
        *files_uploaded += 1;

        // Track upload throughput for mirrored files
        let mut upload_throughput_data = state.upload_throughput_data.write().await;
        upload_throughput_data.push((std::time::Instant::now(), blob_bytes.len() as u64));

        // Keep only last 1000 entries to prevent memory bloat
        if upload_throughput_data.len() > 1000 {
            upload_throughput_data.drain(0..100);
        }
    }

    // After successful mirroring
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

pub async fn serve_index() -> Result<Response, StatusCode> {
    let html_content = include_str!("index.html");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html")
        .body(Body::from(html_content))
        .unwrap())
}

// ===== HELPER FUNCTIONS FOR STREAMING =====

/// Create a streaming response that reads from a growing file
async fn create_tailing_stream(
    reader: File,
    written_len: Arc<AtomicU64>,
    notify: Arc<Notify>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold((reader, written_len, notify, 0u64), |state| async move {
        let (mut reader, written_len, notify, mut pos) = state;

        loop {
            let available = written_len.load(Ordering::Acquire);
            if pos < available {
                // Es gibt neue Bytes; lese einen moderaten Block
                let to_read = std::cmp::min(64 * 1024, (available - pos) as usize);
                let mut buf = vec![0u8; to_read];

                // Seek to current position and read
                if let Err(_) = reader.seek(SeekFrom::Start(pos)).await {
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
                    Ok::<Bytes, std::io::Error>(Bytes::from(buf)),
                    (reader, written_len, notify, pos),
                ));
            } else {
                // Warten bis Downloader mehr geschrieben hat
                notify.notified().await;
            }
        }
    })
}

/// Build response headers for streaming content
fn build_streaming_response_headers(content_type: &str, filename: &str) -> Response<Body> {
    let body = Body::from(vec![]); // Placeholder, will be replaced
    let mut builder = Response::builder().status(StatusCode::OK);

    builder = builder.header(header::CONTENT_TYPE, content_type);
    builder = builder.header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
    builder = builder.header(header::ACCEPT_RANGES, "bytes");

    // Add Content-Disposition header to prevent save dialog
    let filename_display = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename_display);
    builder = builder.header(header::CONTENT_DISPOSITION, content_disposition);

    info!(
        "Built response headers: Content-Type={}, Content-Disposition=inline; filename=\"{}\"",
        content_type, filename_display
    );

    builder.body(body).unwrap()
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
        "public, max-age=31536000, immutable".parse().unwrap(),
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

pub async fn method_not_allowed() -> Result<Response, StatusCode> {
    Ok(Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body("Method Not Allowed".into())
        .unwrap())
}

async fn serve_file_with_range(path: PathBuf, headers: HeaderMap) -> Result<Response, StatusCode> {
    use axum::http::header::RANGE;
    let range_header = headers.get(RANGE).and_then(|r| r.to_str().ok());

    let expires_dt = Utc::now() + Duration::days(365);
    let expires_str = expires_dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let expires_header = HeaderValue::from_str(&expires_str).unwrap();

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
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            let mime = from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or("application/octet-stream".into());

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .header(CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(EXPIRES, expires_header.clone())
                .header(header::CONTENT_DISPOSITION, content_disposition.clone())
                .body(body)
                .unwrap());
        }
    }

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or("application/octet-stream".into()),
        )
        .header(CACHE_CONTROL, "public, max-age=31536000, immutable")
        .header(EXPIRES, expires_header.clone())
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .body(body)
        .unwrap())
}

/// Handles HEAD /upload to indicate upload capability and requirements.
pub async fn head_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Validate Nostr authorization
    let auth = match headers.get(header::AUTHORIZATION) {
        Some(a) => a,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap());
        }
    };
    match validate_nostr_auth(
        match auth.to_str() {
            Ok(s) => s,
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::empty())
                    .unwrap());
            }
        },
        &state,
    )
    .await
    {
        Ok(e) => e,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap());
        }
    };

    // Check storage limits
    let index = state.file_index.read().await;
    let total_files = index.len();
    let total_size: u64 = index.values().map(|m| m.size).sum();
    if total_files >= state.max_total_files || total_size >= state.max_total_size {
        return Ok(Response::builder()
            .status(StatusCode::INSUFFICIENT_STORAGE)
            .body(Body::empty())
            .unwrap());
    }

    // Compose headers per spec
    let builder = Response::builder().status(StatusCode::OK);
    // Optionally add more headers as needed by spec

    Ok(builder.body(Body::empty()).unwrap())
}

/// Handles GET /_stats to return application statistics.
pub async fn get_stats(State(state): State<AppState>) -> Result<Json<Stats>, StatusCode> {
    let stats = state.get_stats().await;
    Ok(Json(stats))
}

/// Handles OPTIONS /upload to signal support for multi-part uploads
pub async fn options_upload() -> Result<Response, StatusCode> {
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ALLOW, "PUT, HEAD, OPTIONS, PATCH")
        .body(Body::empty())
        .unwrap())
}

/// Handles PATCH /upload for chunked uploads
pub async fn patch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    // Extract required headers
    let sha256 = headers.get("X-SHA-256")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing X-SHA-256 header");
            StatusCode::BAD_REQUEST
        })?;

    let upload_type = headers.get("Upload-Type")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing Upload-Type header");
            StatusCode::BAD_REQUEST
        })?;

    let upload_length = headers.get("Upload-Length")
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

    let upload_offset = headers.get("Upload-Offset")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            error!("Missing or invalid Upload-Offset header");
            StatusCode::BAD_REQUEST
        })?;

    let content_type = headers.get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing Content-Type header");
            StatusCode::BAD_REQUEST
        })?;

    // Validate Content-Type is application/octet-stream
    if content_type != "application/octet-stream" {
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
    let chunk_path = temp_dir.join(chunk_filename);
    
    // Stream chunk data directly to file
    let body: Body = req.into_body();
    let mut stream = body.into_data_stream();
    let mut chunk_file = File::create(&chunk_path).await.map_err(|e| {
        error!("Failed to create chunk file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut total_written = 0u64;
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
    }

    // Ensure all data is written to disk
    chunk_file.sync_all().await.map_err(|e| {
        error!("Failed to sync chunk file: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    drop(chunk_file);

    // Validate chunk size matches Content-Length
    if total_written != content_length {
        error!("Chunk size mismatch: expected {}, got {}", content_length, total_written);
        // Clean up the chunk file on error
        let _ = fs::remove_file(&chunk_path).await;
        return Err(StatusCode::BAD_REQUEST);
    }

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
    
    info!("Chunk upload progress: {}/{} bytes ({} chunks)", total_received, upload_length, chunk_upload.chunks.len());
    
    if total_received >= upload_length {
        // Upload is complete, reconstruct the final blob
        if total_received > upload_length {
            warn!("Received more data than expected: {} > {} (possible overlapping chunks)", total_received, upload_length);
        }
        info!("Upload complete! Reconstructing final blob for {} (received: {}, expected: {})", sha256, total_received, upload_length);
        match reconstruct_final_blob(&state, chunk_upload, sha256).await {
            Ok(descriptor) => {
                // Remove from chunk uploads
                chunk_uploads.remove(sha256);
                
                // Track upload statistics
                {
                    let mut files_uploaded = state.files_uploaded.write().await;
                    *files_uploaded += 1;

                    let mut upload_throughput_data = state.upload_throughput_data.write().await;
                    upload_throughput_data.push((std::time::Instant::now(), upload_length));

                    if upload_throughput_data.len() > 1000 {
                        upload_throughput_data.drain(0..100);
                    }
                }

                // Queue cleanup job
                let mut changes_pending = state.changes_pending.write().await;
                *changes_pending = true;

                info!("Successfully reconstructed blob: {} ({} bytes)", sha256, descriptor.size);
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
                    .unwrap())
            }
            Err(e) => {
                error!("Failed to reconstruct final blob: {}", e);
                chunk_uploads.remove(sha256);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        // Upload not complete, return 204 No Content
        info!("Chunk accepted for upload {}: {}/{} bytes remaining", sha256, upload_length - total_received, upload_length);
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
    // Sort chunks by offset
    let mut sorted_chunks = chunk_upload.chunks.clone();
    sorted_chunks.sort_by_key(|c| c.offset);

    // Validate that chunks cover the entire upload length
    let mut expected_offset = 0;
    for chunk in &sorted_chunks {
        if chunk.offset != expected_offset {
            return Err(format!("Gap in chunk coverage at offset {}", expected_offset));
        }
        expected_offset += chunk.length;
    }

    if expected_offset != chunk_upload.upload_length {
        return Err(format!("Chunks don't cover full upload length: {} vs {}", 
                           expected_offset, chunk_upload.upload_length));
    }

    // Create temp file for reconstruction
    let temp_dir = state.upload_dir.join("temp");
    fs::create_dir_all(&temp_dir).await.map_err(|e| format!("Failed to create temp dir: {}", e))?;

    let temp_path = temp_dir.join(format!("reconstruct_{}", uuid::Uuid::new_v4()));
    let mut temp_file = File::create(&temp_path).await.map_err(|e| format!("Failed to create temp file: {}", e))?;

    // Write chunks to temp file in order
    let mut hasher = Sha256::new();
    for chunk in &sorted_chunks {
        // Read chunk data from file
        let mut chunk_file = File::open(&chunk.chunk_path).await.map_err(|e| format!("Failed to open chunk file: {}", e))?;
        let mut chunk_data = Vec::with_capacity(chunk.length as usize);
        chunk_file.read_to_end(&mut chunk_data).await.map_err(|e| format!("Failed to read chunk file: {}", e))?;
        
        // Validate chunk file size
        if chunk_data.len() as u64 != chunk.length {
            return Err(format!("Chunk file size mismatch: expected {}, got {}", chunk.length, chunk_data.len()));
        }
        
        temp_file.write_all(&chunk_data).await.map_err(|e| format!("Failed to write chunk: {}", e))?;
        hasher.update(&chunk_data);
    }

    temp_file.sync_all().await.map_err(|e| format!("Failed to sync temp file: {}", e))?;
    drop(temp_file);

    // Verify SHA256 hash of the reconstructed file matches the expected hash
    let calculated_sha256 = format!("{:x}", hasher.finalize());
    if calculated_sha256 != expected_sha256 {
        let _ = fs::remove_file(&temp_path).await; // Clean up temp file
        // Clean up chunk files on error
        for chunk in &sorted_chunks {
            let _ = fs::remove_file(&chunk.chunk_path).await;
        }
        return Err(format!("SHA256 mismatch: expected {}, got {}", expected_sha256, calculated_sha256));
    }

    // Move to final location using the expected SHA256 (which we just verified matches)
    let extension = chunk_upload.upload_type
        .split('/')
        .last()
        .map(|s| s.to_string());
    
    let final_path = get_nested_path(&state.upload_dir, expected_sha256, extension.as_deref());
    
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    fs::rename(&temp_path, &final_path).await.map_err(|e| format!("Failed to move temp file: {}", e))?;

    // Clean up chunk files after successful reconstruction
    for chunk in &sorted_chunks {
        if let Err(e) = fs::remove_file(&chunk.chunk_path).await {
            error!("Failed to clean up chunk file {}: {}", chunk.chunk_path.display(), e);
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
