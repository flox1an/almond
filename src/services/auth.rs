use base64::{engine::general_purpose::STANDARD, Engine as _};
use nostr_relay_pool::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info};

use crate::error::{AppError, AppResult};
use crate::models::AppState;

/// Authentication mode for different operations
#[derive(Debug, Clone, Copy)]
pub enum AuthMode {
    /// Standard mode - allows WoT (Web of Trust) for uploads if allowed_pubkeys is set
    Standard,
    /// Strict mode - only allows explicit whitelist (for delete operations)
    Strict,
    /// Unrestricted mode - only validates signature, no pubkey authorization checks (for public features)
    Unrestricted,
    /// WoT-only mode - requires WoT check, enforces trusted_pubkeys validation
    WotOnly,
    /// DVM-only mode - requires DVM announcement check, enforces dvm_pubkeys validation
    DvmOnly,
}

/// Parse and decode Nostr auth header
pub fn parse_auth_header(auth_header: &str) -> AppResult<Event> {
    if !auth_header.starts_with("Nostr ") {
        return Err(AppError::Unauthorized(
            "Invalid Authorization header prefix".to_string(),
        ));
    }

    let base64_str = &auth_header[6..]; // Remove "Nostr " prefix
    let decoded_bytes = STANDARD
        .decode(base64_str)
        .map_err(|e| AppError::Unauthorized(format!("Failed to decode base64: {}", e)))?;

    let json_str = String::from_utf8(decoded_bytes)
        .map_err(|e| AppError::Unauthorized(format!("Failed to convert to UTF-8: {}", e)))?;

    let event: Event = serde_json::from_str(&json_str)
        .map_err(|e| AppError::Unauthorized(format!("Failed to parse event JSON: {}", e)))?;

    Ok(event)
}

/// Verify event signature and expiration
pub fn verify_event(event: &Event) -> AppResult<()> {
    // Verify the event signature
    event
        .verify()
        .map_err(|e| AppError::Unauthorized(format!("Invalid event signature: {}", e)))?;

    // Check if the event is expired
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(expiration) = event.tags.find(TagKind::Expiration) {
        if let Some(exp_time) = expiration.content() {
            if let Ok(exp_time) = exp_time.parse::<u64>() {
                if now > exp_time {
                    return Err(AppError::Unauthorized("Event expired".to_string()));
                }
            }
        }
    }

    Ok(())
}

/// Check if pubkey is authorized based on mode
pub async fn check_pubkey_authorization(
    event: &Event,
    state: &AppState,
    mode: AuthMode,
) -> AppResult<()> {
    match mode {
        AuthMode::Strict => {
            // Strict mode: only check allowed_pubkeys, no WoT
            if state.allowed_pubkeys.is_empty() {
                return Err(AppError::Forbidden(
                    "Operation requires ALLOWED_NPUBS to be configured".to_string(),
                ));
            }

            if !state.allowed_pubkeys.contains(&event.pubkey) {
                return Err(AppError::Unauthorized(
                    "Pubkey not in ALLOWED_NPUBS whitelist".to_string(),
                ));
            }
        }
        AuthMode::Standard => {
            // Standard mode: check allowed_pubkeys first, then trusted_pubkeys (WoT)
            if !state.allowed_pubkeys.is_empty() && !state.allowed_pubkeys.contains(&event.pubkey)
            {
                let trusted_pubkeys = state.trusted_pubkeys.read().await;
                if !trusted_pubkeys.contains_key(&event.pubkey) {
                    return Err(AppError::Unauthorized("Pubkey not authorized".to_string()));
                }
            }
        }
        AuthMode::Unrestricted => {
            // Unrestricted mode: no pubkey authorization checks, only signature validation
            // (signature is already validated before this function is called)
        }
        AuthMode::WotOnly => {
            // WoT-only mode: check allowed_pubkeys first, then require WoT
            if !state.allowed_pubkeys.is_empty() && state.allowed_pubkeys.contains(&event.pubkey) {
                // Pubkey is in whitelist, allow it
                return Ok(());
            }

            // Check WoT
            let trusted_pubkeys = state.trusted_pubkeys.read().await;
            if !trusted_pubkeys.contains_key(&event.pubkey) {
                return Err(AppError::Unauthorized(
                    "Pubkey not in Web of Trust".to_string(),
                ));
            }
        }
        AuthMode::DvmOnly => {
            // DVM-only mode: check allowed_pubkeys first, then require DVM announcement
            if !state.allowed_pubkeys.is_empty() && state.allowed_pubkeys.contains(&event.pubkey) {
                return Ok(());
            }

            // Check cached DVM pubkeys
            {
                let dvm_pubkeys = state.dvm_pubkeys.read().await;
                if dvm_pubkeys.contains(&event.pubkey) {
                    return Ok(());
                }
            }

            // Fallback: Check if the pubkey has a recent announcement live (catch new DVMs between refreshes)
            match crate::trust_network::check_dvm_announcement(
                event.pubkey,
                &state.dvm_allowed_kinds,
                &state.dvm_relays,
            )
            .await
            {
                Ok(true) => {
                    info!("🤖 New DVM recognized: {}", event.pubkey.to_hex());
                    // Update cache
                    let mut dvm_pubkeys = state.dvm_pubkeys.write().await;
                    dvm_pubkeys.insert(event.pubkey);
                }
                Ok(false) => {
                    return Err(AppError::Unauthorized(
                        "Pubkey not a recognized DVM for allowed kinds".to_string(),
                    ));
                }
                Err(e) => {
                    error!("Failed to check DVM announcement for {}: {}", event.pubkey.to_hex(), e);
                    return Err(AppError::Unauthorized(
                        "Pubkey not a recognized DVM for allowed kinds".to_string(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Validate event kind
pub fn validate_event_kind(event: &Event, expected_kind: u16) -> AppResult<()> {
    if event.kind != Kind::Custom(expected_kind) {
        return Err(AppError::Unauthorized(format!(
            "Invalid event kind: expected {}, got {:?}",
            expected_kind, event.kind
        )));
    }
    Ok(())
}

/// Validate Nostr authentication with specified mode
pub async fn validate_nostr_auth(
    auth_header: &str,
    state: &AppState,
    mode: AuthMode,
) -> AppResult<Event> {
    let event = parse_auth_header(auth_header)?;
    verify_event(&event)?;
    validate_event_kind(&event, 24242)?;
    check_pubkey_authorization(&event, state, mode).await?;
    Ok(event)
}

/// Validate upload authorization - check for x tag with expected hash
pub fn validate_upload_auth(event: &Event, expected_sha256: &str) -> AppResult<()> {
    let x_tag = event.tags.find(TagKind::x()).ok_or_else(|| {
        error!("No x tag found in event");
        AppError::Unauthorized("No x tag found in event".to_string())
    })?;

    if x_tag.content() != Some(expected_sha256) {
        error!("No matching x tag found for hash {}", expected_sha256);
        return Err(AppError::Unauthorized(format!(
            "No matching x tag found for hash {}",
            expected_sha256
        )));
    }

    Ok(())
}

/// Validate delete authorization - check for t=delete tag and x tag with hash
pub fn validate_delete_auth(event: &Event, sha256: &str) -> AppResult<()> {
    // Check if the event has a 't' tag set to 'delete'
    let t_tag = event.tags.find(TagKind::Custom("t".into()));
    if t_tag.is_none() || t_tag.unwrap().content() != Some("delete") {
        return Err(AppError::Unauthorized(
            "Missing or invalid 't' tag for delete operation".to_string(),
        ));
    }

    // Check if the event has an 'x' tag matching the SHA-256 hash
    let x_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    if x_tags.is_empty() {
        return Err(AppError::Unauthorized(
            "No 'x' tags found in delete authorization".to_string(),
        ));
    }

    // Verify at least one x tag matches the SHA-256
    let has_matching_hash = x_tags.iter().any(|tag| {
        tag.content()
            .map(|content| content == sha256)
            .unwrap_or(false)
    });

    if !has_matching_hash {
        return Err(AppError::Unauthorized(format!(
            "No matching 'x' tag found for hash {}",
            sha256
        )));
    }

    Ok(())
}

/// Validate chunk upload authorization
pub fn validate_chunk_upload_auth(
    event: &Event,
    sha256: &str,
    _chunk_length: &str,
) -> AppResult<()> {
    // Check if the event has a 't' tag set to 'upload'
    let t_tag = event.tags.find(TagKind::Custom("t".into()));
    if t_tag.is_none() || t_tag.unwrap().content() != Some("upload") {
        return Err(AppError::Unauthorized(
            "Missing or invalid 't' tag for chunk upload".to_string(),
        ));
    }

    // Check if the event has an 'x' tag with the final blob hash
    let x_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    if x_tags.is_empty() {
        return Err(AppError::Unauthorized(
            "No 'x' tags found for chunk upload".to_string(),
        ));
    }

    // Verify final blob hash is present
    let has_final_hash = x_tags
        .iter()
        .any(|tag| tag.content().map(|content| content == sha256).unwrap_or(false));

    if !has_final_hash {
        return Err(AppError::Unauthorized(format!(
            "Final blob hash {} not found in x tags",
            sha256
        )));
    }

    Ok(())
}

/// Extract expected SHA-256 hash from auth event x tags
pub fn extract_sha256_from_event(event: &Event) -> Option<String> {
    let x_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("x".into()))
        .collect();

    for x_tag in x_tags {
        if let Some(content) = x_tag.content() {
            // Check if content looks like a SHA-256 hash (64 hex characters)
            if content.len() == 64 && content.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(content.to_string());
            }
        }
    }

    None
}

/// Check if a pubkey is authorized (in whitelist or WoT)
/// Returns true if the pubkey is in allowed_pubkeys or trusted_pubkeys
pub async fn is_pubkey_authorized(pubkey: &PublicKey, state: &AppState) -> bool {
    // Check whitelist first
    if !state.allowed_pubkeys.is_empty() && state.allowed_pubkeys.contains(pubkey) {
        return true;
    }

    // Check WoT
    let trusted_pubkeys = state.trusted_pubkeys.read().await;
    trusted_pubkeys.contains_key(pubkey)
}

/// Check if a pubkey is in the Web of Trust
/// Returns true only if the pubkey is in trusted_pubkeys (WoT)
pub async fn is_pubkey_in_wot(pubkey: &PublicKey, state: &AppState) -> bool {
    let trusted_pubkeys = state.trusted_pubkeys.read().await;
    trusted_pubkeys.contains_key(pubkey)
}
