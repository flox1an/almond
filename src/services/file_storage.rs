use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::models::{AppState, BlobOrigin, FileLocation, FileMetadata};
use crate::services::blob_index::PublishResult;
use crate::services::blob_name;

/// The caller-provided properties of one completed blob.
pub struct BlobPublication {
    pub sha256: String,
    pub origin: BlobOrigin,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    pub size: u64,
    pub expiration: Option<u64>,
}

impl BlobPublication {
    fn metadata(&self, location: FileLocation) -> FileMetadata {
        FileMetadata {
            location,
            extension: self.extension.clone(),
            mime_type: self.mime_type.clone(),
            size: self.size,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            pubkey: None,
            expiration: self.expiration,
            origin: self.origin,
        }
    }
}

/// Build the completed-blob path inside the root selected by `origin`.
fn completed_path(
    state: &AppState,
    origin: BlobOrigin,
    hash: &str,
    extension: Option<&str>,
    expiration: Option<u64>,
) -> PathBuf {
    // `publish_blob` validates the hash (via `validate_sha256_format`) before
    // calling, so the grammar owner's checks cannot fail here.
    let (h0, h1) = blob_name::fan_out(hash).expect("hash validated by publish_blob");
    let filename = blob_name::name(hash, expiration, extension)
        .expect("hash validated by publish_blob");
    let root = match origin {
        BlobOrigin::Upload => &state.storage.uploads,
        BlobOrigin::UpstreamCache => &state.storage.upstream_cache,
    };
    root.join(h0).join(h1).join(filename)
}

/// Create parent directories for a file path.
pub async fn create_parent_dirs(path: &Path) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to create directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

async fn remove_local_file(path: &Path) -> AppResult<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::IoError(format!(
            "Failed to remove file {}: {error}",
            path.display()
        ))),
    }
}

async fn remove_physical_file(state: &AppState, metadata: &FileMetadata) -> AppResult<()> {
    match &metadata.location {
        FileLocation::Local(path) => remove_local_file(path).await,
        FileLocation::S3 { key } => {
            state
                .native_s3
                .as_ref()
                .ok_or_else(|| {
                    AppError::ServiceUnavailable("S3 storage is not configured".to_string())
                })?
                .delete(key)
                .await
        }
    }
}

/// Publish a completed temporary file into its origin-specific final location.
///
/// The per-hash guard spans rename, index mutation, and displaced-copy cleanup,
/// so a cleanup candidate or competing completion cannot delete a new copy.
pub async fn publish_blob(
    state: &AppState,
    temp_path: &Path,
    publication: BlobPublication,
) -> AppResult<Arc<FileMetadata>> {
    validate_sha256_format(&publication.sha256)?;
    let _guard = state.blob_mutation_locks.lock(&publication.sha256).await;

    if let Some(existing) = state.file_index.get(&publication.sha256).await {
        if publication.origin == BlobOrigin::UpstreamCache || existing.origin == BlobOrigin::Upload
        {
            remove_local_file(temp_path).await?;
            return Ok(existing);
        }
    }

    if publication.origin == BlobOrigin::Upload {
        if let Some(s3) = &state.native_s3 {
            let key = s3
                .put(
                    temp_path,
                    &publication.sha256,
                    publication.extension.as_deref(),
                    publication.expiration,
                )
                .await?;
            let metadata = publication.metadata(FileLocation::S3 { key });
            return publish_metadata(state, publication.sha256, metadata).await;
        }
    }

    let final_path = completed_path(
        state,
        publication.origin,
        &publication.sha256,
        publication.extension.as_deref(),
        publication.expiration,
    );
    create_parent_dirs(&final_path).await?;

    // The index has no current entry for this hash while holding its mutation
    // guard, so a pre-existing target is an unindexed duplicate from an
    // interrupted earlier publication.
    if fs::try_exists(&final_path).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to inspect final blob path {}: {error}",
            final_path.display()
        ))
    })? {
        remove_local_file(&final_path).await?;
    }
    fs::rename(temp_path, &final_path).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to publish {} to {}: {error}",
            temp_path.display(),
            final_path.display()
        ))
    })?;

    let sha256 = publication.sha256.clone();
    let metadata = publication.metadata(FileLocation::Local(final_path));
    publish_metadata(state, sha256, metadata).await
}

/// Publish metadata for a pre-existing physical blob, such as an S3 object
/// discovered during a request. It uses the same collision and cleanup rules
/// as a temporary-file publication.
pub async fn publish_existing_metadata(
    state: &AppState,
    sha256: String,
    metadata: FileMetadata,
) -> AppResult<Arc<FileMetadata>> {
    let _guard = state.blob_mutation_locks.lock(&sha256).await;
    publish_metadata(state, sha256, metadata).await
}

async fn publish_metadata(
    state: &AppState,
    sha256: String,
    metadata: FileMetadata,
) -> AppResult<Arc<FileMetadata>> {
    match state.file_index.publish(sha256.clone(), metadata).await {
        PublishResult::Published { displaced } => {
            let published = state
                .file_index
                .get(&sha256)
                .await
                .expect("published blob must remain indexed");
            if let Some(displaced) = displaced {
                if let Err(error) = remove_physical_file(state, &displaced).await {
                    warn!(
                        sha256,
                        origin = ?displaced.origin,
                        path = ?displaced.location,
                        "Preferred blob published but failed to remove superseded copy: {error}"
                    );
                    queue_superseded_deletion(state, sha256.clone(), (*displaced).clone()).await;
                }
            }
            mark_changes_pending(state).await;
            Ok(published)
        }
        PublishResult::Retained { existing } => {
            // This should only be reachable if a caller bypassed the per-hash
            // guard. Keep the incumbent visible rather than replacing it.
            warn!(sha256, "Discarded redundant upstream cache publication");
            Ok(existing)
        }
    }
}

async fn queue_superseded_deletion(state: &AppState, sha256: String, metadata: FileMetadata) {
    let mut pending = state.superseded_blob_deletions.write().await;
    if !pending.iter().any(|(pending_hash, pending_metadata)| {
        pending_hash == &sha256 && pending_metadata.location == metadata.location
    }) {
        pending.push((sha256, metadata));
    }
}

/// Retry removal of copies displaced by a successful preferred publication.
pub async fn reconcile_superseded_blobs(state: &AppState) {
    let pending = std::mem::take(&mut *state.superseded_blob_deletions.write().await);
    let mut retry = Vec::new();
    for (sha256, metadata) in pending {
        let _guard = state.blob_mutation_locks.lock(&sha256).await;
        if let Err(error) = remove_physical_file(state, &metadata).await {
            warn!(
                sha256,
                origin = ?metadata.origin,
                path = ?metadata.location,
                "Failed to reconcile superseded blob deletion: {error}"
            );
            retry.push((sha256, metadata));
        }
    }
    state.superseded_blob_deletions.write().await.extend(retry);
}

/// Get file metadata from the shared index.
pub async fn get_file_metadata(state: &AppState, sha256: &str) -> Option<Arc<FileMetadata>> {
    state.file_index.get(sha256).await
}

/// Delete a particular indexed entry only when it has not changed since the
/// caller selected it. Used by cleanup and destructive operations.
pub async fn delete_indexed_blob(
    state: &AppState,
    sha256: &str,
    expected: &FileMetadata,
) -> AppResult<bool> {
    let _guard = state.blob_mutation_locks.lock(sha256).await;
    let Some(current) = state.file_index.get(sha256).await else {
        return Ok(false);
    };
    if current.location != expected.location
        || current.created_at != expected.created_at
        || current.origin != expected.origin
    {
        return Ok(false);
    }

    remove_physical_file(state, &current).await?;
    Ok(state
        .file_index
        .remove_if_location_matches(sha256, &current.location)
        .await)
}

/// Delete a blob selected by hash.
pub async fn delete_file(state: &AppState, sha256: &str) -> AppResult<()> {
    let metadata = state
        .file_index
        .get(sha256)
        .await
        .ok_or_else(|| AppError::NotFound(format!("File not found: {sha256}")))?;
    if delete_indexed_blob(state, sha256, &metadata).await? {
        debug!(sha256, "Deleted indexed blob");
    }
    Ok(())
}

/// Mark changes as pending for consumers that observe index mutations.
pub async fn mark_changes_pending(state: &AppState) {
    *state.changes_pending.write().await = true;
}

/// Create a temporary file path with an optional extension.
#[must_use]
pub fn create_temp_path(state: &AppState, prefix: &str, extension: Option<&str>) -> PathBuf {
    let uuid = uuid::Uuid::new_v4();
    let filename = extension.map_or_else(
        || format!("{prefix}_{uuid}"),
        |extension| format!("{prefix}_{uuid}.{extension}"),
    );
    state.storage.temp.join(filename)
}

/// Ensure the temporary root exists.
pub async fn ensure_temp_dir(state: &AppState) -> AppResult<PathBuf> {
    fs::create_dir_all(&state.storage.temp)
        .await
        .map_err(|error| {
            AppError::IoError(format!(
                "Failed to create temporary storage {}: {error}",
                state.storage.temp.display()
            ))
        })?;
    Ok(state.storage.temp.clone())
}

/// Reject a write before it can consume disk or make a successful response
/// impossible to retain. The file index tracks final blobs; filesystem free
/// space covers chunk, reconstruction, and other temporary files.
pub async fn ensure_storage_capacity(state: &AppState, bytes: u64) -> AppResult<()> {
    if bytes > state.max_blob_size_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "Blob size {bytes} exceeds configured maximum {}",
            state.max_blob_size_bytes
        )));
    }

    let stats = state.file_index.stats().await;
    if stats.total_bytes.saturating_add(bytes) > state.max_total_size
        || stats.count >= state.max_total_files
    {
        return Err(AppError::InsufficientStorage(
            "Storage quota would be exceeded".to_string(),
        ));
    }

    let available = fs2::available_space(&state.storage.root).map_err(|error| {
        AppError::IoError(format!("Failed to inspect available disk space: {error}"))
    })?;
    if available < state.min_free_disk_bytes.saturating_add(bytes) {
        return Err(AppError::InsufficientStorage(
            "Configured free-disk reserve would be violated".to_string(),
        ));
    }
    Ok(())
}

/// Validate SHA-256 hash format.
pub fn validate_sha256_format(sha256: &str) -> AppResult<()> {
    if !blob_name::is_valid_hash(sha256) {
        return Err(AppError::BadRequest(
            "SHA-256 must be exactly 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    Ok(())
}

/// Extract SHA-256 hash from filename (handles both `hash` and `hash.ext`).
#[must_use]
pub fn extract_sha256_from_filename(filename: &str) -> Option<String> {
    Some(blob_name::parse(filename)?.hash)
}
