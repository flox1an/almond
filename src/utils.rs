use regex::Regex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::{fs, sync::RwLock};
use tracing::{info, warn};
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

pub async fn enforce_storage_limits(state: &AppState) {
    let mut index = state.file_index.write().await;
    let mut files: Vec<(String, FileMetadata)> = vec![];
    let mut total_size = 0;

    // Collect all files and calculate total size
    for (key, metadata) in index.iter() {
        total_size += metadata.size;
        files.push((key.clone(), metadata.clone()));
    }

    let total_files = files.len();
    info!(
        "Storage check: {} files using {:.2} MB",
        total_files,
        total_size as f64 / 1_048_576.0
    );

    if total_files <= state.max_total_files && total_size <= state.max_total_size {
        return;
    }

    // Sort files by creation time (oldest first)
    files.sort_by_key(|(_, metadata)| metadata.created_at);

    let mut removed = 0;
    let initial_files_count = files.len();
    for (key, metadata) in files {
        if fs::remove_file(&metadata.path).await.is_ok() {
            index.remove(&key);
            total_size = total_size.saturating_sub(metadata.size);
            removed += 1;
            warn!("Deleted file: {} ({} bytes)", metadata.path.display(), metadata.size);
        }
        if initial_files_count - removed <= state.max_total_files && total_size <= state.max_total_size {
            break;
        }
    }
    info!("Deleted {} file(s) to enforce storage limits.", removed);
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
