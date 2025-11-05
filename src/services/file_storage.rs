use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tracing::{error, info};

use crate::error::{AppError, AppResult};
use crate::models::{AppState, FileMetadata};

/// Calculate nested directory path for a hash (e.g., a/b/abc123.jpg)
pub fn get_nested_path(upload_dir: &Path, hash: &str, extension: Option<&str>) -> PathBuf {
    let first_level = &hash[..1];
    let second_level = &hash[1..2];
    let mut path = upload_dir.join(first_level).join(second_level);

    if let Some(ext) = extension {
        path = path.join(format!("{}.{}", hash, ext));
    } else {
        path = path.join(hash);
    }

    path
}

/// Create parent directories for a file path
pub async fn create_parent_dirs(path: &Path) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            error!("Failed to create directory {}: {}", parent.display(), e);
            AppError::IoError(format!("Failed to create directory: {}", e))
        })?;
    }
    Ok(())
}

/// Move a file from source to destination, creating parent directories
pub async fn move_file(from: &Path, to: &Path) -> AppResult<()> {
    create_parent_dirs(to).await?;
    
    fs::rename(from, to).await.map_err(|e| {
        error!("Failed to move file from {} to {}: {}", from.display(), to.display(), e);
        AppError::IoError(format!("Failed to move file: {}", e))
    })?;
    
    info!("Successfully moved file from {} to {}", from.display(), to.display());
    Ok(())
}

/// Add file to the index
pub async fn add_to_index(
    state: &AppState,
    sha256: &str,
    path: PathBuf,
    extension: Option<String>,
    mime_type: Option<String>,
    size: u64,
    expiration: Option<u64>,
) -> AppResult<()> {
    let key = sha256[..64.min(sha256.len())].to_string();

    state.file_index.write().await.insert(
        key.clone(),
        FileMetadata {
            path,
            extension,
            mime_type,
            size,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            expiration,
        },
    );

    if let Some(exp) = expiration {
        info!("Added file to index: {} (expires: {})", key, exp);
    } else {
        info!("Added file to index: {}", key);
    }

    // Save metadata to disk if expiration is set
    if expiration.is_some() {
        use crate::utils::save_persisted_metadata;
        save_persisted_metadata(&state.upload_dir, &state.file_index).await;
    }

    Ok(())
}

/// Remove file from index
pub async fn remove_from_index(state: &AppState, sha256: &str) -> AppResult<()> {
    let mut file_index = state.file_index.write().await;
    file_index.remove(sha256);
    info!("Removed file from index: {}", sha256);
    Ok(())
}

/// Get file metadata from index
pub async fn get_file_metadata(state: &AppState, sha256: &str) -> Option<FileMetadata> {
    let file_index = state.file_index.read().await;
    file_index.get(sha256).cloned()
}

/// Delete file from disk and index
pub async fn delete_file(state: &AppState, sha256: &str) -> AppResult<()> {
    // Get file metadata
    let file_metadata = {
        let file_index = state.file_index.read().await;
        file_index.get(sha256).cloned().ok_or_else(|| {
            AppError::NotFound(format!("File not found: {}", sha256))
        })?
    };

    // Delete physical file
    fs::remove_file(&file_metadata.path).await.map_err(|e| {
        error!("Failed to delete file {}: {}", file_metadata.path.display(), e);
        AppError::IoError(format!("Failed to delete file: {}", e))
    })?;

    info!("Deleted file from disk: {}", file_metadata.path.display());

    // Remove from index
    remove_from_index(state, sha256).await?;

    Ok(())
}

/// Mark changes as pending for cleanup job
pub async fn mark_changes_pending(state: &AppState) {
    let mut changes_pending = state.changes_pending.write().await;
    *changes_pending = true;
}

/// Create a temporary file path with optional extension
pub fn create_temp_path(state: &AppState, prefix: &str, extension: Option<&str>) -> PathBuf {
    let temp_dir = state.upload_dir.join("temp");
    let uuid = uuid::Uuid::new_v4();
    
    let filename = if let Some(ext) = extension {
        format!("{}_{}.{}", prefix, uuid, ext)
    } else {
        format!("{}_{}", prefix, uuid)
    };
    
    temp_dir.join(filename)
}

/// Ensure temp directory exists
pub async fn ensure_temp_dir(state: &AppState) -> AppResult<PathBuf> {
    let temp_dir = state.upload_dir.join("temp");
    fs::create_dir_all(&temp_dir).await.map_err(|e| {
        error!("Failed to create temp directory: {}", e);
        AppError::IoError(format!("Failed to create temp directory: {}", e))
    })?;
    Ok(temp_dir)
}

/// Validate SHA-256 hash format
pub fn validate_sha256_format(sha256: &str) -> AppResult<()> {
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(format!(
            "Invalid SHA-256 hash format: {}",
            sha256
        )));
    }
    Ok(())
}

/// Extract SHA-256 hash from filename (handles both "hash" and "hash.ext" formats)
pub fn extract_sha256_from_filename(filename: &str) -> Option<String> {
    let hash = filename.split('.').next()?;
    
    // Validate it looks like a SHA-256 hash
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_string())
    } else {
        None
    }
}
