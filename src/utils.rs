use regex::Regex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{fs, sync::RwLock};
use tracing::{info, error};
use mime_guess::from_path;

use crate::models::{AppState, FileMetadata};

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

pub async fn build_file_index(upload_dir: &Path, index: &RwLock<HashMap<String, FileMetadata>>) {
    let mut map = HashMap::new();
    let mut dirs_to_process = vec![upload_dir.to_path_buf()];
    
    while let Some(current_dir) = dirs_to_process.pop() {
        if let Ok(mut entries) = fs::read_dir(&current_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) {
                        let key = name[..64.min(name.len())].to_string();
                        if let Ok(metadata) = entry.metadata().await {
                            let extension = path.extension()
                                .and_then(|ext| ext.to_str())
                                .map(|s| s.to_string());
                            let mime_type = from_path(&path)
                                .first()
                                .map(|m| m.essence_str().to_string());
                            let created_at = metadata.created()
                                .unwrap_or(std::time::SystemTime::now())
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            
                            map.insert(key, FileMetadata {
                                path,
                                extension,
                                mime_type,
                                size: metadata.len(),
                                created_at,
                            });
                        }
                    }
                } else if path.is_dir() {
                    dirs_to_process.push(path);
                }
            }
        }
    }

    *index.write().await = map;
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
            if !has_entries {
                empty_dirs.push(dir);
            }
        }
    }

    // Second pass: remove empty directories and check parent directories
    let mut parent_dirs = vec![];
    for dir in empty_dirs.into_iter().rev() {
        if fs::remove_dir(&dir).await.is_ok() {
            info!("Removed empty directory: {}", dir.display());
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
            while let Ok(Some(_)) = entries.next_entry().await {
                has_entries = true;
                break;
            }
            if !has_entries {
                if fs::remove_dir(&parent_dir).await.is_ok() {
                    info!("Removed empty parent directory: {}", parent_dir.display());
                }
            }
        }
    }
}

pub async fn enforce_storage_limits(state: &AppState) {
    let mut index = state.file_index.write().await;
    let mut total_size = 0;
    let mut files: Vec<(String, FileMetadata)> = index.iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Sort files by creation date (oldest first)
    files.sort_by(|a, b| a.1.created_at.cmp(&b.1.created_at));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_age_secs = state.max_file_age_days * 24 * 60 * 60;

    for (sha256, metadata) in files {
        // Check file age if max_age_days is set
        if state.max_file_age_days > 0 && now - metadata.created_at > max_age_secs {
            info!("Deleting expired file: {}", sha256);
            if let Err(e) = fs::remove_file(&metadata.path).await {
                error!("Failed to delete expired file {}: {}", sha256, e);
            }
            index.remove(&sha256);
            continue;
        }

        // Check storage limits
        if total_size + metadata.size > state.max_total_size || index.len() >= state.max_total_files {
            info!("Deleting file to enforce limits: {}", sha256);
            if let Err(e) = fs::remove_file(&metadata.path).await {
                error!("Failed to delete file {}: {}", sha256, e);
            }
            index.remove(&sha256);
        } else {
            total_size += metadata.size;
        }
    }

    // Clean up empty directories
    cleanup_empty_dirs(&state.upload_dir).await;
}

pub fn get_sha256_hash_from_filename(filename: &str) -> Option<String> {
    let re = Regex::new(r"^([a-fA-F0-9]{64})(\.[a-zA-Z0-9]+)?$").unwrap();
    if let Some(captures) = re.captures(filename) {
        Some(captures[1].to_string()) // Return the first capture group (the hash)
    } else {
        None
    }
}

pub async fn find_file(
    index: &RwLock<HashMap<String, FileMetadata>>,
    base_name: &str,
) -> Option<FileMetadata> {
    let index = index.read().await;
    index.get(base_name).cloned()
}

pub fn parse_range_header(header_value: &str, total_size: u64) -> Option<(u64, u64)> {
    if !header_value.starts_with("bytes=") {
        return None;
    }
    let range = &header_value[6..];
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start = parts[0].parse::<u64>().ok()?;
    let end = if parts[1].is_empty() {
        total_size - 1
    } else {
        parts[1].parse::<u64>().ok()?
    };
    if start > end || end >= total_size {
        return None;
    }
    Some((start, end))
}
