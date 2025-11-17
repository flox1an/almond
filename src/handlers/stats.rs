use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde_json::json;

use crate::models::{AppState, ListQuery, Stats};

/// Handle stats requests
pub async fn get_stats(
    State(state): State<AppState>,
    Query(_params): Query<ListQuery>,
    _headers: HeaderMap,
) -> Json<serde_json::Value> {
    let files_uploaded = state.files_uploaded.read().await;
    let files_downloaded = state.files_downloaded.read().await;
    let file_index = state.file_index.read().await;
    let trusted_pubkeys = state.trusted_pubkeys.read().await;
    let upload_throughput_data = state.upload_throughput_data.read().await;

    let total_files = file_index.len();
    let total_size: u64 = file_index.values().map(|f| f.size).sum();
    let _total_trusted_pubkeys = trusted_pubkeys.len();

    let stats = Stats {
        files_uploaded: *files_uploaded,
        files_downloaded: *files_downloaded,
        total_files,
        total_size_bytes: total_size,
        total_size_mb: total_size as f64 / (1024.0 * 1024.0),
        upload_throughput_mbps: 0.0, // TODO: Calculate actual throughput
        download_throughput_mbps: 0.0, // TODO: Calculate actual throughput
        max_total_size_mb: 0.0, // TODO: Get from state
        max_total_files: 0, // TODO: Get from state
        storage_usage_percent: 0.0, // TODO: Calculate actual usage
    };

    let mut response = json!({
        "stats": stats,
        "upload_throughput": upload_throughput_data.len(),
    });

    // Add throughput data if requested
    if !upload_throughput_data.is_empty() {
        let upload_data: Vec<_> = upload_throughput_data
            .iter()
            .map(|(time, size)| {
                json!({
                    "timestamp": time.elapsed().as_secs(),
                    "size": size
                })
            })
            .collect();
        response["upload_throughput_data"] = json!(upload_data);
    }

    Json(response)
}
