// Refactored upload handlers using the new service layer
// This file shows the improved structure - handlers should be thin and delegate to services

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::error::AppError;
use crate::helpers::{
    extract_content_type, extract_content_type_from_response, extract_expiration,
    get_extension_from_mime, track_upload_stats,
};
use crate::models::{AppState, FileLocation};
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
                let received_sats = cashu::receive_token(wallet, &token).await?;
                if received_sats < required_sats {
                    return Err(AppError::PaymentRequired {
                        amount_sats: required_sats,
                        unit: "sat".to_string(),
                        mints: state.cashu_accepted_mints.clone(),
                    });
                }
            } else {
                return Err(AppError::ServiceUnavailable(
                    "Payment service is not initialized".to_string(),
                ));
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
            return Err(AppError::Forbidden(
                "Upload feature is disabled".to_string(),
            ));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public if state.allowed_pubkeys.is_empty() => {
            auth::AuthMode::Unrestricted
        }
        crate::models::FeatureMode::Public => auth::AuthMode::Strict,
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
    let declared_size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(state.max_blob_size_bytes);
    file_storage::ensure_storage_capacity(&state, declared_size).await?;

    // Prepare temp file
    file_storage::ensure_temp_dir(&state).await?;
    let temp_path = file_storage::create_temp_path(&state, "upload", None);
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    // Stream to temp file and calculate hash.  The explicit accounting is
    // required even when a transport layer already rejects oversized bodies.
    let body_stream = req.into_body().into_data_stream();
    let (sha256, total_bytes) =
        upload::stream_to_temp_file(body_stream, &temp_path, state.max_blob_size_bytes).await?;

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
    track_upload_stats(&state);

    // Create response
    let descriptor =
        state.create_blob_descriptor(&sha256, total_bytes, Some(content_type), expiration);

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
            return Err(AppError::Forbidden(
                "Mirror feature is disabled".to_string(),
            ));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public if state.allowed_pubkeys.is_empty() => {
            auth::AuthMode::Unrestricted
        }
        crate::models::FeatureMode::Public => auth::AuthMode::Strict,
    };

    // Validate Nostr authorization
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth_mode).await?;

    // BUD-11: validate t=upload tag for mirror operations
    auth::validate_t_tag(&auth_event, "upload")?;

    // Extract expected SHA-256 from auth event and expiration from headers
    let expected_sha256 = auth::extract_sha256_from_event(&auth_event).ok_or_else(|| {
        AppError::Unauthorized("No valid SHA-256 hash found in auth event".to_string())
    })?;
    let expiration = extract_expiration(&headers);

    const MAX_MIRROR_JSON_BYTES: usize = 64 * 1024;
    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_MIRROR_JSON_BYTES)
        .await
        .map_err(|_| AppError::PayloadTooLarge("Mirror request body exceeds 64 KiB".to_string()))?;

    let body: Value = serde_json::from_slice(&body_bytes)?;
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Missing 'url' field in request body".to_string()))?;

    info!("Starting to mirror blob from URL: {}", url);

    // Fetch from URL (includes redirect-free, address-pinned SSRF protection).
    let response = upload::fetch_from_url(url).await?;
    let content_type = extract_content_type_from_response(response.headers());
    let content_length = response.content_length();
    let max_size_bytes =
        (state.max_upstream_download_size_mb * 1024 * 1024).min(state.max_blob_size_bytes);
    upload::check_size_limit(content_length, max_size_bytes)?;
    let declared_size = content_length.ok_or_else(|| {
        AppError::BadRequest("Mirror source must provide Content-Length".to_string())
    })?;
    if state.feature_paid_mirror {
        check_payment(&state, &headers, declared_size, true).await?;
    }
    file_storage::ensure_storage_capacity(&state, declared_size).await?;

    // Prepare temp file
    file_storage::ensure_temp_dir(&state).await?;
    let extension = get_extension_from_mime(&content_type);
    let temp_path = file_storage::create_temp_path(&state, "mirror", extension.as_deref());
    let _temp_guard = TempFileGuard::new(temp_path.clone());

    info!("💾 Streaming blob to temp file: {}", temp_path.display());
    // The body is still counted while streaming: Content-Length is only an
    // early rejection signal and cannot enlarge the accepted blob.
    let (calculated_sha256, body_size) =
        upload::stream_response_to_temp_file(response, &temp_path, max_size_bytes).await?;

    info!(
        "🔐 SHA256 verification: calculated {} vs expected {}",
        calculated_sha256, expected_sha256
    );

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
    track_upload_stats(&state);

    // HLS recursive mirror: if this is a playlist, mirror referenced segments in background
    if hls::is_hls_playlist(&content_type) {
        if let Some(origin_base_url) = hls::extract_origin_base_url(url) {
            // Read the stored playlist to parse references
            if let Some(metadata) = file_storage::get_file_metadata(&state, &expected_sha256).await
            {
                let playlist: Result<String, String> = match &metadata.location {
                    FileLocation::Local(path) => tokio::fs::read_to_string(path)
                        .await
                        .map_err(|error| error.to_string()),
                    FileLocation::S3 { key } => match &state.native_s3 {
                        Some(s3) => s3.read_text(key).await.map_err(|error| error.to_string()),
                        None => Err("S3 backend is not configured".to_owned()),
                    },
                };
                match playlist {
                    Ok(content) => {
                        let references = hls::parse_playlist_references(&content);
                        if !references.is_empty() {
                            debug!(
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
                    Err(e) => warn!(
                        "[HLS] Failed to read playlist file for recursive mirror: {}",
                        e
                    ),
                }
            }
        } else {
            warn!("[HLS] Could not extract origin base URL from: {}", url);
        }
    }

    // Create response
    let descriptor =
        state.create_blob_descriptor(&expected_sha256, body_size, Some(content_type), expiration);

    info!(
        "🎉 Mirror operation completed successfully: {} -> {} ({} bytes)",
        url, expected_sha256, body_size
    );

    let json_body = serde_json::to_string(&descriptor)
        .map_err(|e| AppError::InternalError(format!("Failed to serialize response: {}", e)))?;

    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json_body))
        .map_err(|e| AppError::InternalError(format!("Failed to build response: {}", e)))
}

/// Handle one bounded, authenticated chunk of a resumable upload.
pub async fn patch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, AppError> {
    use crate::constants::{
        DEFAULT_CONTENT_TYPE, UPLOAD_LENGTH_HEADER, UPLOAD_OFFSET_HEADER, UPLOAD_TYPE_HEADER,
        X_SHA_256_HEADER,
    };
    use crate::models::{ChunkInfo, ChunkUpload, ChunkUploadKey};

    let auth_mode = match state.feature_upload_enabled {
        crate::models::FeatureMode::Off => {
            return Err(AppError::Forbidden(
                "Upload feature is disabled".to_string(),
            ));
        }
        crate::models::FeatureMode::Wot => auth::AuthMode::WotOnly,
        crate::models::FeatureMode::Dvm => auth::AuthMode::DvmOnly,
        crate::models::FeatureMode::Public if state.allowed_pubkeys.is_empty() => {
            auth::AuthMode::Unrestricted
        }
        crate::models::FeatureMode::Public => auth::AuthMode::Strict,
    };

    let sha256 = headers
        .get(X_SHA_256_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing X-SHA-256 header".to_string()))?
        .to_owned();
    file_storage::validate_sha256_format(&sha256)?;
    let upload_type = headers
        .get(UPLOAD_TYPE_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing Upload-Type header".to_string()))?
        .to_owned();
    let parse_header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| AppError::BadRequest(format!("Missing or invalid {name} header")))
    };
    let upload_length = parse_header(UPLOAD_LENGTH_HEADER)?;
    let content_length = parse_header(header::CONTENT_LENGTH.as_str())?;
    let upload_offset = parse_header(UPLOAD_OFFSET_HEADER)?;
    if content_length == 0 {
        return Err(AppError::BadRequest(
            "Empty chunks are not accepted".to_string(),
        ));
    }
    if upload_length == 0 || upload_length > state.max_blob_size_bytes {
        return Err(AppError::PayloadTooLarge(
            "Upload-Length exceeds the configured blob limit".to_string(),
        ));
    }
    let max_chunk_size = state.max_chunk_size_mb * 1024 * 1024;
    if content_length > max_chunk_size {
        return Err(AppError::PayloadTooLarge(
            "Chunk exceeds the configured chunk limit".to_string(),
        ));
    }
    let chunk_end = upload_offset
        .checked_add(content_length)
        .ok_or_else(|| AppError::BadRequest("Upload-Offset overflows".to_string()))?;
    if chunk_end > upload_length {
        return Err(AppError::BadRequest(
            "Chunk exceeds Upload-Length".to_string(),
        ));
    }
    if extract_content_type(&headers) != DEFAULT_CONTENT_TYPE {
        return Err(AppError::BadRequest(
            "Chunk Content-Type must be application/octet-stream".to_string(),
        ));
    }

    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;
    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth_mode).await?;
    auth::validate_chunk_upload_auth(&auth_event, &sha256, &content_length.to_string())?;
    let key = ChunkUploadKey {
        pubkey: auth_event.pubkey,
        sha256: sha256.clone(),
    };

    // Pay before a potentially final chunk reaches disk.  No await occurs
    // while the session map is locked.
    let completes_upload = {
        let uploads = state.chunk_uploads.read().await;
        uploads
            .get(&key)
            .map_or(content_length == upload_length, |upload| {
                upload
                    .chunks
                    .iter()
                    .try_fold(content_length, |total, chunk| {
                        total.checked_add(chunk.length)
                    })
                    == Some(upload_length)
            })
    };
    if completes_upload {
        check_payment(&state, &headers, upload_length, state.feature_paid_upload).await?;
    }
    file_storage::ensure_storage_capacity(&state, content_length).await?;
    file_storage::ensure_temp_dir(&state).await?;
    let chunk_temp_dir = state.storage.temp.join("chunks");
    tokio::fs::create_dir_all(&chunk_temp_dir)
        .await
        .map_err(|error| AppError::IoError(format!("Failed to create chunk directory: {error}")))?;
    let chunk_path = chunk_temp_dir.join(format!(
        "chunk_{}_{}_{}",
        sha256,
        upload_offset,
        uuid::Uuid::new_v4()
    ));

    let write_result = upload::stream_to_temp_file(
        req.into_body().into_data_stream(),
        &chunk_path,
        content_length,
    )
    .await;
    let (_, written) = match write_result {
        Ok(result) => result,
        Err(error) => {
            let _ = tokio::fs::remove_file(&chunk_path).await;
            return Err(error);
        }
    };
    if written != content_length {
        let _ = tokio::fs::remove_file(&chunk_path).await;
        return Err(AppError::BadRequest(
            "Chunk body does not match Content-Length".to_string(),
        ));
    }

    let expiration = extract_expiration(&headers);
    let completion = {
        let mut uploads = state.chunk_uploads.write().await;
        if uploads.contains_key(&key) {
            Some(false)
        } else {
            let owner_sessions = uploads
                .keys()
                .filter(|existing| existing.pubkey == auth_event.pubkey)
                .count();
            if uploads.len() >= state.max_chunk_upload_sessions
                || owner_sessions >= state.max_chunk_upload_sessions_per_pubkey
            {
                None
            } else {
                uploads.insert(
                    key.clone(),
                    ChunkUpload {
                        sha256: sha256.clone(),
                        owner: auth_event.pubkey,
                        upload_type: upload_type.clone(),
                        upload_length,
                        temp_path: state
                            .storage
                            .temp
                            .join(format!("chunk_upload_{}", uuid::Uuid::new_v4())),
                        chunks: Vec::new(),
                        created_at: std::time::Instant::now(),
                        expiration,
                    },
                );
                Some(false)
            }
        }
        .and_then(|_| {
            let upload = uploads.get_mut(&key)?;
            if upload.upload_type != upload_type || upload.upload_length != upload_length {
                return None;
            }
            let overlaps = upload.chunks.iter().any(|chunk| {
                chunk.offset < chunk_end
                    && upload_offset < chunk.offset.saturating_add(chunk.length)
            });
            if overlaps {
                return None;
            }
            upload.chunks.push(ChunkInfo {
                offset: upload_offset,
                length: content_length,
                chunk_path: chunk_path.clone(),
            });
            let total = upload
                .chunks
                .iter()
                .try_fold(0u64, |sum, chunk| sum.checked_add(chunk.length))?;
            if total == upload.upload_length {
                uploads.remove(&key)
            } else {
                Some(upload.clone())
            }
        })
    };

    let Some(upload_data) = completion else {
        let _ = tokio::fs::remove_file(&chunk_path).await;
        return Err(AppError::Conflict(
            "Chunk conflicts with an existing session or session capacity is exhausted".to_string(),
        ));
    };
    if upload_data.chunks.len() == 1 && upload_data.chunks[0].length != upload_length {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .map_err(|error| {
                AppError::InternalError(format!("Failed to build response: {error}"))
            });
    }

    // A completed upload was atomically removed from the map before any
    // reconstruction starts, preventing a second finisher from racing it.
    if upload_data
        .chunks
        .iter()
        .map(|chunk| chunk.length)
        .sum::<u64>()
        != upload_length
    {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .map_err(|error| {
                AppError::InternalError(format!("Failed to build response: {error}"))
            });
    }
    let descriptor = reconstruct_blob(&state, &upload_data, &sha256).await?;
    track_upload_stats(&state);
    let body = serde_json::to_string(&descriptor).map_err(|error| {
        AppError::InternalError(format!("Failed to serialize response: {error}"))
    })?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|error| AppError::InternalError(format!("Failed to build response: {error}")))
}

/// Reconstruct a claimed upload exactly once and clean every temporary path on
/// both success and failure.
async fn reconstruct_blob(
    state: &AppState,
    chunk_upload: &crate::models::ChunkUpload,
    expected_sha256: &str,
) -> Result<crate::models::BlobDescriptor, AppError> {
    use sha2::{Digest, Sha256};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut chunks = chunk_upload.chunks.clone();
    chunks.sort_by_key(|chunk| chunk.offset);
    let mut expected_offset = 0u64;
    for chunk in &chunks {
        if chunk.offset != expected_offset {
            for chunk in &chunks {
                let _ = tokio::fs::remove_file(&chunk.chunk_path).await;
            }
            return Err(AppError::BadRequest(
                "Chunk coverage has a gap or overlap".to_string(),
            ));
        }
        expected_offset = expected_offset
            .checked_add(chunk.length)
            .ok_or_else(|| AppError::BadRequest("Chunk length overflows".to_string()))?;
    }
    if expected_offset != chunk_upload.upload_length {
        for chunk in &chunks {
            let _ = tokio::fs::remove_file(&chunk.chunk_path).await;
        }
        return Err(AppError::BadRequest(
            "Chunks do not cover the declared upload length".to_string(),
        ));
    }

    let temp_dir = file_storage::ensure_temp_dir(state).await?;
    let temp_path = temp_dir.join(format!("reconstruct_{}", uuid::Uuid::new_v4()));
    let _guard = TempFileGuard::new(temp_path.clone());
    let result = async {
        let mut target = File::create(&temp_path).await.map_err(|error| {
            AppError::IoError(format!("Failed to create reconstruction file: {error}"))
        })?;
        let mut hasher = Sha256::new();
        let mut total_written = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];

        for chunk in &chunks {
            let mut source = File::open(&chunk.chunk_path).await.map_err(|error| {
                AppError::IoError(format!("Failed to open chunk file: {error}"))
            })?;
            let mut remaining = chunk.length;
            while remaining > 0 {
                let read_len = remaining.min(buffer.len() as u64) as usize;
                let read = source
                    .read(&mut buffer[..read_len])
                    .await
                    .map_err(|error| AppError::IoError(format!("Failed to read chunk: {error}")))?;
                if read == 0 {
                    return Err(AppError::BadRequest(
                        "Chunk file is shorter than declared".to_string(),
                    ));
                }
                target.write_all(&buffer[..read]).await.map_err(|error| {
                    AppError::IoError(format!("Failed to write reconstruction: {error}"))
                })?;
                hasher.update(&buffer[..read]);
                remaining -= read as u64;
                total_written = total_written.checked_add(read as u64).ok_or_else(|| {
                    AppError::BadRequest("Reconstruction size overflows".to_string())
                })?;
            }
            if source.read(&mut buffer[..1]).await.map_err(|error| {
                AppError::IoError(format!("Failed to validate chunk size: {error}"))
            })? != 0
            {
                return Err(AppError::BadRequest(
                    "Chunk file is longer than declared".to_string(),
                ));
            }
        }
        if total_written != chunk_upload.upload_length
            || hex::encode(hasher.finalize()) != expected_sha256
        {
            return Err(AppError::Unauthorized(
                "Reconstructed blob does not match claimed SHA-256".to_string(),
            ));
        }
        target.sync_all().await.map_err(|error| {
            AppError::IoError(format!("Failed to sync reconstruction: {error}"))
        })?;
        drop(target);
        upload::finalize_upload(
            state,
            &temp_path,
            expected_sha256,
            total_written,
            get_extension_from_mime(&chunk_upload.upload_type),
            Some(chunk_upload.upload_type.clone()),
            chunk_upload.expiration,
        )
        .await?;
        Ok(state.create_blob_descriptor(
            expected_sha256,
            total_written,
            Some(chunk_upload.upload_type.clone()),
            chunk_upload.expiration,
        ))
    }
    .await;
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(&chunk.chunk_path).await;
    }
    result
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
                error!(
                    "Failed to clean up temp file {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}
