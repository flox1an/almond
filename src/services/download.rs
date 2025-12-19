use std::path::PathBuf;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::sync::Notify;
use tracing::info;

use crate::error::AppResult;
use crate::helpers::get_extension_from_mime;
use crate::models::AppState;

/// Prepare download state for tracking ongoing downloads
pub async fn prepare_download_state(
    state: &AppState,
    filename: &str,
    content_type: &str,
) -> AppResult<(PathBuf, Arc<AtomicU64>, Arc<Notify>)> {
    // Derive extension from content type
    let file_extension = get_extension_from_mime(content_type)
        .map(|ext| format!(".{}", ext))
        .unwrap_or_default();

    // Create temp file with proper extension
    let temp_dir = state.upload_dir.join("temp");
    let temp_filename = format!("upstream_{}{}", uuid::Uuid::new_v4(), file_extension);
    let temp_path = temp_dir.join(temp_filename);

    // Mark this file as being downloaded with shared state
    let written_len = Arc::new(AtomicU64::new(0));
    let notify = Arc::new(Notify::new());
    
    {
        let mut ongoing_downloads = state.ongoing_downloads.write().await;
        ongoing_downloads.insert(
            filename.to_string(),
            (
                std::time::Instant::now(),
                written_len.clone(),
                notify.clone(),
                temp_path.clone(),
                content_type.to_string(),
            ),
        );
        info!(
            "Marked {} as being downloaded at {} (content-type: {}, extension: {})",
            filename, temp_path.display(), content_type, file_extension
        );
    }

    Ok((temp_path, written_len, notify))
}

/// Remove file from ongoing downloads tracking
pub async fn remove_from_ongoing_downloads(state: &AppState, filename: &str) {
    let mut ongoing_downloads = state.ongoing_downloads.write().await;
    ongoing_downloads.remove(filename);
    info!("Removed {} from ongoing downloads", filename);
}

/// Check if a file is currently being downloaded
pub async fn is_download_in_progress(state: &AppState, filename: &str) -> bool {
    state.ongoing_downloads.read().await.contains_key(filename)
}

/// Mark download as failed in cache
pub async fn mark_failed_lookup(state: &AppState, filename: &str) {
    let mut failed_lookups = state.failed_upstream_lookups.write().await;
    failed_lookups.insert(filename.to_string(), std::time::Instant::now());
    info!("Added {} to failed upstream lookups cache", filename);
}

/// Check if download was recently failed (within 1 hour)
pub async fn is_recently_failed(state: &AppState, filename: &str) -> bool {
    let failed_lookups = state.failed_upstream_lookups.read().await;
    if let Some(failed_time) = failed_lookups.get(filename) {
        let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
        return *failed_time > one_hour_ago;
    }
    false
}
