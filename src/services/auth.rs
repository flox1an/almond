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

/// Verify event signature, created_at, and expiration (BUD-11)
pub fn verify_event(event: &Event) -> AppResult<()> {
    // Verify the event signature
    event
        .verify()
        .map_err(|e| AppError::Unauthorized(format!("Invalid event signature: {}", e)))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // BUD-11: created_at must be in the past
    if event.created_at.as_secs() > now {
        return Err(AppError::Unauthorized(
            "Event created_at is in the future".to_string(),
        ));
    }

    // BUD-11: expiration tag MUST be present and in the future
    let expiration = event
        .tags
        .find(TagKind::Expiration)
        .ok_or_else(|| AppError::Unauthorized("Missing required expiration tag".to_string()))?;

    let exp_time = expiration
        .content()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| AppError::Unauthorized("Invalid expiration tag value".to_string()))?;

    if now > exp_time {
        return Err(AppError::Unauthorized("Event expired".to_string()));
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
    validate_server_tags(&event, &state.public_url)?;
    check_pubkey_authorization(&event, state, mode).await?;
    Ok(event)
}

/// Validate the `t` tag matches the expected verb (BUD-11)
pub fn validate_t_tag(event: &Event, expected_verb: &str) -> AppResult<()> {
    let t_tag = event.tags.find(TagKind::Custom("t".into()));
    if t_tag.is_none() || t_tag.unwrap().content() != Some(expected_verb) {
        return Err(AppError::Unauthorized(format!(
            "Missing or invalid 't' tag: expected '{}'",
            expected_verb
        )));
    }
    Ok(())
}

/// Validate `server` tags if present (BUD-11)
/// If the event has server tags, the server's domain must match at least one.
pub fn validate_server_tags(event: &Event, public_url: &str) -> AppResult<()> {
    let server_tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == TagKind::Custom("server".into()))
        .collect();

    if server_tags.is_empty() {
        return Ok(());
    }

    // Extract domain from public_url by stripping scheme and path
    let server_domain = extract_domain(public_url);

    let matches = server_tags.iter().any(|tag| {
        tag.content()
            .map(|content| content.to_lowercase() == server_domain)
            .unwrap_or(false)
    });

    if !matches {
        return Err(AppError::Unauthorized(
            "Auth event server tag does not match this server".to_string(),
        ));
    }

    Ok(())
}

/// Extract lowercase domain from a URL string (e.g. "https://example.com:3000/path" -> "example.com")
fn extract_domain(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    // Take everything before the first '/' or ':'
    without_scheme
        .split(&['/', ':'][..])
        .next()
        .unwrap_or(without_scheme)
        .to_lowercase()
}

/// Validate upload authorization - check for t=upload tag and x tag with expected hash (BUD-11)
pub fn validate_upload_auth(event: &Event, expected_sha256: &str) -> AppResult<()> {
    validate_t_tag(event, "upload")?;

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
    validate_t_tag(event, "delete")?;

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
    validate_t_tag(event, "upload")?;

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

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HASH: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Build a signed event with given tags, kind 24242, created_at = now
    fn build_event(keys: &Keys, tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::Custom(24242), "test auth event")
            .tags(tags)
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Build a signed event with a custom created_at timestamp
    fn build_event_with_created_at(keys: &Keys, tags: Vec<Tag>, created_at: u64) -> Event {
        EventBuilder::new(Kind::Custom(24242), "test auth event")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Build a signed event with a custom kind
    fn build_event_with_kind(keys: &Keys, tags: Vec<Tag>, kind: u16) -> Event {
        EventBuilder::new(Kind::Custom(kind), "test auth event")
            .tags(tags)
            .sign_with_keys(keys)
            .unwrap()
    }

    fn valid_expiration_tag() -> Tag {
        Tag::expiration(Timestamp::from_secs(now_secs() + 3600))
    }

    fn expired_tag() -> Tag {
        Tag::expiration(Timestamp::from_secs(now_secs() - 100))
    }

    fn t_tag(verb: &str) -> Tag {
        Tag::custom(TagKind::Custom("t".into()), vec![verb])
    }

    fn x_tag(hash: &str) -> Tag {
        Tag::custom(TagKind::Custom("x".into()), vec![hash])
    }

    fn server_tag(domain: &str) -> Tag {
        Tag::custom(TagKind::Custom("server".into()), vec![domain])
    }

    // ── verify_event tests ──

    #[test]
    fn test_verify_event_valid() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        assert!(verify_event(&event).is_ok());
    }

    #[test]
    fn test_verify_event_missing_expiration() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![]);
        let err = verify_event(&event).unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("expiration")));
    }

    #[test]
    fn test_verify_event_expired() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![expired_tag()]);
        let err = verify_event(&event).unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("expired")));
    }

    #[test]
    fn test_verify_event_created_at_in_future() {
        let keys = Keys::generate();
        let event = build_event_with_created_at(
            &keys,
            vec![valid_expiration_tag()],
            now_secs() + 3600,
        );
        let err = verify_event(&event).unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("future")));
    }

    #[test]
    fn test_verify_event_created_at_in_past() {
        let keys = Keys::generate();
        let event = build_event_with_created_at(
            &keys,
            vec![valid_expiration_tag()],
            now_secs() - 60,
        );
        assert!(verify_event(&event).is_ok());
    }

    // ── validate_event_kind tests ──

    #[test]
    fn test_validate_event_kind_correct() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        assert!(validate_event_kind(&event, 24242).is_ok());
    }

    #[test]
    fn test_validate_event_kind_wrong() {
        let keys = Keys::generate();
        let event = build_event_with_kind(&keys, vec![valid_expiration_tag()], 1);
        let err = validate_event_kind(&event, 24242).unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("kind")));
    }

    // ── validate_t_tag tests ──

    #[test]
    fn test_validate_t_tag_upload() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag(), t_tag("upload")]);
        assert!(validate_t_tag(&event, "upload").is_ok());
    }

    #[test]
    fn test_validate_t_tag_delete() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag(), t_tag("delete")]);
        assert!(validate_t_tag(&event, "delete").is_ok());
    }

    #[test]
    fn test_validate_t_tag_missing() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        let err = validate_t_tag(&event, "upload").unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("'t' tag")));
    }

    #[test]
    fn test_validate_t_tag_wrong_verb() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag(), t_tag("delete")]);
        let err = validate_t_tag(&event, "upload").unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("upload")));
    }

    #[test]
    fn test_validate_t_tag_prevents_cross_endpoint_reuse() {
        let keys = Keys::generate();
        // A delete token should not work for upload
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("delete"), x_tag(TEST_HASH)],
        );
        assert!(validate_upload_auth(&event, TEST_HASH).is_err());
    }

    // ── validate_server_tags tests ──

    #[test]
    fn test_server_tag_no_tags_allows_any() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        assert!(validate_server_tags(&event, "https://example.com").is_ok());
    }

    #[test]
    fn test_server_tag_matching_domain() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), server_tag("example.com")],
        );
        assert!(validate_server_tags(&event, "https://example.com").is_ok());
    }

    #[test]
    fn test_server_tag_matching_domain_with_port() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), server_tag("example.com")],
        );
        assert!(validate_server_tags(&event, "https://example.com:3000").is_ok());
    }

    #[test]
    fn test_server_tag_matching_domain_with_path() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), server_tag("example.com")],
        );
        assert!(validate_server_tags(&event, "https://example.com/path").is_ok());
    }

    #[test]
    fn test_server_tag_case_insensitive() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), server_tag("Example.COM")],
        );
        assert!(validate_server_tags(&event, "https://example.com").is_ok());
    }

    #[test]
    fn test_server_tag_wrong_domain() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), server_tag("other.com")],
        );
        let err = validate_server_tags(&event, "https://example.com").unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(msg) if msg.contains("server tag")));
    }

    #[test]
    fn test_server_tag_multiple_one_matches() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![
                valid_expiration_tag(),
                server_tag("other.com"),
                server_tag("example.com"),
            ],
        );
        assert!(validate_server_tags(&event, "https://example.com").is_ok());
    }

    #[test]
    fn test_server_tag_multiple_none_match() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![
                valid_expiration_tag(),
                server_tag("other.com"),
                server_tag("another.com"),
            ],
        );
        assert!(validate_server_tags(&event, "https://example.com").is_err());
    }

    // ── validate_upload_auth tests ──

    #[test]
    fn test_upload_auth_valid() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("upload"), x_tag(TEST_HASH)],
        );
        assert!(validate_upload_auth(&event, TEST_HASH).is_ok());
    }

    #[test]
    fn test_upload_auth_missing_t_tag() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), x_tag(TEST_HASH)],
        );
        assert!(validate_upload_auth(&event, TEST_HASH).is_err());
    }

    #[test]
    fn test_upload_auth_wrong_hash() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("upload"), x_tag(TEST_HASH)],
        );
        let other_hash = "b1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert!(validate_upload_auth(&event, other_hash).is_err());
    }

    #[test]
    fn test_upload_auth_missing_x_tag() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("upload")],
        );
        assert!(validate_upload_auth(&event, TEST_HASH).is_err());
    }

    // ── validate_delete_auth tests ──

    #[test]
    fn test_delete_auth_valid() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("delete"), x_tag(TEST_HASH)],
        );
        assert!(validate_delete_auth(&event, TEST_HASH).is_ok());
    }

    #[test]
    fn test_delete_auth_missing_t_tag() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), x_tag(TEST_HASH)],
        );
        assert!(validate_delete_auth(&event, TEST_HASH).is_err());
    }

    #[test]
    fn test_delete_auth_wrong_t_tag() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("upload"), x_tag(TEST_HASH)],
        );
        assert!(validate_delete_auth(&event, TEST_HASH).is_err());
    }

    #[test]
    fn test_delete_auth_wrong_hash() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("delete"), x_tag(TEST_HASH)],
        );
        let other_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(validate_delete_auth(&event, other_hash).is_err());
    }

    #[test]
    fn test_delete_auth_multiple_x_tags_one_matches() {
        let keys = Keys::generate();
        let other_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let event = build_event(
            &keys,
            vec![
                valid_expiration_tag(),
                t_tag("delete"),
                x_tag(other_hash),
                x_tag(TEST_HASH),
            ],
        );
        assert!(validate_delete_auth(&event, TEST_HASH).is_ok());
    }

    // ── validate_chunk_upload_auth tests ──

    #[test]
    fn test_chunk_upload_auth_valid() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("upload"), x_tag(TEST_HASH)],
        );
        assert!(validate_chunk_upload_auth(&event, TEST_HASH, "1024").is_ok());
    }

    #[test]
    fn test_chunk_upload_auth_wrong_t_tag() {
        let keys = Keys::generate();
        let event = build_event(
            &keys,
            vec![valid_expiration_tag(), t_tag("delete"), x_tag(TEST_HASH)],
        );
        assert!(validate_chunk_upload_auth(&event, TEST_HASH, "1024").is_err());
    }

    // ── parse_auth_header tests ──

    #[test]
    fn test_parse_auth_header_invalid_prefix() {
        let err = parse_auth_header("Bearer abc123").unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[test]
    fn test_parse_auth_header_valid() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        let json = serde_json::to_string(&event).unwrap();
        let b64 = STANDARD.encode(json.as_bytes());
        let header = format!("Nostr {}", b64);
        let parsed = parse_auth_header(&header).unwrap();
        assert_eq!(parsed.id, event.id);
    }

    // ── extract_domain tests ──

    #[test]
    fn test_extract_domain_https() {
        assert_eq!(extract_domain("https://example.com"), "example.com");
    }

    #[test]
    fn test_extract_domain_with_port() {
        assert_eq!(extract_domain("https://example.com:3000"), "example.com");
    }

    #[test]
    fn test_extract_domain_with_path() {
        assert_eq!(extract_domain("https://example.com/path/to"), "example.com");
    }

    #[test]
    fn test_extract_domain_http() {
        assert_eq!(extract_domain("http://localhost:3000"), "localhost");
    }

    #[test]
    fn test_extract_domain_uppercase() {
        assert_eq!(extract_domain("https://EXAMPLE.COM"), "example.com");
    }

    // ── extract_sha256_from_event tests ──

    #[test]
    fn test_extract_sha256_valid() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag(), x_tag(TEST_HASH)]);
        assert_eq!(extract_sha256_from_event(&event), Some(TEST_HASH.to_string()));
    }

    #[test]
    fn test_extract_sha256_no_x_tag() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag()]);
        assert_eq!(extract_sha256_from_event(&event), None);
    }

    #[test]
    fn test_extract_sha256_invalid_length() {
        let keys = Keys::generate();
        let event = build_event(&keys, vec![valid_expiration_tag(), x_tag("tooshort")]);
        assert_eq!(extract_sha256_from_event(&event), None);
    }
}
