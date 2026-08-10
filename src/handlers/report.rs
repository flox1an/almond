use axum::{body::Body, extract::State, http::StatusCode, response::Response, Json};
use nostr_relay_pool::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::models::{AppState, FeatureMode, ReportAction};
use crate::services::file_storage::{self, Removal};

/// NIP-56 Report event structure
#[derive(Debug, Deserialize)]
pub struct ReportEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// Response for successful report
#[derive(Serialize)]
pub struct ReportResponse {
    pub message: String,
    pub processed: Vec<String>,
    pub action: String,
}

/// Extract blob hashes from x tags in the report event
fn extract_blob_hashes(tags: &[Vec<String>]) -> Vec<String> {
    tags.iter()
        .filter_map(|tag| {
            if tag.len() >= 2 && tag[0] == "x" {
                // Validate SHA-256 format (64 hex characters)
                let hash = &tag[1];
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    Some(hash.clone())
                } else {
                    warn!("Invalid hash format in x tag: {}", hash);
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

/// Extract report type from tags (spam, illegal, nudity, etc.)
fn extract_report_type(tags: &[Vec<String>]) -> Option<String> {
    // Look for the report type in x tags (second element after hash)
    for tag in tags {
        if tag.len() >= 3 && tag[0] == "x" {
            return Some(tag[2].clone());
        }
    }
    None
}

/// Handle blob report (BUD-09)
/// PUT /report
pub async fn report_blob(
    State(state): State<AppState>,
    Json(report): Json<ReportEvent>,
) -> Result<Response<Body>, StatusCode> {
    // Check if reports feature is enabled
    if !state.feature_report_enabled.is_enabled() {
        error!("Reports feature is disabled");
        return Err(StatusCode::NOT_FOUND);
    }

    info!("📋 Received report from pubkey: {}", report.pubkey);

    // Validate report event kind (must be 1984 for NIP-56)
    if report.kind != 1984 {
        error!(
            "Invalid report event kind: expected 1984, got {}",
            report.kind
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    // Parse and validate the pubkey
    let reporter_pubkey = PublicKey::from_hex(&report.pubkey).map_err(|e| {
        error!("Invalid reporter pubkey: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Reports can only perform a destructive state transition under the same
    // explicit whitelist required by DELETE.  Public reports are accepted as
    // non-destructive signals after signature validation below.
    let is_allowed = state.allowed_pubkeys.contains(&reporter_pubkey);

    // Parse the full event and verify signature
    let event_json = serde_json::to_string(&serde_json::json!({
        "id": report.id,
        "pubkey": report.pubkey,
        "created_at": report.created_at,
        "kind": report.kind,
        "tags": report.tags,
        "content": report.content,
        "sig": report.sig
    }))
    .map_err(|e| {
        error!("Failed to serialize event: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let event: Event = serde_json::from_str(&event_json).map_err(|e| {
        error!("Failed to parse event: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Verify event signature
    if let Err(e) = event.verify() {
        error!("Invalid event signature: {}", e);
        return Err(StatusCode::UNAUTHORIZED);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let created_at = u64::try_from(report.created_at).map_err(|_| StatusCode::BAD_REQUEST)?;
    if created_at > now.saturating_add(state.auth_clock_skew_secs)
        || now.saturating_sub(created_at) > state.auth_max_age_secs
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    {
        let mut seen = state.destructive_event_replays.write().await;
        seen.retain(|_, expires_at| *expires_at >= now);
        if seen
            .insert(
                report.id.clone(),
                now.saturating_add(state.auth_max_ttl_secs),
            )
            .is_some()
        {
            return Err(StatusCode::CONFLICT);
        }
    }
    if state.feature_report_enabled == FeatureMode::Public {
        let body = serde_json::to_string(&ReportResponse {
            message: "Report accepted for moderation".to_string(),
            processed: Vec::new(),
            action: "none".to_string(),
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    if !is_allowed {
        return Err(StatusCode::UNAUTHORIZED);
    }

    info!("✅ Report signature verified");

    // Extract blob hashes from x tags
    let blob_hashes = extract_blob_hashes(&report.tags);

    if blob_hashes.is_empty() {
        error!("No valid blob hashes found in report");
        return Err(StatusCode::BAD_REQUEST);
    }

    let report_type = extract_report_type(&report.tags);
    info!(
        "📋 Report contains {} blob(s), type: {:?}, content: {}",
        blob_hashes.len(),
        report_type,
        if report.content.len() > 100 {
            format!("{}...", &report.content[..100])
        } else {
            report.content.clone()
        }
    );

    let mut processed_hashes: Vec<String> = Vec::new();

    // Process each reported blob
    for sha256 in &blob_hashes {
        if state.file_index.get(sha256).await.is_none() {
            warn!("Reported blob not found: {}", sha256);
            continue;
        }

        let removal = match state.report_action {
            ReportAction::Quarantine => Removal::Quarantined,
            ReportAction::Delete => Removal::Reported,
        };

        match file_storage::remove_indexed_blob(&state, sha256, removal, None).await {
            Ok(true) => {
                info!(sha256, action = ?state.report_action, "Processed reported blob");
                processed_hashes.push(sha256.clone());
            }
            Ok(false) => warn!("Reported blob vanished before it could be processed: {sha256}"),
            Err(error) => error!("Failed to process reported blob {sha256}: {error}"),
        }
    }

    if processed_hashes.is_empty() {
        warn!("No blobs were processed from report");
        let body = serde_json::to_string(&serde_json::json!({
            "error": "No matching blobs found"
        }))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!(
        "🎉 Report processed: {} blob(s) {}",
        processed_hashes.len(),
        state.report_action.as_str()
    );

    let response = ReportResponse {
        message: format!(
            "Report processed: {} blob(s) {}",
            processed_hashes.len(),
            if state.report_action == ReportAction::Quarantine {
                "quarantined"
            } else {
                "deleted"
            }
        ),
        processed: processed_hashes,
        action: state.report_action.as_str().to_string(),
    };

    let body = serde_json::to_string(&response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
