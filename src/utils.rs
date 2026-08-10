use mime_guess::from_path;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::fs;
use tracing::{error, info, warn};

use crate::models::{AppState, FileLocation, FileMetadata};
use crate::services::blob_index::BlobIndex;

pub fn get_nested_path(
    upload_dir: &Path,
    hash: &str,
    extension: Option<&str>,
    expiration: Option<u64>,
) -> PathBuf {
    let first_level = &hash[..1];
    let second_level = &hash[1..2];
    let path = upload_dir.join(first_level).join(second_level);

    // Build filename: <hash>_<expiration>.<ext> or <hash>_<expiration> or <hash>.<ext> or <hash>
    let filename = match (expiration, extension) {
        (Some(exp), Some(ext)) => format!("{}_{}.{}", hash, exp, ext),
        (Some(exp), None) => format!("{}_{}", hash, exp),
        (None, Some(ext)) => format!("{}.{}", hash, ext),
        (None, None) => hash.to_string(),
    };

    path.join(filename)
}

pub async fn build_file_index(upload_dir: &Path, index: &BlobIndex) {
    let mut map = HashMap::new();
    let mut dirs_to_process = vec![upload_dir.to_path_buf()];

    while let Some(current_dir) = dirs_to_process.pop() {
        if let Ok(mut entries) = fs::read_dir(&current_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) {
                        // Parse filename to extract hash, expiration, and extension
                        // Format: <hash>_<expiration>.<ext> or <hash>_<expiration> or <hash>.<ext> or <hash>
                        let (key, expiration) = parse_filename_for_hash_and_expiration(&name);

                        if let Ok(metadata) = entry.metadata().await {
                            let extension = path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .map(|s| s.to_string());
                            let mime_type = from_path(&path)
                                .first()
                                .map(|m| m.essence_str().to_string());
                            let created_at = metadata
                                .created()
                                .unwrap_or(std::time::SystemTime::now())
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();

                            map.insert(
                                key,
                                FileMetadata {
                                    location: FileLocation::Local(path),
                                    extension,
                                    mime_type,
                                    size: metadata.len(),
                                    created_at,
                                    pubkey: None,
                                    expiration,
                                },
                            );
                        }
                    }
                } else if path.is_dir() {
                    dirs_to_process.push(path);
                }
            }
        }
    }

    index.replace(map).await;
}

/// Parse filename to extract SHA256 hash and optional expiration
/// Formats: <hash>_<expiration>.<ext>, <hash>_<expiration>, <hash>.<ext>, <hash>
fn parse_filename_for_hash_and_expiration(filename: &str) -> (String, Option<u64>) {
    // Remove extension if present
    let name_without_ext = filename.split('.').next().unwrap_or(filename);

    // Check if it contains underscore (expiration separator)
    if let Some(underscore_pos) = name_without_ext.find('_') {
        let hash = &name_without_ext[..underscore_pos];
        let expiration_str = &name_without_ext[underscore_pos + 1..];

        // Validate hash is 64 hex chars
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            // Try to parse expiration
            if let Ok(expiration) = expiration_str.parse::<u64>() {
                return (hash.to_string(), Some(expiration));
            }
        }
    }

    // No expiration, just extract the hash (first 64 chars)
    let hash = &name_without_ext[..64.min(name_without_ext.len())];
    (hash.to_string(), None)
}

async fn cleanup_empty_dirs(root_dir: &Path) {
    let mut dirs_to_process = vec![root_dir.to_path_buf()];
    let mut empty_dirs = vec![];

    // First pass: collect all empty directories
    while let Some(dir) = dirs_to_process.pop() {
        if let Ok(mut entries) = fs::read_dir(&dir).await {
            let mut has_entries = false;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() {
                    dirs_to_process.push(path);
                }
                has_entries = true;
            }
            if !has_entries && dir != root_dir {
                empty_dirs.push(dir);
            }
        }
    }

    // Second pass: remove empty directories and check parent directories
    let mut parent_dirs = vec![];
    for dir in empty_dirs.into_iter().rev() {
        if fs::remove_dir(&dir).await.is_ok() {
            info!("🗑 Removed empty directory: {}", dir.display());
            // Add parent directory to check if it becomes empty
            if let Some(parent) = dir.parent() {
                if parent != root_dir {
                    parent_dirs.push(parent.to_path_buf());
                }
            }
        }
    }

    // Third pass: check parent directories that might have become empty
    for parent_dir in parent_dirs {
        if let Ok(mut entries) = fs::read_dir(&parent_dir).await {
            let mut has_entries = false;
            if let Ok(Some(_)) = entries.next_entry().await {
                has_entries = true;
            }
            if !has_entries && fs::remove_dir(&parent_dir).await.is_ok() {
                info!("🗑 Removed empty parent directory: {}", parent_dir.display());
            }
        }
    }
}

pub async fn enforce_storage_limits(state: &AppState) {
    let mut total_size = 0u64;
    let mut files = state.file_index.snapshot().await;

    // Retain newest blobs; once the quota is full, evict the oldest ones.
    files.sort_by(|left, right| right.1.created_at.cmp(&left.1.created_at));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_age_secs = state.max_file_age_days * 24 * 60 * 60;

    let mut deleted_by_expiration = 0;
    let mut deleted_by_age = 0;
    let mut deleted_by_limits = 0;
    let mut retained_files = 0usize;
    let mut deletion_candidates = Vec::new();

    for (sha256, metadata) in files.into_iter() {
        // Check file expiration if expiration is set
        if let Some(expiration) = metadata.expiration {
            if now >= expiration {
                deletion_candidates.push((sha256, metadata, "expiration"));
                continue;
            }
        }

        // Check file age if max_age_days is set
        if state.max_file_age_days > 0 && now - metadata.created_at > max_age_secs {
            deletion_candidates.push((sha256, metadata, "age"));
            continue;
        }

        // Check storage limits
        if total_size.saturating_add(metadata.size) > state.max_total_size
            || retained_files >= state.max_total_files
        {
            deletion_candidates.push((sha256, metadata, "limits"));
        } else {
            total_size += metadata.size;
            retained_files += 1;
        }
    }

    let mut deleted_files = Vec::new();
    for (sha256, metadata, reason) in deletion_candidates {
        match reason {
            "expiration" => info!(
                "🗑 Deleting expired file (X-Expiration): {} (expired at {:?}, now {})",
                sha256, metadata.expiration, now
            ),
            "age" => info!("🗑 Deleting expired file (MAX_FILE_AGE_DAYS): {}", sha256),
            _ => info!("🗑 Deleting file to enforce limits: {}", sha256),
        }

        let deleted = match &metadata.location {
            FileLocation::Local(path) => match fs::remove_file(path).await {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => {
                    error!("❌ Failed to delete file {}: {}", sha256, error);
                    false
                }
            },
            FileLocation::S3 { .. } => match &state.native_s3 {
                Some(s3) => match s3.delete_matching(&sha256).await {
                    Ok(()) => true,
                    Err(error) => {
                        error!("❌ Failed to delete S3 blob {}: {}", sha256, error);
                        false
                    }
                },
                None => false,
            },
        };
        if !deleted {
            continue;
        }
        match reason {
            "expiration" => deleted_by_expiration += 1,
            "age" => deleted_by_age += 1,
            _ => deleted_by_limits += 1,
        }
        deleted_files.push((sha256, metadata.location.clone()));
    }

    for (sha256, deleted_location) in deleted_files {
        state
            .file_index
            .remove_if_location_matches(&sha256, &deleted_location)
            .await;
    }

    // Log cleanup summary
    if deleted_by_expiration > 0 || deleted_by_age > 0 || deleted_by_limits > 0 {
        info!("🧹 Cleanup summary: {} expired (X-Expiration), {} aged out (MAX_FILE_AGE_DAYS), {} by storage limits",
              deleted_by_expiration, deleted_by_age, deleted_by_limits);
    }

    // Clean up empty directories
    if deleted_by_expiration > 0 || deleted_by_age > 0 || deleted_by_limits > 0 {
        cleanup_empty_dirs(&state.upload_dir).await;
    }
}

/// Extract the SHA-256 hash from `<hash>` or `<hash>.<ext>`.
///
/// Hand-rolled rather than regex-backed: this runs on every blob request, and
/// the borrowed return keeps the hot path allocation-free.
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
    let cutoff_time = std::time::Instant::now() - timeout_duration;

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
    let chunks_dir = state.upload_dir.join("temp").join("chunks");

    if !chunks_dir.exists() {
        return;
    }

    let timeout_duration = std::time::Duration::from_secs(state.chunk_cleanup_timeout_minutes * 60);
    let cutoff_time = std::time::SystemTime::now() - timeout_duration;

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
    let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
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
    let cutoff_time = std::time::Instant::now() - cache_ttl_duration;

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
