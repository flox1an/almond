use axum::body::to_bytes;
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    response::Response,
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::StreamExt;
use mime_guess::from_path;
use nostr::prelude::*;
use reqwest::{header as reqwest_header, Client};
use serde_json::{self, Value};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};
use uuid;

use crate::models::{AppState, BlobDescriptor, FileMetadata, ListQuery};
use crate::utils::{find_file, get_nested_path, get_sha256_hash_from_filename, parse_range_header};

pub async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<BlobDescriptor>>, (StatusCode, String)> {
    // Validate Nostr authorization
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        (StatusCode::UNAUTHORIZED, "Missing Authorization header".to_string())
    })?;

    let _event: Event = validate_nostr_auth(
        auth.to_str().map_err(|_| {
            (StatusCode::UNAUTHORIZED, "Invalid Authorization header format".to_string())
        })?,
        &state.allowed_pubkeys,
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
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    info!("get for url: {}", filename);

    if let Some(filename) = get_sha256_hash_from_filename(&filename) {
        info!("Found file: {}", filename);

        match find_file(&state.file_index, &filename).await {
            Some(file_metadata) => {
                if req.method() == axum::http::Method::HEAD {
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
                    return serve_file_with_range(file_metadata.path, req).await;
                }
            }
            None => Err(StatusCode::NOT_FOUND),
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
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

async fn validate_nostr_auth(
    auth: &str,
    allowed_pubkeys: &[PublicKey],
) -> Result<Event, StatusCode> {
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

    info!("Decoded Nostr event JSON: {}", json_str);

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

    if let Some(expiration) = event.tags.iter().find(|t| t.as_vec()[0] == "expiration") {
        if let Some(exp_time) = expiration.as_vec().get(1) {
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

    // Check if pubkey is allowed
    if !allowed_pubkeys.is_empty() && !allowed_pubkeys.contains(&event.pubkey) {
        error!("Pubkey not authorized");
        return Err(StatusCode::UNAUTHORIZED);
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
        &state.allowed_pubkeys,
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
    let x_tags: Vec<_> = auth_event
        .tags
        .iter()
        .filter(|t| t.as_vec()[0] == "x")
        .collect();

    if x_tags.is_empty() {
        error!("No x tag found in event");
        return Err(StatusCode::UNAUTHORIZED);
    }

    if !x_tags.iter().any(|t| t.as_vec().get(1) == Some(&sha256)) {
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
        &state.allowed_pubkeys,
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

pub async fn method_not_allowed() -> Result<Response, StatusCode> {
    Ok(Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body("Method Not Allowed".into())
        .unwrap())
}

async fn serve_file_with_range(path: PathBuf, req: Request<Body>) -> Result<Response, StatusCode> {
    use axum::http::header::RANGE;
    let range_header = req.headers().get(RANGE).and_then(|r| r.to_str().ok());

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
        .body(body)
        .unwrap())
}
