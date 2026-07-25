use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use tokio::fs;
use tracing::{debug, error, info, warn};

use crate::models::AppState;
use crate::services::auth;

/// Handle blob deletion
pub async fn delete_blob(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Extract SHA-256 hash from filename (remove extension if present)
    let sha256 = filename
        .split('.')
        .next()
        .ok_or_else(|| {
            error!("Invalid filename format: {}", filename);
            StatusCode::BAD_REQUEST
        })?
        .to_string();

    // Validate SHA-256 format (64 hex characters)
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        error!("Invalid SHA-256 hash format: {}", sha256);
        return Err(StatusCode::BAD_REQUEST);
    }

    debug!("🗑️  Delete request for blob: {}", sha256);

    // Validate Nostr authorization (strict mode - no WOT)
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing Authorization header");
            StatusCode::UNAUTHORIZED
        })?;

    let auth_event = auth::validate_nostr_auth(auth_header, &state, auth::AuthMode::Strict)
        .await
        .map_err(StatusCode::from)?;

    debug!(
        "✅ Authorization validated for pubkey: {}",
        auth_event.pubkey
    );

    // Validate delete-specific authorization (t=delete tag + x tag)
    auth::validate_delete_auth(&auth_event, &sha256).map_err(StatusCode::from)?;

    debug!("✅ Delete authorization tags validated");

    // Check if blob exists in file index
    let file_metadata = state.file_index.get(&sha256).await.ok_or_else(|| {
        warn!("Blob not found in index: {}", sha256);
        StatusCode::NOT_FOUND
    })?;
    let file_path = file_metadata.path.clone();

    debug!("📁 Found blob at: {}", file_path.display());

    // Delete the physical file
    fs::remove_file(&file_path).await.map_err(|e| {
        error!("Failed to delete file {}: {}", file_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    debug!("✅ Deleted file from disk: {}", file_path.display());

    // Remove from file index
    state.file_index.remove(&sha256).await;

    debug!("✅ Removed blob from index: {}", sha256);

    // Mark changes pending for cleanup (empty directory cleanup)
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    info!("🎉 Successfully deleted blob: {}", sha256);

    // Return 204 No Content on success
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
