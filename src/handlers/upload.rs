// Refactored upload handlers using the new service layer
// This file shows the improved structure - handlers should be thin and delegate to services

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use serde_json::Value;
use tracing::{error, info, warn};

use crate::error::AppError;
use crate::helpers::*;
use crate::models::AppState;
use crate::services::{auth, cashu, file_storage, hls, upload};

/// Check if payment is required and process it
///
/// Supports two flows:
/// 1. Preemptive: Client sends X-Cashu with first request (validated after size known)
/// 2. Reactive: Client gets 402, then retries with X-Cashu
async fn check_payment(
    state: &AppState,
    headers: &HeaderMap,
    size_bytes: u64,
    feature_enabled: bool,
) -> Result<(), AppError> {
    if !feature_enabled {
        return Ok(());
    }

    let required_sats = cashu::calculate_price(size_bytes, state.cashu_price_per_mb);

    // Check for X-Cashu header (preemptive or retry payment)
    let cashu_header = cashu::extract_cashu_header(headers);

    match cashu_header {
        None => {
            // No payment provided, return 402
            Err(AppError::PaymentRequired {
                amount_sats: required_sats,
                unit: "sat".to_string(),
                mints: state.cashu_accepted_mints.clone(),
            })
        }
        Some(token_str) => {
            // Parse and verify token
            let token = cashu::parse_token(&token_str)?;

            // Verify amount is sufficient for actual size
            cashu::verify_token_basics(&token, required_sats, &state.cashu_accepted_mints)?;

            // Receive token into wallet
            if let Some(wallet) = &state.cashu_wallet {
                cashu::receive_token(wallet, &token).await?;
            }

            Ok(())
        }
    }
}

/// Handle file uploads - REFACTORED VERSION
pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, AppError> {
    // Check if upload feature is enabled and determine auth mode
    let auth_mode = match state.feature_upload_enabled {
        crate::models::FeatureMode::Off => {
            return Err(AppError::Forbidden("Upload feature is disabled".to_string()));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public => auth::AuthMode::Unrestricted,
    };

    // Validate Nostr authorization
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth_mode).await?;

    // Extract content type, extension, and expiration
    let content_type = extract_content_type(&headers);
    let extension = get_extension_from_mime(&content_type);
    let expiration = extract_expiration(&headers);

    // Prepare temp file
    file_storage::ensure_temp_dir(&state).await?;
    let temp_path = file_storage::create_temp_path(&state, "upload", None);
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    // Stream to temp file and calculate hash
    let body_stream = req.into_body().into_data_stream();
    let (sha256, total_bytes) = upload::stream_to_temp_file(body_stream, &temp_path).await?;

    // Validate authorization matches the hash (must come before payment check)
    auth::validate_upload_auth(&auth_event, &sha256)?;

    // Check payment if required (after we know the size and auth is validated)
    check_payment(&state, &headers, total_bytes, state.feature_paid_upload).await?;

    // Finalize upload
    upload::finalize_upload(
        &state,
        &temp_path,
        &sha256,
        total_bytes,
        extension.clone(),
        Some(content_type.clone()),
        expiration,
    )
    .await?;

    // Track statistics
    track_upload_stats(&state, total_bytes).await;

    // Create response
    let descriptor = state.create_blob_descriptor(&sha256, total_bytes, Some(content_type), expiration);

    let json_body = serde_json::to_string(&descriptor)
        .map_err(|e| AppError::InternalError(format!("Failed to serialize response: {}", e)))?;

    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json_body))
        .map_err(|e| AppError::InternalError(format!("Failed to build response: {}", e)))
}

/// Handle blob mirroring - REFACTORED VERSION
pub async fn mirror_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, AppError> {
    // Check if mirror feature is enabled and determine auth mode
    let auth_mode = match state.feature_mirror_enabled {
        crate::models::FeatureMode::Off => {
            return Err(AppError::Forbidden("Mirror feature is disabled".to_string()));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public => auth::AuthMode::Unrestricted,
    };

    // Validate Nostr authorization
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth_mode).await?;

    // Extract expected SHA-256 from auth event and expiration from headers
    let expected_sha256 = auth::extract_sha256_from_event(&auth_event)
        .ok_or_else(|| AppError::Unauthorized("No valid SHA-256 hash found in auth event".to_string()))?;
    let expiration = extract_expiration(&headers);

    info!("Expected SHA-256 from auth event: {}", expected_sha256);

    // Parse request body for URL
    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await
        .map_err(|_| AppError::InternalError("Failed to read request body".to_string()))?;
    
    let body: Value = serde_json::from_slice(&body_bytes)?;
    let url = body.get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Missing 'url' field in request body".to_string()))?;

    info!("Starting to mirror blob from URL: {}", url);

    // Fetch from URL (includes SSRF validation)
    let response = upload::fetch_from_url(url).await?;

    // Extract content type and check size
    let content_type = extract_content_type_from_response(response.headers());
    let content_length = response.content_length();
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;

    upload::check_size_limit(content_length, max_size_bytes)?;

    // Check payment if required
    if let Some(size) = content_length {
        check_payment(&state, &headers, size, state.feature_paid_mirror).await?;
    }

    // Prepare temp file
    file_storage::ensure_temp_dir(&state).await?;
    let extension = get_extension_from_mime(&content_type);
    let temp_path = file_storage::create_temp_path(&state, "mirror", extension.as_deref());
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    info!("💾 Streaming blob to temp file: {}", temp_path.display());

    // Stream and hash
    let (calculated_sha256, body_size) = upload::stream_response_to_temp_file(
        response,
        &temp_path,
        max_size_bytes,
    )
    .await?;

    info!("🔐 SHA256 verification: calculated {} vs expected {}", calculated_sha256, expected_sha256);

    // Validate hash matches
    if calculated_sha256 != expected_sha256 {
        return Err(AppError::Unauthorized(format!(
            "SHA256 hash mismatch: expected {}, got {}",
            expected_sha256, calculated_sha256
        )));
    }

    info!("✅ SHA256 verification passed");

    // Finalize upload
    upload::finalize_upload(
        &state,
        &temp_path,
        &expected_sha256,
        body_size,
        get_extension_from_mime(&content_type),
        Some(content_type.clone()),
        expiration,
    )
    .await?;

    // Track statistics
    track_upload_stats(&state, body_size).await;

    // HLS recursive mirror: if this is a playlist, mirror referenced segments in background
    if hls::is_hls_playlist(&content_type) {
        if let Some(origin_base_url) = hls::extract_origin_base_url(url) {
            // Read the stored playlist to parse references
            if let Some(metadata) = file_storage::get_file_metadata(&state, &expected_sha256).await {
                match tokio::fs::read_to_string(&metadata.path).await {
                    Ok(content) => {
                        let references = hls::parse_playlist_references(&content);
                        if !references.is_empty() {
                            info!(
                                "[HLS] Detected playlist with {} references, spawning background mirror from {}",
                                references.len(),
                                origin_base_url
                            );
                            let state_clone = state.clone();
                            let concurrency = state.hls_mirror_concurrency;
                            tokio::spawn(async move {
                                hls::mirror_hls_references(
                                    state_clone,
                                    origin_base_url,
                                    references,
                                    concurrency,
                                )
                                .await;
                            });
                        }
                    }
                    Err(e) => {
                        warn!("[HLS] Failed to read playlist file for recursive mirror: {}", e);
                    }
                }
            }
        } else {
            warn!("[HLS] Could not extract origin base URL from: {}", url);
        }
    }

    // Create response
    let descriptor = state.create_blob_descriptor(&expected_sha256, body_size, Some(content_type), expiration);

    info!("🎉 Mirror operation completed successfully: {} -> {} ({} bytes)",
          url, expected_sha256, body_size);

    let json_body = serde_json::to_string(&descriptor)
        .map_err(|e| AppError::InternalError(format!("Failed to serialize response: {}", e)))?;

    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json_body))
        .map_err(|e| AppError::InternalError(format!("Failed to build response: {}", e)))
}

/// Handle chunked uploads - REFACTORED VERSION
pub async fn patch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, AppError> {
    use crate::constants::*;
    use crate::models::{ChunkInfo, ChunkUpload};
    use futures_util::StreamExt;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use uuid;

    // Check if upload feature is enabled
    if !state.feature_upload_enabled.is_enabled() {
        return Err(AppError::Forbidden("Upload feature is disabled".to_string()));
    }

    // Extract and validate headers
    let sha256 = headers
        .get(X_SHA_256_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing X-SHA-256 header".to_string()))?;

    let upload_type = headers
        .get(UPLOAD_TYPE_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing Upload-Type header".to_string()))?;

    let upload_length = headers
        .get(UPLOAD_LENGTH_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| AppError::BadRequest("Missing or invalid Upload-Length header".to_string()))?;

    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| AppError::BadRequest("Missing or invalid Content-Length header".to_string()))?;

    let upload_offset = headers
        .get(UPLOAD_OFFSET_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| AppError::BadRequest("Missing or invalid Upload-Offset header".to_string()))?;

    let content_type = extract_content_type(&headers);
    let expiration = extract_expiration(&headers);

    // Validate Content-Type is application/octet-stream
    if content_type != DEFAULT_CONTENT_TYPE {
        return Err(AppError::BadRequest(format!("Invalid Content-Type: {}", content_type)));
    }

    // Determine auth mode based on feature configuration
    let auth_mode = match state.feature_upload_enabled {
        crate::models::FeatureMode::Off => {
            // Already checked above, this shouldn't happen
            return Err(AppError::Forbidden("Upload feature is disabled".to_string()));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public => auth::AuthMode::Unrestricted,
    };

    // Validate Nostr authorization
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth_mode).await?;

    // Validate authorization for chunk upload
    auth::validate_chunk_upload_auth(&auth_event, sha256, &content_length.to_string())?;

    // Validate chunk size limits
    let max_chunk_size_bytes = state.max_chunk_size_mb * 1024 * 1024;
    if content_length > max_chunk_size_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "Chunk size {} exceeds maximum {} MB",
            content_length,
            state.max_chunk_size_mb
        )));
    }

    // Validate offset + length doesn't exceed upload length
    if upload_offset + content_length > upload_length {
        return Err(AppError::BadRequest(format!(
            "Chunk exceeds upload length: offset {} + length {} > {}",
            upload_offset, content_length, upload_length
        )));
    }

    // Create temp directory for chunk files
    let chunk_temp_dir = state.upload_dir.join("temp").join("chunks");
    tokio::fs::create_dir_all(&chunk_temp_dir).await.map_err(|e| {
        error!("Failed to create chunk temp directory: {}", e);
        AppError::IoError(format!("Failed to create chunk directory: {}", e))
    })?;

    // Create chunk file
    let chunk_filename = format!("chunk_{}_{}_{}", sha256, upload_offset, uuid::Uuid::new_v4());
    let chunk_path = chunk_temp_dir.join(&chunk_filename);
    info!("💾 Creating chunk file: {} (offset: {}, length: {})", chunk_filename, upload_offset, content_length);

    // Stream chunk data to file
    let body_stream = req.into_body().into_data_stream();
    let mut chunk_file = File::create(&chunk_path).await.map_err(|e| {
        error!("Failed to create chunk file: {}", e);
        AppError::IoError(format!("Failed to create chunk file: {}", e))
    })?;

    let mut total_written = 0u64;
    let mut stream = body_stream;
    while let Some(chunk) = stream.next().await {
        let data = chunk.map_err(|e| {
            error!("Failed to read chunk: {}", e);
            AppError::BadRequest(format!("Failed to read chunk: {}", e))
        })?;

        chunk_file.write_all(&data).await.map_err(|e| {
            error!("Failed to write chunk data: {}", e);
            AppError::IoError(format!("Failed to write chunk data: {}", e))
        })?;
        total_written += data.len() as u64;
    }

    chunk_file.sync_all().await.map_err(|e| {
        error!("Failed to sync chunk file: {}", e);
        AppError::IoError(format!("Failed to sync chunk file: {}", e))
    })?;
    drop(chunk_file);

    info!("✅ Chunk data written: {} bytes", total_written);

    // Validate chunk size matches Content-Length
    if total_written != content_length {
        error!("❌ Chunk size mismatch: expected {}, got {}", content_length, total_written);
        let _ = tokio::fs::remove_file(&chunk_path).await;
        return Err(AppError::BadRequest(format!(
            "Chunk size mismatch: expected {}, got {}",
            content_length, total_written
        )));
    }

    // Get or create chunk upload state
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
            expiration,
        }
    });

    if is_new_upload {
        info!("🚀 Starting new chunked upload: {} ({} bytes, type: {})", sha256, upload_length, upload_type);
    } else {
        info!("📦 Adding chunk to existing upload: {} (offset: {}, length: {})", sha256, upload_offset, content_length);
    }

    // Validate upload parameters match
    if chunk_upload.upload_type != upload_type || chunk_upload.upload_length != upload_length {
        return Err(AppError::BadRequest("Upload parameters mismatch".to_string()));
    }

    // Check for duplicate chunks
    if chunk_upload.chunks.iter().any(|c| c.offset == upload_offset) {
        error!("Duplicate chunk at offset {} for upload {}", upload_offset, sha256);
        let _ = tokio::fs::remove_file(&chunk_path).await;
        return Err(AppError::BadRequest(format!("Duplicate chunk at offset {}", upload_offset)));
    }

    // Add chunk info
    chunk_upload.chunks.push(ChunkInfo {
        offset: upload_offset,
        length: content_length,
        chunk_path,
    });

    // Check if upload is complete
    let total_received: u64 = chunk_upload.chunks.iter().map(|c| c.length).sum();
    let progress_percent = (total_received as f64 / upload_length as f64 * 100.0) as u8;

    info!("📈 Chunk upload progress: {}/{} bytes ({}%) - {} chunks",
          total_received, upload_length, progress_percent, chunk_upload.chunks.len());

    if total_received >= upload_length {
        info!("🎉 Upload complete! Reconstructing final blob for {}", sha256);

        // Check payment if required (for the full upload size)
        check_payment(&state, &headers, upload_length, state.feature_paid_upload).await?;

        // Clone data and release lock
        let chunk_upload_data = chunk_upload.clone();
        drop(chunk_uploads);

        // Reconstruct the blob
        match reconstruct_blob(&state, &chunk_upload_data, sha256).await {
            Ok(descriptor) => {
                // Remove from chunk uploads
                state.chunk_uploads.write().await.remove(sha256);

                // Track statistics
                track_upload_stats(&state, upload_length).await;

                // Mark changes pending
                file_storage::mark_changes_pending(&state).await;

                info!("🏁 Chunked upload completed successfully: {} ({} bytes)", sha256, descriptor.size);

                let json_body = serde_json::to_string(&descriptor)
                    .map_err(|e| AppError::InternalError(format!("Failed to serialize response: {}", e)))?;

                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json_body))
                    .map_err(|e| AppError::InternalError(format!("Failed to build response: {}", e)))
            }
            Err(e) => {
                error!("Failed to reconstruct blob: {}", e);
                state.chunk_uploads.write().await.remove(sha256);
                Err(e)
            }
        }
    } else {
        // Upload not complete
        let remaining = upload_length - total_received;
        info!("⏳ Chunk accepted: {} bytes remaining - {} chunks received",
              remaining, chunk_upload.chunks.len());

        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .map_err(|e| AppError::InternalError(format!("Failed to build response: {}", e)))
    }
}

/// Reconstruct final blob from chunks
async fn reconstruct_blob(
    state: &AppState,
    chunk_upload: &crate::models::ChunkUpload,
    expected_sha256: &str,
) -> Result<crate::models::BlobDescriptor, AppError> {
    use sha2::{Digest, Sha256};
    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    info!("🔧 Reconstructing blob: {} ({} chunks, {} bytes)",
          expected_sha256, chunk_upload.chunks.len(), chunk_upload.upload_length);

    // Sort chunks by offset
    let mut sorted_chunks = chunk_upload.chunks.clone();
    sorted_chunks.sort_by_key(|c| c.offset);

    // Validate coverage
    let mut expected_offset = 0;
    for (i, chunk) in sorted_chunks.iter().enumerate() {
        if chunk.offset != expected_offset {
            return Err(AppError::InternalError(format!(
                "Gap in chunk coverage at offset {} (chunk {}: offset {}, length {})",
                expected_offset, i, chunk.offset, chunk.length
            )));
        }
        expected_offset += chunk.length;
    }

    if expected_offset != chunk_upload.upload_length {
        return Err(AppError::InternalError(format!(
            "Chunks don't cover full upload length: {} vs {}",
            expected_offset, chunk_upload.upload_length
        )));
    }

    info!("✅ Chunk coverage validated: {} bytes", expected_offset);

    // Create temp file for reconstruction
    let temp_dir = state.upload_dir.join("temp");
    tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
        AppError::IoError(format!("Failed to create temp dir: {}", e))
    })?;

    let temp_path = temp_dir.join(format!("reconstruct_{}", uuid::Uuid::new_v4()));
    let mut temp_file = File::create(&temp_path).await.map_err(|e| {
        AppError::IoError(format!("Failed to create reconstruction file: {}", e))
    })?;

    // Reconstruct file with hash calculation
    let mut hasher = Sha256::new();
    let mut total_written = 0u64;

    for (i, chunk) in sorted_chunks.iter().enumerate() {
        let mut chunk_file = File::open(&chunk.chunk_path).await.map_err(|e| {
            AppError::IoError(format!("Failed to open chunk file: {}", e))
        })?;

        let mut chunk_data = Vec::with_capacity(chunk.length as usize);
        chunk_file.read_to_end(&mut chunk_data).await.map_err(|e| {
            AppError::IoError(format!("Failed to read chunk file: {}", e))
        })?;

        if chunk_data.len() as u64 != chunk.length {
            return Err(AppError::InternalError(format!(
                "Chunk file size mismatch: expected {}, got {}",
                chunk.length, chunk_data.len()
            )));
        }

        temp_file.write_all(&chunk_data).await.map_err(|e| {
            AppError::IoError(format!("Failed to write chunk: {}", e))
        })?;

        hasher.update(&chunk_data);
        total_written += chunk_data.len() as u64;

        if i % 10 == 0 || i == sorted_chunks.len() - 1 {
            info!("📊 Reconstruction progress: {}/{} chunks ({} bytes)", i + 1, sorted_chunks.len(), total_written);
        }
    }

    temp_file.sync_all().await.map_err(|e| {
        AppError::IoError(format!("Failed to sync reconstruction file: {}", e))
    })?;
    drop(temp_file);

    // Verify hash
    let calculated_sha256 = format!("{:x}", hasher.finalize());
    info!("🔍 SHA256 verification: calculated {} vs expected {}", calculated_sha256, expected_sha256);

    if calculated_sha256 != expected_sha256 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        for chunk in &sorted_chunks {
            let _ = tokio::fs::remove_file(&chunk.chunk_path).await;
        }
        return Err(AppError::Unauthorized(format!(
            "SHA256 mismatch: expected {}, got {}",
            expected_sha256, calculated_sha256
        )));
    }

    info!("✅ SHA256 verification passed");

    // Finalize upload
    upload::finalize_upload(
        state,
        &temp_path,
        expected_sha256,
        chunk_upload.upload_length,
        get_extension_from_mime(&chunk_upload.upload_type),
        Some(chunk_upload.upload_type.clone()),
        chunk_upload.expiration,
    )
    .await?;

    // Clean up chunk files
    info!("🧹 Cleaning up {} chunk files", sorted_chunks.len());
    for chunk in &sorted_chunks {
        if let Err(e) = tokio::fs::remove_file(&chunk.chunk_path).await {
            error!("Failed to clean up chunk file {}: {}", chunk.chunk_path.display(), e);
        }
    }

    // Return descriptor
    Ok(state.create_blob_descriptor(
        expected_sha256,
        chunk_upload.upload_length,
        Some(chunk_upload.upload_type.clone()),
        chunk_upload.expiration,
    ))
}

/// Temp file guard for cleanup (keeps existing implementation)
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
                error!("Failed to clean up temp file {}: {}", self.path.display(), e);
            }
        }
    }
}
