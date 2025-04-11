use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub upload_dir: PathBuf,
    pub file_index: Arc<RwLock<HashMap<String, PathBuf>>>,
    pub max_total_size: u64,
    pub max_total_files: usize,
    pub bind_addr: String,
    pub public_url: String,
    pub cleanup_interval_secs: u64,
    pub changes_pending: Arc<RwLock<bool>>,
}

impl AppState {
    pub fn create_blob_descriptor(
        &self,
        sha256: &str,
        size: u64,
        content_type: Option<String>,
    ) -> BlobDescriptor {
        BlobDescriptor {
            url: format!("{}/{}", self.public_url, sha256),
            sha256: sha256.to_string(),
            size,
            r#type: content_type,
            uploaded: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
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

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
}
