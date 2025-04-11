use regex::Regex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::{fs, sync::RwLock};
use tracing::{info, warn};

use crate::models::AppState;

pub async fn build_file_index(upload_dir: &Path, index: &RwLock<HashMap<String, PathBuf>>) {
    let mut map = HashMap::new();
    if let Ok(mut entries) = fs::read_dir(upload_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) {
                    let key = name[..64.min(name.len())].to_string();
                    map.insert(key, path);
                }
            }
        }
    }
    *index.write().await = map;
}

pub async fn enforce_storage_limits(state: &AppState) {
    let mut entries = match fs::read_dir(&state.upload_dir).await {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = vec![];
    let mut total_size = 0;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if let Ok(metadata) = entry.metadata().await {
            if metadata.is_file() {
                let size = metadata.len();
                let created = metadata.created().unwrap_or(std::time::SystemTime::now());
                total_size += size;
                files.push((path, size, created));
            }
        }
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

    files.sort_by_key(|f| f.2);

    let mut removed = 0;
    for (path, size, _) in &files {
        if fs::remove_file(path).await.is_ok() {
            let key = path
                .file_name()
                .and_then(|f| f.to_str())
                .map(|s| s[..64.min(s.len())].to_string());
            if let Some(k) = key {
                state.file_index.write().await.remove(&k);
            }
            total_size = total_size.saturating_sub(*size);
            removed += 1;
            warn!("Deleted file: {} ({} bytes)", path.display(), size);
        }
        if files.len() - removed <= state.max_total_files && total_size <= state.max_total_size {
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
    index: &RwLock<HashMap<String, PathBuf>>,
    base_name: &str,
) -> Option<PathBuf> {
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
