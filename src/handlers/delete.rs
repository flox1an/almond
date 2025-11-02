use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use nostr_relay_pool::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tracing::{error, info, warn};

use crate::models::AppState;

/// Validate Nostr authentication for delete operation (strict mode - no WOT)
async fn validate_nostr_auth_strict(auth: &str, state: &AppState) -> Result<Event, StatusCode> {
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

    // Check if the event kind is correct (24242 for delete)
    if event.kind != Kind::Custom(24242) {
        error!("Invalid event kind: expected 24242, got {:?}", event.kind);
        return Err(StatusCode::UNAUTHORIZED);
    }

    // STRICT: Delete requires explicit whitelist - no WOT fallback
    if state.allowed_pubkeys.is_empty() {
        error!("Delete operation requires ALLOWED_NPUBS to be configured");
        return Err(StatusCode::FORBIDDEN);
    }

    // STRICT: Only check allowed_pubkeys, do NOT check trusted_pubkeys (no WOT for delete)
    if !state.allowed_pubkeys.contains(&event.pubkey) {
        error!(
            "Pubkey {} not in ALLOWED_NPUBS whitelist (WOT not allowed for delete)",
            event.pubkey
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(event)
}

/// Validate authorization event for delete operation
fn validate_delete_auth(event: &Event, sha256: &str) -> Result<(), StatusCode> {
    // Check if the event has a 't' tag set to 'delete'
    let t_tag = event.tags.find(TagKind::Custom("t".into()));
    if t_tag.is_none() || t_tag.unwrap().content() != Some("delete") {
        error!("Missing or invalid 't' tag for delete operation");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Check if the event has an 'x' tag matching the SHA-256 hash
    let x_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    if x_tags.is_empty() {
        error!("No 'x' tags found in delete authorization");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Verify at least one x tag matches the SHA-256
    let has_matching_hash = x_tags.iter().any(|tag| {
        tag.content()
            .map(|content| content == sha256)
            .unwrap_or(false)
    });

    if !has_matching_hash {
        error!("No matching 'x' tag found for hash {}", sha256);
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(())
}

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

    info!("🗑️  Delete request for blob: {}", sha256);

    // Validate Nostr authorization (strict mode - no WOT)
    let auth = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        error!("Missing Authorization header");
        StatusCode::UNAUTHORIZED
    })?;

    let auth_event: Event = validate_nostr_auth_strict(
        auth.to_str().map_err(|_| {
            error!("Invalid Authorization header format");
            StatusCode::UNAUTHORIZED
        })?,
        &state,
    )
    .await?;

    info!(
        "✅ Authorization validated for pubkey: {}",
        auth_event.pubkey
    );

    // Validate delete-specific authorization
    validate_delete_auth(&auth_event, &sha256)?;

    info!("✅ Delete authorization tags validated");

    // Check if blob exists in file index
    let file_index = state.file_index.read().await;
    let file_metadata = file_index.get(&sha256).ok_or_else(|| {
        warn!("Blob not found in index: {}", sha256);
        StatusCode::NOT_FOUND
    })?;

    let file_path = file_metadata.path.clone();
    drop(file_index); // Release read lock before modifying

    info!("📁 Found blob at: {}", file_path.display());

    // Delete the physical file
    fs::remove_file(&file_path).await.map_err(|e| {
        error!("Failed to delete file {}: {}", file_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    info!("✅ Deleted file from disk: {}", file_path.display());

    // Remove from file index
    let mut file_index = state.file_index.write().await;
    file_index.remove(&sha256);
    drop(file_index);

    info!("✅ Removed blob from index: {}", sha256);

    // Mark changes pending for cleanup (empty directory cleanup)
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    info!("🎉 Successfully deleted blob: {}", sha256);

    // Return 204 No Content on success
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}
