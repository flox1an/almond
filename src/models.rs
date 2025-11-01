use mime_guess;
use nostr_relay_pool::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, RwLock};

#[derive(Clone)]
pub struct FileMetadata {
    pub path: PathBuf,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    pub size: u64,
    pub created_at: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub upload_dir: PathBuf,
    pub file_index: Arc<RwLock<HashMap<String, FileMetadata>>>,
    pub max_total_size: u64,
    pub max_total_files: usize,
    pub bind_addr: String,
    pub public_url: String,
    pub cleanup_interval_secs: u64,
    pub changes_pending: Arc<RwLock<bool>>,
    pub allowed_pubkeys: Vec<PublicKey>,
    pub trusted_pubkeys: Arc<RwLock<HashMap<PublicKey, usize>>>,
    pub max_file_age_days: u64,
    pub files_uploaded: Arc<RwLock<u64>>,
    pub files_downloaded: Arc<RwLock<u64>>,
    pub upload_throughput_data: Arc<RwLock<Vec<(Instant, u64)>>>,
    pub download_throughput_data: Arc<RwLock<Vec<(Instant, u64)>>>,
    pub upstream_servers: Vec<String>,
    pub max_upstream_download_size_mb: u64,
    pub max_chunk_size_mb: u64,
    pub chunk_cleanup_timeout_minutes: u64,
    pub feature_upload_enabled: bool,
    pub feature_mirror_enabled: bool,
    pub feature_list_enabled: bool,
    pub ongoing_downloads:
        Arc<RwLock<HashMap<String, (Instant, Arc<AtomicU64>, Arc<Notify>, PathBuf, String)>>>,
    pub chunk_uploads: Arc<RwLock<HashMap<String, ChunkUpload>>>,
    pub failed_upstream_lookups: Arc<RwLock<HashMap<String, Instant>>>,
}

impl AppState {
    pub fn create_blob_descriptor(
        &self,
        sha256: &str,
        size: u64,
        content_type: Option<String>,
    ) -> BlobDescriptor {
        let extension = content_type
            .as_ref()
            .and_then(|ct| mime_guess::get_mime_extensions_str(ct))
            .and_then(|mime| mime.first().map(|ext| ext.to_string()));

        let url = match extension {
            Some(ext) => format!("{}/{}.{}", self.public_url, sha256, ext),
            None => format!("{}/{}", self.public_url, sha256),
        };

        BlobDescriptor {
            url,
            sha256: sha256.to_string(),
            size,
            r#type: content_type,
            uploaded: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    pub async fn get_stats(&self) -> Stats {
        let index = self.file_index.read().await;
        let total_files = index.len();
        let total_size_bytes: u64 = index.values().map(|m| m.size).sum();
        let total_size_mb = total_size_bytes as f64 / (1024.0 * 1024.0);
        let max_total_size_mb = self.max_total_size as f64 / (1024.0 * 1024.0);
        let storage_usage_percent = (total_size_bytes as f64 / self.max_total_size as f64) * 100.0;

        // Calculate throughput over the last hour
        let upload_throughput_data = self.upload_throughput_data.read().await;
        let one_hour_ago = Instant::now() - std::time::Duration::from_secs(3600);
        let recent_upload_data: Vec<_> = upload_throughput_data
            .iter()
            .filter(|(timestamp, _)| *timestamp > one_hour_ago)
            .collect();

        let upload_throughput_mbps = if recent_upload_data.len() > 1 {
            let total_bytes: u64 = recent_upload_data.iter().map(|(_, bytes)| bytes).sum();
            let time_span = recent_upload_data
                .last()
                .unwrap()
                .0
                .duration_since(recent_upload_data.first().unwrap().0);
            if time_span.as_secs() > 0 {
                (total_bytes as f64 / (1024.0 * 1024.0)) / (time_span.as_secs() as f64)
            } else {
                0.0
            }
        } else {
            0.0
        };

        let download_throughput_data = self.download_throughput_data.read().await;
        let one_hour_ago = Instant::now() - std::time::Duration::from_secs(3600);
        let recent_download_data: Vec<_> = download_throughput_data
            .iter()
            .filter(|(timestamp, _)| *timestamp > one_hour_ago)
            .collect();

        let download_throughput_mbps = if recent_download_data.len() > 1 {
            let total_bytes: u64 = recent_download_data.iter().map(|(_, bytes)| bytes).sum();
            let time_span = recent_download_data
                .last()
                .unwrap()
                .0
                .duration_since(recent_download_data.first().unwrap().0);
            if time_span.as_secs() > 0 {
                (total_bytes as f64 / (1024.0 * 1024.0)) / (time_span.as_secs() as f64)
            } else {
                0.0
            }
        } else {
            0.0
        };

        let files_uploaded = *self.files_uploaded.read().await;
        let files_downloaded = *self.files_downloaded.read().await;

        Stats {
            total_files,
            total_size_bytes,
            total_size_mb,
            upload_throughput_mbps,
            download_throughput_mbps,
            files_uploaded,
            files_downloaded,
            max_total_size_mb,
            max_total_files: self.max_total_files,
            storage_usage_percent,
        }
    }
}

#[derive(Serialize)]
pub struct BlobDescriptor {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub uploaded: u64,
}

#[derive(Serialize)]
pub struct Stats {
    pub total_files: usize,
    pub total_size_bytes: u64,
    pub total_size_mb: f64,
    pub upload_throughput_mbps: f64,
    pub download_throughput_mbps: f64,
    pub files_uploaded: u64,
    pub files_downloaded: u64,
    pub max_total_size_mb: f64,
    pub max_total_files: usize,
    pub storage_usage_percent: f64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
}

#[derive(Clone)]
pub struct ChunkUpload {
    pub sha256: String,
    pub upload_type: String,
    pub upload_length: u64,
    pub temp_path: PathBuf,
    pub chunks: Vec<ChunkInfo>,
    pub created_at: Instant,
}

#[derive(Clone)]
pub struct ChunkInfo {
    pub offset: u64,
    pub length: u64,
    pub chunk_path: PathBuf,
}
