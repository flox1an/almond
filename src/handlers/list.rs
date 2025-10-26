use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde_json::json;

use crate::models::{AppState, ListQuery};

/// Handle list requests
pub async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
    _headers: HeaderMap,
) -> Json<serde_json::Value> {
    let file_index = state.file_index.read().await;
    let mut files: Vec<_> = file_index
        .iter()
        .map(|(key, metadata)| {
            json!({
                "sha256": key,
                "size": metadata.size,
                "mime_type": metadata.mime_type,
                "extension": metadata.extension,
                "created_at": metadata.created_at
            })
        })
        .collect();

    // Sort by created_at descending (newest first)
    files.sort_by(|a, b| {
        let a_time = a["created_at"].as_u64().unwrap_or(0);
        let b_time = b["created_at"].as_u64().unwrap_or(0);
        b_time.cmp(&a_time)
    });

    // Apply pagination (using since/until as offset/limit for now)
    let start = params.since.unwrap_or(0) as usize;
    let limit = params.until.unwrap_or(100) as usize;
    let end = (start + limit).min(files.len());

    let paginated_files = if start < files.len() {
        files[start..end].to_vec()
    } else {
        Vec::new()
    };

    let response = json!({
        "files": paginated_files,
        "total": files.len(),
        "offset": start,
        "limit": limit
    });

    Json(response)
}
