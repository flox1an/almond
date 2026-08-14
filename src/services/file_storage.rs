use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs;
use tracing::{info, warn};

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

/// The authoritative result of one storage publication.
///
/// A content-addressed blob may already exist. In that case `metadata`
/// describes the incumbent bytes and `created` is false; callers must not
/// report request metadata that storage deliberately retained.
pub struct StoredBlob {
    pub metadata: Arc<FileMetadata>,
    pub created: bool,
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
    let filename =
        blob_name::name(hash, expiration, extension).expect("hash validated by publish_blob");
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

/// Reject a hash previously quarantined by report moderation.
///
/// Quarantine keeps the original blob filename, which may carry a retention
/// suffix or an extension; a hash prefix followed by `_`, `.`, or end-of-name
/// is therefore the only accepted match.
pub async fn ensure_not_quarantined(state: &AppState, sha256: &str) -> AppResult<()> {
    validate_sha256_format(sha256)?;
    let mut entries = match fs::read_dir(&state.storage.quarantine).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::IoError(format!(
                "Failed to inspect quarantine directory {}: {error}",
                state.storage.quarantine.display()
            )));
        }
    };

    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to read quarantine directory {}: {error}",
            state.storage.quarantine.display()
        ))
    })? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if quarantine_name_matches(sha256, name) {
            return Err(AppError::Forbidden(
                "Blob is quarantined and cannot be uploaded".to_owned(),
            ));
        }
    }
    Ok(())
}

fn quarantine_name_matches(sha256: &str, name: &str) -> bool {
    name.strip_prefix(sha256).is_some_and(|suffix| {
        suffix.is_empty() || suffix.starts_with('_') || suffix.starts_with('.')
    })
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
    temp: TempBlob,
    publication: BlobPublication,
) -> AppResult<StoredBlob> {
    let temp_path = temp.path().to_path_buf();
    validate_sha256_format(&publication.sha256)?;
    let _guard = state.blob_mutation_locks.lock(&publication.sha256).await;

    ensure_not_quarantined(state, &publication.sha256).await?;

    if let Some(existing) = state.file_index.get(&publication.sha256).await {
        if publication.origin == BlobOrigin::UpstreamCache || existing.origin == BlobOrigin::Upload
        {
            // `temp` unlinks the redundant copy as it drops out of scope.
            return Ok(StoredBlob {
                metadata: existing,
                created: false,
            });
        }
    }

    if publication.origin == BlobOrigin::Upload {
        if let Some(s3) = &state.native_s3 {
            let key = s3
                .put(
                    &temp_path,
                    &publication.sha256,
                    publication.extension.as_deref(),
                    publication.expiration,
                )
                .await?;
            // `put` consumes the temporary file on success.
            temp.into_published();
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
    fs::rename(&temp_path, &final_path).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to publish {} to {}: {error}",
            temp_path.display(),
            final_path.display()
        ))
    })?;
    // The bytes now live at their final path; nothing is left to unlink.
    temp.into_published();

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
    Ok(publish_metadata(state, sha256, metadata).await?.metadata)
}

async fn publish_metadata(
    state: &AppState,
    sha256: String,
    metadata: FileMetadata,
) -> AppResult<StoredBlob> {
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
            Ok(StoredBlob {
                metadata: published,
                created: true,
            })
        }
        PublishResult::Retained { existing } => {
            // This should only be reachable if a caller bypassed the per-hash
            // guard. Keep the incumbent visible rather than replacing it.
            warn!(sha256, "Discarded redundant upstream cache publication");
            Ok(StoredBlob {
                metadata: existing,
                created: false,
            })
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

/// Read a stored blob as UTF-8 text, whichever adapter holds it.
///
/// Callers outside this module used to match on `FileLocation` themselves and
/// separately re-check whether a native backend was configured. Asking storage
/// keeps the two adapters behind one interface.
pub async fn read_text(state: &AppState, metadata: &FileMetadata) -> AppResult<String> {
    match &metadata.location {
        FileLocation::Local(path) => fs::read_to_string(path).await.map_err(|error| {
            AppError::IoError(format!("Failed to read {}: {error}", path.display()))
        }),
        FileLocation::S3 { key } => {
            state
                .native_s3
                .as_ref()
                .ok_or_else(|| {
                    AppError::ServiceUnavailable("S3 storage is not configured".to_string())
                })?
                .read_text(key)
                .await
        }
    }
}

/// The on-disk path of a blob, when the filesystem adapter is the one holding
/// it.
///
/// A blob in the native backend has no local path to hand out, so `None` means
/// "this adapter cannot expose a path", not "the blob is missing".
#[must_use]
pub fn local_path(metadata: &FileMetadata) -> Option<&Path> {
    match &metadata.location {
        FileLocation::Local(path) => Some(path),
        FileLocation::S3 { .. } => None,
    }
}

/// Get file metadata from the shared index.
pub async fn get_file_metadata(state: &AppState, sha256: &str) -> Option<Arc<FileMetadata>> {
    state.file_index.get(sha256).await
}

/// Why a blob is leaving the served set.
///
/// Each reason fixes the two things the call sites used to decide for
/// themselves: what happens to the stored bytes, and how thoroughly the
/// native backend is swept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// BUD-02 `DELETE /<hash>`: drop the indexed copy and every sibling object
    /// stored under the same hash.
    Requested,
    /// BUD-09 report resolved as a deletion.
    Reported,
    /// BUD-09 report resolved as a quarantine: local bytes move out of the
    /// served tree into the quarantine root and are kept.
    Quarantined,
    /// Retention sweep: expiry or capacity pressure.
    Evicted,
}

impl Removal {
    /// A requested or reported removal means "make this hash go away", so it
    /// also drops backend objects the index does not point at. An eviction
    /// only reclaims the copy it selected.
    const fn sweeps_backend(self) -> bool {
        matches!(self, Self::Requested | Self::Reported)
    }
}

/// Remove one indexed blob under the per-hash mutation guard.
///
/// This is the only way a blob leaves the index. The guard spans the metadata
/// read, the physical disposal and the index mutation, so a concurrent
/// `publish_blob` cannot have its fresh copy deleted in between.
///
/// `expected` pins the entry the caller selected: the removal is skipped when
/// the index has moved on, so a stale retention candidate cannot delete a
/// republished hash. Callers acting on a hash alone pass `None`.
///
/// Removal is origin-agnostic by design. See
/// `docs/plans/2026-07-25-storage-origin-split-design.md`, "Existing behavior
/// preserved": delete and report act on the currently indexed copy regardless
/// of origin.
///
/// Returns whether an indexed entry was removed. An unindexed hash is not an
/// error — absence is already the caller's desired end state.
pub async fn remove_indexed_blob(
    state: &AppState,
    sha256: &str,
    removal: Removal,
    expected: Option<&FileMetadata>,
) -> AppResult<bool> {
    let _guard = state.blob_mutation_locks.lock(sha256).await;

    let Some(current) = state.file_index.get(sha256).await else {
        // Nothing indexed, but a requested or reported removal still sweeps the
        // backend so a hash that lost its index entry cannot linger in it.
        if removal.sweeps_backend() {
            sweep_backend_copies(state, sha256).await?;
        }
        return Ok(false);
    };

    if let Some(expected) = expected {
        if current.location != expected.location
            || current.created_at != expected.created_at
            || current.origin != expected.origin
        {
            return Ok(false);
        }
    }

    if removal == Removal::Quarantined {
        quarantine_current(state, sha256, &current).await?;
    } else {
        remove_physical_file(state, &current).await?;
    }

    if removal.sweeps_backend() {
        sweep_backend_copies(state, sha256).await?;
    }

    let removed = state
        .file_index
        .remove_if_location_matches(sha256, &current.location)
        .await;
    if removed {
        mark_changes_pending(state).await;
    }
    Ok(removed)
}

/// Move a local blob into the quarantine root, keeping its bytes.
///
/// A blob held in the native backend cannot be moved into a local directory,
/// so quarantining one removes it instead — the behaviour the report handler
/// had before this path existed.
async fn quarantine_current(
    state: &AppState,
    sha256: &str,
    metadata: &FileMetadata,
) -> AppResult<()> {
    let FileLocation::Local(path) = &metadata.location else {
        return sweep_backend_copies(state, sha256).await;
    };

    let quarantine = &state.storage.quarantine;
    fs::create_dir_all(quarantine).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to create quarantine directory {}: {error}",
            quarantine.display()
        ))
    })?;

    let name = path.file_name().map_or_else(
        || sha256.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let destination = quarantine.join(name);
    fs::rename(path, &destination).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to quarantine {} into {}: {error}",
            path.display(),
            destination.display()
        ))
    })?;

    info!(sha256, quarantine = ?destination, "Quarantined blob");
    Ok(())
}

/// Drop every native-backend object stored under `sha256`, indexed or not.
async fn sweep_backend_copies(state: &AppState, sha256: &str) -> AppResult<()> {
    match &state.native_s3 {
        Some(s3) => s3.delete_matching(sha256).await,
        None => Ok(()),
    }
}

/// Mark changes as pending for consumers that observe index mutations.
pub async fn mark_changes_pending(state: &AppState) {
    *state.changes_pending.write().await = true;
}

/// A temporary file that owns its own disposal.
///
/// A temp path used to be a bare `PathBuf` handed across every seam, so the
/// rule for removing it was re-decided at each site — three `Drop` guards in
/// one handler, nine hand-written unlinks elsewhere, and a `publish_blob` that
/// deleted its caller's file on one branch but not the others. Owning the path
/// states the rule once: whoever holds a `TempBlob` and lets it drop has
/// removed the file, and the only way to keep the bytes is to publish them.
///
/// Disposal is a synchronous unlink because `Drop` cannot await. An unlink is
/// a single fast syscall, and the alternative — leaking on every error path —
/// is what this type exists to prevent.
pub struct TempBlob {
    /// `None` once the bytes have been moved somewhere permanent.
    path: Option<PathBuf>,
}

impl TempBlob {
    /// Reserve a temporary path under the storage temp root.
    ///
    /// The file itself is created by whoever streams into it; reserving only
    /// fixes the name, so an intake that fails before writing still drops a
    /// `TempBlob` that finds nothing to unlink.
    #[must_use]
    pub fn reserve(state: &AppState, prefix: &str, extension: Option<&str>) -> Self {
        let uuid = uuid::Uuid::new_v4();
        let filename = extension.map_or_else(
            || format!("{prefix}_{uuid}"),
            |extension| format!("{prefix}_{uuid}.{extension}"),
        );
        Self {
            path: Some(state.storage.temp.join(filename)),
        }
    }

    /// Take ownership of a file some other module created.
    ///
    /// Used by the upstream download path, where the coalescing machinery owns
    /// the file while followers read it and only hands it over at publication.
    #[must_use]
    pub fn adopt(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Where to write the bytes.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("a TempBlob is only pathless after it has been published")
    }

    /// Give up ownership: the bytes moved somewhere permanent, so dropping
    /// this value must not unlink them.
    fn into_published(mut self) {
        self.path = None;
    }
}

impl Drop for TempBlob {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warn!(
                path = %path.display(),
                "Failed to remove temporary blob: {error}"
            ),
        }
    }
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

    let available = fs4::available_space(&state.storage.root).map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("almond_temp_blob_{label}_{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn quarantine_match_requires_a_complete_hash_prefix() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(quarantine_name_matches(hash, hash));
        assert!(quarantine_name_matches(
            hash,
            &format!("{hash}_1234.tar.gz")
        ));
        assert!(!quarantine_name_matches(hash, &format!("{hash}suffix")));
    }

    fn written_scratch(label: &str) -> PathBuf {
        let path = scratch_path(label);
        std::fs::write(&path, b"blob bytes").expect("scratch file is writable");
        path
    }

    #[test]
    fn dropping_a_temp_blob_unlinks_its_file() {
        // Every intake error path depends on this: a partial blob must not
        // survive the failure that abandoned it.
        let path = written_scratch("dropped");
        drop(TempBlob::adopt(path.clone()));
        assert!(
            !path.exists(),
            "a dropped TempBlob must leave nothing behind"
        );
    }

    #[test]
    fn a_published_temp_blob_keeps_its_bytes() {
        // Publication renames the file into place, so the disposal must not
        // then unlink the blob that was just published.
        let path = written_scratch("published");
        TempBlob::adopt(path.clone()).into_published();
        assert!(path.exists(), "published bytes must survive disposal");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn dropping_a_reserved_but_unwritten_temp_blob_is_harmless() {
        // Reserving only fixes a name. An intake that fails before writing a
        // single byte still drops a TempBlob, and that must not be an error.
        let path = scratch_path("never_written");
        drop(TempBlob::adopt(path.clone()));
        assert!(!path.exists());
    }
}
