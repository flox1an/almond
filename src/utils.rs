use mime_guess::from_path;
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::fs;
use tracing::{error, info, warn};

use crate::error::{AppError, AppResult};
use crate::models::{AppState, BlobOrigin, FileLocation, FileMetadata, StorageLayout};
use crate::services::blob_index::BlobIndex;
use crate::services::file_storage;

/// Create all explicitly managed storage roots.
pub async fn initialize_storage(layout: &StorageLayout) -> AppResult<()> {
    for directory in [
        &layout.root,
        &layout.uploads,
        &layout.upstream_cache,
        &layout.temp,
        &layout.quarantine,
    ] {
        fs::create_dir_all(directory).await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to create storage directory {}: {error}",
                directory.display()
            ))
        })?;
    }
    Ok(())
}

/// Move legacy `<hex>/<hex>/blob` trees into `uploads/` without copying blob
/// bodies. A partially completed earlier migration is resumed entry by entry.
pub async fn migrate_legacy_blobs(layout: &StorageLayout) -> AppResult<()> {
    let mut entries = fs::read_dir(&layout.root).await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to read storage root {}: {error}",
            layout.root.display()
        ))
    })?;
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        AppError::IoError(format!(
            "Failed to enumerate storage root {}: {error}",
            layout.root.display()
        ))
    })? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 1 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map_err(|error| {
                AppError::IoError(format!(
                    "Failed to inspect legacy entry {}: {error}",
                    entry.path().display()
                ))
            })?
            .is_dir()
        {
            continue;
        }
        merge_legacy_tree(&entry.path(), &layout.uploads.join(name.as_ref())).await?;
    }
    Ok(())
}

async fn merge_legacy_tree(source_root: &Path, destination_root: &Path) -> AppResult<()> {
    let mut directories = vec![(source_root.to_path_buf(), destination_root.to_path_buf())];
    while let Some((source, destination)) = directories.pop() {
        fs::create_dir_all(&destination).await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to create migration destination {}: {error}",
                destination.display()
            ))
        })?;
        let mut entries = fs::read_dir(&source).await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to read migration source {}: {error}",
                source.display()
            ))
        })?;
        while let Some(entry) = entries.next_entry().await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to enumerate migration source {}: {error}",
                source.display()
            ))
        })? {
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let file_type = entry.file_type().await.map_err(|error| {
                AppError::IoError(format!(
                    "Failed to inspect migration source {}: {error}",
                    source_path.display()
                ))
            })?;
            if file_type.is_dir() {
                directories.push((source_path, destination_path));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if fs::try_exists(&destination_path).await.map_err(|error| {
                AppError::IoError(format!(
                    "Failed to inspect migration destination {}: {error}",
                    destination_path.display()
                ))
            })? {
                let source_size = entry
                    .metadata()
                    .await
                    .map_err(|error| {
                        AppError::IoError(format!(
                            "Failed to inspect migration source {}: {error}",
                            source_path.display()
                        ))
                    })?
                    .len();
                let destination_size = fs::metadata(&destination_path)
                    .await
                    .map_err(|error| {
                        AppError::IoError(format!(
                            "Failed to inspect migration destination {}: {error}",
                            destination_path.display()
                        ))
                    })?
                    .len();
                if source_size != destination_size {
                    return Err(AppError::IoError(format!(
                        "Conflicting legacy migration files: {} and {}",
                        source_path.display(),
                        destination_path.display()
                    )));
                }
                fs::remove_file(&source_path).await.map_err(|error| {
                    AppError::IoError(format!(
                        "Failed to remove migrated duplicate {}: {error}",
                        source_path.display()
                    ))
                })?;
            } else {
                fs::rename(&source_path, &destination_path)
                    .await
                    .map_err(|error| {
                        AppError::IoError(format!(
                            "Failed to migrate {} to {}: {error}",
                            source_path.display(),
                            destination_path.display()
                        ))
                    })?;
            }
        }
    }
    Ok(())
}

/// Reconstruct the index from explicit roots. Upstream cache is scanned first,
/// then uploads overwrite same-hash cache entries deterministically.
pub async fn build_file_index(layout: &StorageLayout, index: &BlobIndex) -> AppResult<()> {
    let mut map = HashMap::new();
    scan_blob_root(&layout.upstream_cache, BlobOrigin::UpstreamCache, &mut map).await?;
    let displaced_cache = scan_blob_root(&layout.uploads, BlobOrigin::Upload, &mut map).await?;
    index.replace(map).await;

    for duplicate in displaced_cache {
        if let FileLocation::Local(path) = duplicate.location {
            if let Err(error) = fs::remove_file(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        path = %path.display(),
                        "Uploaded blob won startup collision but failed to remove cache duplicate: {error}"
                    );
                }
            }
        }
    }
    Ok(())
}

async fn scan_blob_root(
    root: &Path,
    origin: BlobOrigin,
    map: &mut HashMap<String, FileMetadata>,
) -> AppResult<Vec<FileMetadata>> {
    let mut displaced = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = fs::read_dir(&directory).await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to scan blob root {}: {error}",
                directory.display()
            ))
        })?;
        while let Some(entry) = entries.next_entry().await.map_err(|error| {
            AppError::IoError(format!(
                "Failed to enumerate blob root {}: {error}",
                directory.display()
            ))
        })? {
            let path = entry.path();
            let file_type = entry.file_type().await.map_err(|error| {
                AppError::IoError(format!(
                    "Failed to inspect blob path {}: {error}",
                    path.display()
                ))
            })?;
            if file_type.is_dir() {
                directories.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some((sha256, expiration)) = parse_filename_for_hash_and_expiration(&name) else {
                continue;
            };
            let metadata = entry.metadata().await.map_err(|error| {
                AppError::IoError(format!(
                    "Failed to read blob metadata {}: {error}",
                    path.display()
                ))
            })?;
            let modified = metadata.modified().map_err(|error| {
                AppError::IoError(format!(
                    "Failed to read modification time for {}: {error}",
                    path.display()
                ))
            })?;
            let created_at = modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_owned);
            let indexed = FileMetadata {
                location: FileLocation::Local(path.clone()),
                extension,
                mime_type: from_path(&path)
                    .first()
                    .map(|mime| mime.essence_str().to_owned()),
                size: metadata.len(),
                created_at,
                pubkey: None,
                expiration,
                origin,
            };
            if let Some(previous) = map.insert(sha256, indexed) {
                if previous.origin == BlobOrigin::UpstreamCache && origin == BlobOrigin::Upload {
                    displaced.push(previous);
                }
            }
        }
    }
    Ok(displaced)
}

/// Parse `<hash>[_<expiration>][.<extension>]` only when the hash is valid.
fn parse_filename_for_hash_and_expiration(filename: &str) -> Option<(String, Option<u64>)> {
    let stem = filename.split_once('.').map_or(filename, |(stem, _)| stem);
    let (hash, expiration) = match stem.split_once('_') {
        Some((hash, expiration)) => (hash, Some(expiration.parse::<u64>().ok()?)),
        None => (stem, None),
    };
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| (hash.to_owned(), expiration))
}

async fn cleanup_empty_dirs(root_dir: &Path) {
    let mut directories = vec![root_dir.to_path_buf()];
    let mut empty_dirs = vec![];
    while let Some(directory) = directories.pop() {
        if let Ok(mut entries) = fs::read_dir(&directory).await {
            let mut has_entries = false;
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.path().is_dir() {
                    directories.push(entry.path());
                }
                has_entries = true;
            }
            if !has_entries && directory != root_dir {
                empty_dirs.push(directory);
            }
        }
    }
    for directory in empty_dirs.into_iter().rev() {
        if fs::remove_dir(&directory).await.is_ok() {
            info!(path = %directory.display(), "Removed empty blob directory");
        }
    }
}

pub async fn enforce_storage_limits(state: &AppState) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    enforce_storage_limits_at(state, now).await;
}

/// Apply expiration and aggregate capacity cleanup at an injected time.
pub async fn enforce_storage_limits_at(state: &AppState, now: u64) {
    file_storage::reconcile_superseded_blobs(state).await;
    let entries = state.file_index.snapshot().await;
    for (sha256, metadata) in entries {
        if let Some(reason) = expiration_reason(state, &metadata, now) {
            delete_cleanup_candidate(state, &sha256, &metadata, reason).await;
        }
    }

    let remaining = state.file_index.snapshot().await;
    let mut total_size = remaining
        .iter()
        .fold(0u64, |sum, (_, metadata)| sum.saturating_add(metadata.size));
    let mut total_files = remaining.len();

    for (sha256, metadata) in capacity_eviction_order(&remaining) {
        if total_size <= state.max_total_size && total_files <= state.max_total_files {
            break;
        }
        if delete_cleanup_candidate(state, &sha256, &metadata, "capacity").await {
            total_size = total_size.saturating_sub(metadata.size);
            total_files = total_files.saturating_sub(1);
        }
    }

    cleanup_empty_dirs(&state.storage.uploads).await;
    cleanup_empty_dirs(&state.storage.upstream_cache).await;
}

fn capacity_eviction_order(
    entries: &[(String, Arc<FileMetadata>)],
) -> Vec<(String, Arc<FileMetadata>)> {
    let mut ordered = Vec::with_capacity(entries.len());
    for origin in [BlobOrigin::UpstreamCache, BlobOrigin::Upload] {
        let mut by_origin = entries
            .iter()
            .filter(|(_, metadata)| metadata.origin == origin)
            .cloned()
            .collect::<Vec<_>>();
        by_origin.sort_by_key(|(_, metadata)| metadata.created_at);
        ordered.extend(by_origin);
    }
    ordered
}

fn expiration_reason(state: &AppState, metadata: &FileMetadata, now: u64) -> Option<&'static str> {
    expiration_reason_for(
        metadata,
        state.max_file_age_days,
        state.max_upstream_cache_ttl_days,
        now,
    )
}

fn expiration_reason_for(
    metadata: &FileMetadata,
    max_file_age_days: u64,
    max_upstream_cache_ttl_days: u64,
    now: u64,
) -> Option<&'static str> {
    match metadata.origin {
        BlobOrigin::Upload => {
            if metadata
                .expiration
                .is_some_and(|expiration| now >= expiration)
            {
                return Some("expiration");
            }
            (max_file_age_days > 0
                && now
                    >= metadata
                        .created_at
                        .saturating_add(max_file_age_days.saturating_mul(86_400)))
            .then_some("upload_age")
        }
        BlobOrigin::UpstreamCache => (max_upstream_cache_ttl_days > 0
            && now
                >= metadata
                    .created_at
                    .saturating_add(max_upstream_cache_ttl_days.saturating_mul(86_400)))
        .then_some("upstream_cache_ttl"),
    }
}

async fn delete_cleanup_candidate(
    state: &AppState,
    sha256: &str,
    metadata: &FileMetadata,
    reason: &str,
) -> bool {
    match file_storage::delete_indexed_blob(state, sha256, metadata).await {
        Ok(true) => {
            info!(
                sha256,
                origin = ?metadata.origin,
                reason,
                path = ?metadata.location,
                "Deleted blob during storage cleanup"
            );
            true
        }
        Ok(false) => false,
        Err(error) => {
            error!(
                sha256,
                origin = ?metadata.origin,
                reason,
                path = ?metadata.location,
                "Failed to delete blob during storage cleanup: {error}"
            );
            false
        }
    }
}

/// Extract the SHA-256 hash from `<hash>` or `<hash>.<ext>`.
///
/// Hand-rolled rather than regex-backed: this runs on every blob request, and
/// the borrowed return keeps the hot path allocation-free.
#[must_use]
pub fn get_sha256_hash_from_filename(filename: &str) -> Option<&str> {
    let (hash, extension) = match filename.split_once('.') {
        Some((hash, extension)) => (hash, Some(extension)),
        None => (filename, None),
    };

    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if let Some(extension) = extension {
        if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
    }

    Some(hash)
}

pub async fn find_file(index: &BlobIndex, base_name: &str) -> Option<Arc<FileMetadata>> {
    index.get(base_name).await
}

/// Outcome of parsing a `Range` request header.
#[derive(Debug, PartialEq, Eq)]
pub enum RangeSpec {
    /// Inclusive byte range to serve as `206 Partial Content`.
    Satisfiable { start: u64, end: u64 },
    /// Well-formed range that lies entirely outside the resource -> `416`.
    Unsatisfiable,
    /// Nothing usable: malformed, or a multi-range request we do not build a
    /// `multipart/byteranges` body for. RFC 9110 §14.2 permits ignoring the
    /// header and serving the full `200` representation.
    Ignore,
}

/// Parse a single-range `Range: bytes=...` header.
///
/// Handles all three RFC 9110 §14.1.1 forms: `bytes=START-END`,
/// `bytes=START-` (open ended) and `bytes=-SUFFIX` (final N bytes). The suffix
/// form matters in practice: MP4 players probe the trailing `moov` atom with
/// it, and treating it as unparseable means shipping the entire blob instead.
#[must_use]
pub fn parse_range_header(header_value: &str, total_size: u64) -> RangeSpec {
    let Some(spec) = header_value.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    let spec = spec.trim();

    if spec.contains(',') {
        return RangeSpec::Ignore;
    }

    let Some((first, last)) = spec.split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (first, last) = (first.trim(), last.trim());

    // A zero-length resource cannot satisfy any range.
    if total_size == 0 {
        return RangeSpec::Unsatisfiable;
    }

    let (start, end) = if first.is_empty() {
        // Suffix form: `bytes=-N` selects the final N bytes.
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        if suffix == 0 {
            return RangeSpec::Unsatisfiable;
        }
        (total_size.saturating_sub(suffix), total_size - 1)
    } else {
        let Ok(start) = first.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        if start >= total_size {
            return RangeSpec::Unsatisfiable;
        }
        let end = if last.is_empty() {
            total_size - 1
        } else {
            match last.parse::<u64>() {
                // An end past EOF is clamped, not rejected (RFC 9110 §14.1.1).
                Ok(end) => end.min(total_size - 1),
                Err(_) => return RangeSpec::Ignore,
            }
        };
        if start > end {
            return RangeSpec::Unsatisfiable;
        }
        (start, end)
    };

    RangeSpec::Satisfiable { start, end }
}

/// Clean up abandoned chunked uploads and their associated files
pub async fn cleanup_abandoned_chunks(state: &AppState) {
    let timeout_duration = std::time::Duration::from_secs(state.chunk_cleanup_timeout_minutes * 60);
    let cutoff_time = std::time::Instant::now()
        .checked_sub(timeout_duration)
        .unwrap();

    // Get chunk uploads that are older than the timeout
    let mut chunk_uploads = state.chunk_uploads.write().await;
    let mut to_remove = Vec::new();

    for (key, chunk_upload) in chunk_uploads.iter() {
        if chunk_upload.created_at < cutoff_time {
            info!("Cleaning up abandoned chunked upload: {}", key.sha256);
            to_remove.push(key.clone());
        }
    }

    // Remove abandoned uploads and clean up their files
    for key in to_remove {
        if let Some(chunk_upload) = chunk_uploads.remove(&key) {
            let chunk_count = chunk_upload.chunks.len();
            // Clean up all chunk files for this upload
            for chunk in chunk_upload.chunks {
                if let Err(e) = fs::remove_file(&chunk.chunk_path).await {
                    warn!(
                        "Failed to clean up chunk file {}: {}",
                        chunk.chunk_path.display(),
                        e
                    );
                }
            }
            info!(
                "🗑 Cleaned up {} chunk files for abandoned upload: {}",
                chunk_count, key.sha256
            );
        }
    }

    // Also clean up orphaned chunk files in the temp/chunks directory
    cleanup_orphaned_chunk_files(state).await;
}

/// Clean up orphaned chunk files that don't belong to any active upload
async fn cleanup_orphaned_chunk_files(state: &AppState) {
    let chunks_dir = state.storage.temp.join("chunks");

    if !chunks_dir.exists() {
        return;
    }

    let timeout_duration = std::time::Duration::from_secs(state.chunk_cleanup_timeout_minutes * 60);
    let cutoff_time = SystemTime::now() - timeout_duration;

    let mut entries = match fs::read_dir(&chunks_dir).await {
        Ok(entries) => entries,
        Err(e) => {
            error!("❌ Failed to read chunks directory: {}", e);
            return;
        }
    };

    let mut cleaned_count = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_file() {
            // Check if the file is older than the timeout
            if let Ok(metadata) = entry.metadata().await {
                if let Ok(modified) = metadata.modified() {
                    if modified < cutoff_time {
                        if let Err(e) = fs::remove_file(&path).await {
                            warn!(
                                "❌ Failed to clean up orphaned chunk file {}: {}",
                                path.display(),
                                e
                            );
                        } else {
                            cleaned_count += 1;
                        }
                    }
                }
            }
        }
    }

    if cleaned_count > 0 {
        info!("Cleaned up {} orphaned chunk files", cleaned_count);
    }
}

/// Clean up expired failed upstream lookups (older than 1 hour)
pub async fn cleanup_expired_failed_lookups(state: &AppState) {
    let one_hour_ago = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(3600))
        .unwrap();
    let mut failed_lookups = state.failed_upstream_lookups.write().await;
    let initial_count = failed_lookups.len();

    failed_lookups.retain(|_, &mut timestamp| timestamp > one_hour_ago);

    let cleaned_count = initial_count - failed_lookups.len();
    if cleaned_count > 0 {
        info!(
            "Cleaned up {} expired failed upstream lookups",
            cleaned_count
        );
    }
}

/// Clean up expired blossom server list cache entries
pub async fn cleanup_expired_blossom_server_lists(state: &AppState) {
    let cache_ttl_duration =
        std::time::Duration::from_secs(state.blossom_server_list_cache_ttl_hours * 3600);
    let cutoff_time = std::time::Instant::now()
        .checked_sub(cache_ttl_duration)
        .unwrap();

    let mut cache = state.blossom_server_lists.write().await;
    let initial_count = cache.len();

    cache.retain(|_, (_, cached_at)| *cached_at > cutoff_time);

    let cleaned_count = initial_count - cache.len();
    if cleaned_count > 0 {
        info!(
            "Cleaned up {} expired blossom server list cache entries",
            cleaned_count
        );
    }
}

#[cfg(test)]
mod range_tests {
    use super::{parse_range_header as parse, RangeSpec};

    fn sat(start: u64, end: u64) -> RangeSpec {
        RangeSpec::Satisfiable { start, end }
    }

    #[test]
    fn parses_closed_range() {
        assert_eq!(parse("bytes=0-99", 1000), sat(0, 99));
        assert_eq!(parse("bytes=500-999", 1000), sat(500, 999));
    }

    #[test]
    fn parses_open_ended_range() {
        assert_eq!(parse("bytes=500-", 1000), sat(500, 999));
        assert_eq!(parse("bytes=0-", 1), sat(0, 0));
    }

    /// Regression: the suffix form used to fail to parse and silently fall back
    /// to a full-body 200 response.
    #[test]
    fn parses_suffix_range() {
        assert_eq!(parse("bytes=-500", 1000), sat(500, 999));
        // A suffix longer than the resource clamps to the whole resource.
        assert_eq!(parse("bytes=-5000", 1000), sat(0, 999));
    }

    #[test]
    fn clamps_end_past_eof() {
        assert_eq!(parse("bytes=900-5000", 1000), sat(900, 999));
    }

    #[test]
    fn rejects_ranges_outside_resource() {
        assert_eq!(parse("bytes=1000-1100", 1000), RangeSpec::Unsatisfiable);
        assert_eq!(parse("bytes=-0", 1000), RangeSpec::Unsatisfiable);
        assert_eq!(parse("bytes=0-0", 0), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn ignores_unusable_headers() {
        assert_eq!(parse("items=0-99", 1000), RangeSpec::Ignore);
        assert_eq!(parse("bytes=abc-def", 1000), RangeSpec::Ignore);
        assert_eq!(parse("bytes=0-9,20-29", 1000), RangeSpec::Ignore);
        assert_eq!(parse("bytes=", 1000), RangeSpec::Ignore);
    }

    #[test]
    fn inverted_range_is_unsatisfiable() {
        assert_eq!(parse("bytes=800-100", 1000), RangeSpec::Unsatisfiable);
    }
}

#[cfg(test)]
mod storage_tests {
    use super::{
        build_file_index, capacity_eviction_order, expiration_reason_for, initialize_storage,
        migrate_legacy_blobs,
    };
    use crate::models::{BlobOrigin, FileLocation, FileMetadata, StorageLayout};
    use crate::services::blob_index::BlobIndex;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn hash(character: char) -> String {
        character.to_string().repeat(64)
    }

    fn metadata(origin: BlobOrigin, created_at: u64, expiration: Option<u64>) -> FileMetadata {
        FileMetadata {
            location: FileLocation::Local(std::path::PathBuf::from("/tmp/blob")),
            extension: None,
            mime_type: None,
            size: 1,
            created_at,
            pubkey: None,
            expiration,
            origin,
        }
    }

    async fn write_blob(root: &std::path::Path, sha256: &str, body: &[u8]) {
        let path = root.join(&sha256[..1]).join(&sha256[1..2]).join(sha256);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(path, body).await.unwrap();
    }

    #[test]
    fn expiration_policies_remain_origin_specific() {
        let now = 86_400;
        let upload = metadata(BlobOrigin::Upload, 0, None);
        let cache = metadata(BlobOrigin::UpstreamCache, 0, None);

        assert_eq!(expiration_reason_for(&upload, 0, 1, now), None);
        assert_eq!(
            expiration_reason_for(&cache, 0, 1, now),
            Some("upstream_cache_ttl")
        );
        assert_eq!(
            expiration_reason_for(&upload, 1, 0, now),
            Some("upload_age")
        );
        assert_eq!(
            expiration_reason_for(&metadata(BlobOrigin::Upload, now, Some(now)), 1, 1, now,),
            Some("expiration")
        );
    }

    #[test]
    fn capacity_eviction_prefers_oldest_cache_entries() {
        let entries = vec![
            (
                "upload-old".to_string(),
                Arc::new(metadata(BlobOrigin::Upload, 1, None)),
            ),
            (
                "cache-new".to_string(),
                Arc::new(metadata(BlobOrigin::UpstreamCache, 3, None)),
            ),
            (
                "cache-old".to_string(),
                Arc::new(metadata(BlobOrigin::UpstreamCache, 2, None)),
            ),
            (
                "upload-new".to_string(),
                Arc::new(metadata(BlobOrigin::Upload, 4, None)),
            ),
        ];
        let ordered = capacity_eviction_order(&entries)
            .into_iter()
            .map(|(sha256, _)| sha256)
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            vec!["cache-old", "cache-new", "upload-old", "upload-new"]
        );
    }

    #[tokio::test]
    async fn migration_and_reconstruction_preserve_origin_precedence() {
        let root = std::env::temp_dir().join(format!("almond-storage-{}", uuid::Uuid::new_v4()));
        let layout = StorageLayout::new(root.clone());
        initialize_storage(&layout).await.unwrap();

        let duplicate = hash('a');
        let cache_only = hash('b');
        write_blob(&root, &duplicate, b"legacy-upload").await;
        write_blob(&layout.upstream_cache, &duplicate, b"cache-duplicate").await;
        let legacy_path = root
            .join(&duplicate[..1])
            .join(&duplicate[1..2])
            .join(&duplicate);
        std::fs::File::open(legacy_path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(123)),
            )
            .unwrap();
        write_blob(&layout.upstream_cache, &cache_only, b"cache-only").await;
        tokio::fs::write(layout.temp.join(&duplicate), b"ignored")
            .await
            .unwrap();
        tokio::fs::write(layout.quarantine.join(&duplicate), b"ignored")
            .await
            .unwrap();

        migrate_legacy_blobs(&layout).await.unwrap();
        let index = BlobIndex::new();
        build_file_index(&layout, &index).await.unwrap();

        let uploaded = index.get(&duplicate).await.unwrap();
        assert_eq!(uploaded.origin, BlobOrigin::Upload);
        assert!(matches!(
            &uploaded.location,
            FileLocation::Local(path) if path.starts_with(&layout.uploads)
        ));
        assert!(!layout
            .upstream_cache
            .join("a")
            .join("a")
            .join(&duplicate)
            .exists());

        let cached = index.get(&cache_only).await.unwrap();
        assert_eq!(cached.origin, BlobOrigin::UpstreamCache);
        assert_eq!(index.stats().await.count, 2);
        assert_eq!(uploaded.created_at, 123);

        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
