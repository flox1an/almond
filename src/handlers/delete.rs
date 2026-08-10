use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use tracing::{debug, error, info};

use crate::models::AppState;
use crate::services::auth;
use crate::services::file_storage::{self, Removal};

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

    // Absence in either backend is success; a backend failure is availability
    // uncertainty and must not become a 404.
    file_storage::remove_indexed_blob(&state, &sha256, Removal::Requested, None)
        .await
        .map_err(|error| {
            error!("Failed to delete blob {sha256}: {error}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    info!("🎉 Successfully deleted blob: {}", sha256);

    // Return 204 No Content on success
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
