use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use tokio::fs;
use tracing::{debug, error, info};

use crate::models::{AppState, FileLocation};
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

    // Delete every relevant native copy. Absence in either backend is success;
    // a backend failure is availability uncertainty and must not become a 404.
    if let Some(file_metadata) = state.file_index.get(&sha256).await {
        if let FileLocation::Local(file_path) = &file_metadata.location {
            match fs::remove_file(file_path).await {
                Ok(()) => debug!("✅ Deleted local blob: {}", file_path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    error!(
                        "Failed to delete local blob {}: {}",
                        file_path.display(),
                        error
                    );
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
            }
        }
    }

    if let Some(s3) = &state.native_s3 {
        s3.delete_matching(&sha256).await.map_err(|error| {
            error!("Failed to delete S3 blob {sha256}: {error}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    }

    state.file_index.remove(&sha256).await;
    *state.changes_pending.write().await = true;

    info!("🎉 Successfully deleted blob: {}", sha256);

    // Return 204 No Content on success
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
