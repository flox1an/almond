use std::sync::Arc;

use tokio::fs::File;
use tokio::sync::watch;
use tracing::info;

use crate::error::{AppError, AppResult};
use crate::helpers::get_extension_from_mime;
use crate::models::{AppState, DownloadHandle, DownloadPhase, DownloadProgress};

pub struct PreparedDownload {
    pub handle: Arc<DownloadHandle>,
    pub writer: File,
}

/// Prepare the temp file and publish an attachable download handle.
///
/// The file exists before the handle is inserted, so followers that observe the
/// handle can always open the path.
pub async fn prepare_download_state(
    state: &AppState,
    filename: &str,
    content_type: &str,
    total_len: Option<u64>,
) -> AppResult<PreparedDownload> {
    let file_extension = get_extension_from_mime(content_type)
        .map(|ext| format!(".{}", ext))
        .unwrap_or_default();
    let temp_dir = state.upload_dir.join("temp");
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|e| AppError::IoError(format!("Failed to create temp directory: {e}")))?;

    let temp_filename = format!("upstream_{}{}", uuid::Uuid::new_v4(), file_extension);
    let temp_path = temp_dir.join(temp_filename);
    let writer = File::create(&temp_path)
        .await
        .map_err(|e| AppError::IoError(format!("Failed to create temp file: {e}")))?;
    let (progress, _) = watch::channel(DownloadProgress {
        written: 0,
        phase: DownloadPhase::Running,
    });
    let handle = Arc::new(DownloadHandle {
        started: std::time::Instant::now(),
        temp_path,
        content_type: content_type.to_string(),
        total_len,
        progress,
    });

    state
        .ongoing_downloads
        .write()
        .await
        .insert(filename.to_string(), handle.clone());
    info!(
        "Marked {} as being downloaded at {} (content-type: {}, extension: {})",
        filename,
        handle.temp_path.display(),
        content_type,
        file_extension
    );

    Ok(PreparedDownload { handle, writer })
}

pub struct DownloadGuard {
    state: AppState,
    key: String,
    handle: Arc<DownloadHandle>,
    armed: bool,
}

impl DownloadGuard {
    pub fn new(state: &AppState, key: &str, handle: Arc<DownloadHandle>) -> Self {
        Self {
            state: state.clone(),
            key: key.to_string(),
            handle,
            armed: true,
        }
    }

    pub async fn finish(mut self, phase: DownloadPhase) {
        self.handle.progress.send_modify(|progress| {
            progress.phase = phase;
        });
        remove_handle(&self.state, &self.key, &self.handle).await;
        self.armed = false;
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        self.handle.progress.send_if_modified(|progress| {
            if progress.phase == DownloadPhase::Running {
                progress.phase = DownloadPhase::Failed;
                true
            } else {
                false
            }
        });

        let state = self.state.clone();
        let key = std::mem::take(&mut self.key);
        let handle = self.handle.clone();
        tokio::spawn(async move {
            remove_handle(&state, &key, &handle).await;
        });
    }
}

async fn remove_handle(state: &AppState, filename: &str, handle: &Arc<DownloadHandle>) {
    let mut ongoing_downloads = state.ongoing_downloads.write().await;
    if ongoing_downloads
        .get(filename)
        .is_some_and(|current| Arc::ptr_eq(current, handle))
    {
        ongoing_downloads.remove(filename);
        info!("Removed {} from ongoing downloads", filename);
    }
}

/// Remove file from ongoing downloads tracking.
pub async fn remove_from_ongoing_downloads(state: &AppState, filename: &str) {
    let mut ongoing_downloads = state.ongoing_downloads.write().await;
    ongoing_downloads.remove(filename);
    info!("Removed {} from ongoing downloads", filename);
}

/// Check if a file is currently being downloaded.
pub async fn is_download_in_progress(state: &AppState, filename: &str) -> bool {
    state.ongoing_downloads.read().await.contains_key(filename)
}

/// Mark download as failed in cache.
pub async fn mark_failed_lookup(state: &AppState, filename: &str) {
    let mut failed_lookups = state.failed_upstream_lookups.write().await;
    failed_lookups.insert(filename.to_string(), std::time::Instant::now());
    info!("Added {} to failed upstream lookups cache", filename);
}

/// Check if download was recently failed (within 1 hour).
pub async fn is_recently_failed(state: &AppState, filename: &str) -> bool {
    let failed_lookups = state.failed_upstream_lookups.read().await;
    if let Some(failed_time) = failed_lookups.get(filename) {
        let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
        return *failed_time > one_hour_ago;
    }
    false
}
