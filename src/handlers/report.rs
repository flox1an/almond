use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::Response,
    Json,
};
use nostr_relay_pool::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tracing::{error, info, warn};

use crate::models::{AppState, FeatureMode, ReportAction};

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

/// Move blob to quarantine directory
async fn quarantine_blob(state: &AppState, sha256: &str, file_path: &PathBuf) -> Result<PathBuf, std::io::Error> {
    let quarantine_dir = state.upload_dir.join("quarantine");
    fs::create_dir_all(&quarantine_dir).await?;

    let file_name = file_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| sha256.to_string());

    let quarantine_path = quarantine_dir.join(&file_name);

    fs::rename(file_path, &quarantine_path).await?;

    Ok(quarantine_path)
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
        error!("Invalid report event kind: expected 1984, got {}", report.kind);
        return Err(StatusCode::BAD_REQUEST);
    }

    // Parse and validate the pubkey
    let reporter_pubkey = PublicKey::from_hex(&report.pubkey).map_err(|e| {
        error!("Invalid reporter pubkey: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Check if reporter is allowed
    let is_allowed = if state.allowed_pubkeys.contains(&reporter_pubkey) {
        true
    } else if state.feature_report_enabled == FeatureMode::Wot {
        // Check trusted pubkeys for WOT mode
        let trusted = state.trusted_pubkeys.read().await;
        trusted.contains_key(&reporter_pubkey)
    } else if state.feature_report_enabled == FeatureMode::Public {
        true
    } else {
        false
    };

    if !is_allowed {
        error!("Reporter {} not authorized to submit reports", report.pubkey);
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Parse the full event and verify signature
    let event_json = serde_json::to_string(&serde_json::json!({
        "id": report.id,
        "pubkey": report.pubkey,
        "created_at": report.created_at,
        "kind": report.kind,
        "tags": report.tags,
        "content": report.content,
        "sig": report.sig
    })).map_err(|e| {
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
        // Check if blob exists
        let file_index = state.file_index.read().await;
        let file_metadata = match file_index.get(sha256) {
            Some(metadata) => metadata.clone(),
            None => {
                warn!("Reported blob not found: {}", sha256);
                continue;
            }
        };
        drop(file_index);

        let file_path = file_metadata.path.clone();
        info!("📁 Processing reported blob: {} at {}", sha256, file_path.display());

        match state.report_action {
            ReportAction::Quarantine => {
                // Move to quarantine directory
                match quarantine_blob(&state, sha256, &file_path).await {
                    Ok(quarantine_path) => {
                        info!("🔒 Quarantined blob {} to {}", sha256, quarantine_path.display());

                        // Remove from file index
                        let mut file_index = state.file_index.write().await;
                        file_index.remove(sha256);
                        drop(file_index);

                        processed_hashes.push(sha256.clone());
                    }
                    Err(e) => {
                        error!("Failed to quarantine blob {}: {}", sha256, e);
                    }
                }
            }
            ReportAction::Delete => {
                // Delete the file permanently
                match fs::remove_file(&file_path).await {
                    Ok(()) => {
                        info!("🗑️  Deleted reported blob: {}", sha256);

                        // Remove from file index
                        let mut file_index = state.file_index.write().await;
                        file_index.remove(sha256);
                        drop(file_index);

                        processed_hashes.push(sha256.clone());
                    }
                    Err(e) => {
                        error!("Failed to delete blob {}: {}", sha256, e);
                    }
                }
            }
        }
    }

    // Mark changes pending for cleanup
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;

    if processed_hashes.is_empty() {
        warn!("No blobs were processed from report");
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&serde_json::json!({
                "error": "No matching blobs found"
            })).unwrap()))
            .unwrap());
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

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&response).unwrap()))
        .unwrap())
}
