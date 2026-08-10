use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use tracing::{debug, error, info};

use crate::models::AppState;
use crate::services::authorization;
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

    // Authorization is bound to this hash and consumes its single-use nonce,
    // so the same signed event cannot delete twice.
    let authorized =
        authorization::authorize(&headers, &state, authorization::Operation::Delete)
            .await
            .map_err(StatusCode::from)?;
    authorized
        .bind(&state, &sha256)
        .await
        .map_err(StatusCode::from)?;

    debug!("✅ Delete authorization validated");

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
