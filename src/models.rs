use crate::helpers::get_extension_from_mime;
use crate::metrics::Metrics;
use cdk::wallet::Wallet as CdkWallet;
use nostr_relay_pool::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, RwLock};

/// Feature mode controlling access to features
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureMode {
    /// Feature is disabled
    Off,
    /// Feature is enabled only for WOT (Web of Trust) pubkeys
    Wot,
    /// Feature is enabled for everyone
    Public,
}

impl FeatureMode {
    /// Parse from string value (off/wot/public, case-insensitive)
    /// Falls back to a default if the string doesn't match
    pub fn from_str_with_default(s: &str, default: FeatureMode) -> Self {
        match s.to_lowercase().as_str() {
            "off" | "false" => FeatureMode::Off,
            "wot" => FeatureMode::Wot,
            "public" | "true" => FeatureMode::Public,
            _ => default,
        }
    }

    /// Check if feature is enabled (wot or public)
    pub fn is_enabled(&self) -> bool {
        matches!(self, FeatureMode::Wot | FeatureMode::Public)
    }

    /// Check if feature requires WOT validation
    pub fn requires_wot(&self) -> bool {
        matches!(self, FeatureMode::Wot)
    }

    /// Convert to string for metrics/logging
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureMode::Off => "off",
            FeatureMode::Wot => "wot",
            FeatureMode::Public => "public",
        }
    }
}

/// Upstream mode controlling how files are fetched from upstream servers
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpstreamMode {
    /// Proxy: Stream from upstream while saving locally (current behavior)
    #[default]
    Proxy,
    /// Redirect: Issue 302 redirect to upstream, no local caching
    Redirect,
    /// RedirectAndCache: Issue 302 redirect to upstream, download in background for future requests
    RedirectAndCache,
}

impl UpstreamMode {
    /// Parse from string value (proxy/redirect/redirect_and_cache, case-insensitive)
    /// Falls back to Proxy if the string doesn't match
    pub fn from_str_with_default(s: &str) -> Self {
        match s.to_lowercase().replace('-', "_").as_str() {
            "proxy" => UpstreamMode::Proxy,
            "redirect" => UpstreamMode::Redirect,
            "redirect_and_cache" | "redirectandcache" => UpstreamMode::RedirectAndCache,
            _ => UpstreamMode::Proxy,
        }
    }

    /// Check if this mode uses redirect (vs proxy)
    pub fn is_redirect(&self) -> bool {
        matches!(self, UpstreamMode::Redirect | UpstreamMode::RedirectAndCache)
    }

    /// Check if this mode caches in background after redirect
    pub fn caches_in_background(&self) -> bool {
        matches!(self, UpstreamMode::RedirectAndCache)
    }

    /// Convert to string for logging
    pub fn as_str(&self) -> &'static str {
        match self {
            UpstreamMode::Proxy => "proxy",
            UpstreamMode::Redirect => "redirect",
            UpstreamMode::RedirectAndCache => "redirect_and_cache",
        }
    }
}

/// Action to take when a blob is reported (BUD-09)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportAction {
    /// Quarantine the blob (move to quarantine directory, still accessible to admins)
    Quarantine,
    /// Delete the blob permanently
    Delete,
}

impl ReportAction {
    /// Parse from string value (quarantine/delete, case-insensitive)
    /// Falls back to Quarantine if the string doesn't match
    pub fn from_str_with_default(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "delete" => ReportAction::Delete,
            "quarantine" | _ => ReportAction::Quarantine,
        }
    }

    /// Convert to string for logging
    pub fn as_str(&self) -> &'static str {
        match self {
            ReportAction::Quarantine => "quarantine",
            ReportAction::Delete => "delete",
        }
    }
}

type OngoingDownloadsMap = Arc<RwLock<HashMap<String, (Instant, Arc<AtomicU64>, Arc<Notify>, PathBuf, String)>>>;

#[derive(Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub path: PathBuf,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    pub size: u64,
    pub created_at: u64,
    pub pubkey: Option<PublicKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u64>,
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
    pub upstream_servers: Vec<String>,
    pub upstream_mode: UpstreamMode,
    pub max_upstream_download_size_mb: u64,
    pub max_chunk_size_mb: u64,
    pub chunk_cleanup_timeout_minutes: u64,
    pub feature_upload_enabled: FeatureMode,
    pub feature_mirror_enabled: FeatureMode,
    pub feature_list_enabled: bool,
    pub feature_custom_upstream_origin_enabled: FeatureMode,
    pub feature_homepage_enabled: bool,
    pub ongoing_downloads: OngoingDownloadsMap,
    pub chunk_uploads: Arc<RwLock<HashMap<String, ChunkUpload>>>,
    pub failed_upstream_lookups: Arc<RwLock<HashMap<String, Instant>>>,
    pub blossom_server_lists: Arc<RwLock<HashMap<PublicKey, (Vec<String>, Instant)>>>,
    pub blossom_server_list_cache_ttl_hours: u64,
    /// Filter algorithm: "bloom", "binary-fuse-8", "binary-fuse-16", or "binary-fuse-32"
    pub filter_algorithm: String,
    // Prometheus metrics
    pub metrics: Metrics,
    /// Action to take when a blob is reported (quarantine or delete)
    pub report_action: ReportAction,
    /// Whether reports feature is enabled
    pub feature_report_enabled: FeatureMode,
    // Cashu payment configuration (BUD-07)
    pub feature_paid_upload: bool,
    pub feature_paid_mirror: bool,
    pub feature_paid_download: bool,
    pub cashu_price_per_mb: u64,
    pub cashu_accepted_mints: Vec<String>,
    pub cashu_wallet_path: PathBuf,
    pub cashu_wallet: Option<Arc<RwLock<CdkWallet>>>,
}

impl AppState {
    pub fn create_blob_descriptor(
        &self,
        sha256: &str,
        size: u64,
        content_type: Option<String>,
        expiration: Option<u64>,
    ) -> BlobDescriptor {
        let extension = content_type
            .as_ref()
            .and_then(|ct| get_extension_from_mime(ct));

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
            expiration,
        }
    }

    pub async fn get_stats(&self) {
        let index = self.file_index.read().await;
        let total_files = index.len();
        let total_size_bytes: u64 = index.values().map(|m| m.size).sum();

        let files_uploaded = *self.files_uploaded.read().await;
        let files_downloaded = *self.files_downloaded.read().await;

        // Update Prometheus metrics
        self.metrics.update(
            files_uploaded,
            files_downloaded,
            total_size_bytes,
            total_files,
            self.max_total_files,
            self.max_total_size,
            self.max_file_age_days,
            &self.upload_dir,
        );

        // Update feature flag metrics
        self.metrics.update_feature_flags(
            &self.feature_upload_enabled,
            &self.feature_mirror_enabled,
            &self.feature_custom_upstream_origin_enabled,
        );
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
    #[serde(rename = "as")]
    pub author: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileRequestQuery {
    /// Legacy server parameter (supports multiple servers)
    #[serde(rename = "server", default)]
    pub servers: Vec<String>,
    /// Single custom origin server
    pub origin: Option<String>,
    /// Servers where the file is stored (multiple xs parameters allowed, Blossom BUD-01)
    #[serde(default)]
    pub xs: Vec<String>,
    /// Author pubkey (Blossom BUD-01)
    #[serde(rename = "as")]
    pub author_pubkey: Option<String>,
}

#[derive(Clone)]
pub struct ChunkUpload {
    pub sha256: String,
    pub upload_type: String,
    pub upload_length: u64,
    pub temp_path: PathBuf,
    pub chunks: Vec<ChunkInfo>,
    pub created_at: Instant,
    pub expiration: Option<u64>,
}

#[derive(Clone)]
pub struct ChunkInfo {
    pub offset: u64,
    pub length: u64,
    pub chunk_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_request_query_single_xs() {
        // Test with serde_html_form which is what axum_extra::Query uses
        let query_string = "xs=blossom.primal.net";
        let result: Result<FileRequestQuery, _> = serde_html_form::from_str(query_string);
        assert!(result.is_ok(), "Single xs parameter should deserialize successfully: {:?}", result.as_ref().err());
        let query = result.unwrap();
        assert_eq!(query.xs.len(), 1);
        assert_eq!(query.xs[0], "blossom.primal.net");
    }

    #[test]
    fn test_file_request_query_multiple_xs() {
        let query_string = "xs=blossom.primal.net&xs=video.nostr.build";
        let result: Result<FileRequestQuery, _> = serde_html_form::from_str(query_string);
        assert!(result.is_ok(), "Multiple xs parameters should deserialize successfully: {:?}", result.as_ref().err());
        let query = result.unwrap();
        assert_eq!(query.xs.len(), 2);
        assert_eq!(query.xs[0], "blossom.primal.net");
        assert_eq!(query.xs[1], "video.nostr.build");
    }

    #[test]
    fn test_file_request_query_no_xs() {
        let query_string = "origin=example.com";
        let result: Result<FileRequestQuery, _> = serde_html_form::from_str(query_string);
        assert!(result.is_ok(), "Query without xs should deserialize successfully");
        let query = result.unwrap();
        assert_eq!(query.xs.len(), 0);
    }

    #[test]
    fn test_file_request_query_combined() {
        let query_string = "xs=blossom.primal.net&xs=video.nostr.build&as=08039bc2786f9f58c94146c6666fac9a7d7ceb40d0798a8f49140763cc715053";
        let result: Result<FileRequestQuery, _> = serde_html_form::from_str(query_string);
        assert!(result.is_ok(), "Query with multiple xs and as parameters should deserialize successfully: {:?}", result.err());
        let query = result.unwrap();
        assert_eq!(query.xs.len(), 2);
        assert_eq!(query.xs[0], "blossom.primal.net");
        assert_eq!(query.xs[1], "video.nostr.build");
        assert_eq!(query.author_pubkey, Some("08039bc2786f9f58c94146c6666fac9a7d7ceb40d0798a8f49140763cc715053".to_string()));
    }

    #[test]
    fn test_upstream_mode_parsing() {
        // Test all valid values
        assert_eq!(UpstreamMode::from_str_with_default("proxy"), UpstreamMode::Proxy);
        assert_eq!(UpstreamMode::from_str_with_default("redirect"), UpstreamMode::Redirect);
        assert_eq!(UpstreamMode::from_str_with_default("redirect_and_cache"), UpstreamMode::RedirectAndCache);

        // Test case insensitivity
        assert_eq!(UpstreamMode::from_str_with_default("PROXY"), UpstreamMode::Proxy);
        assert_eq!(UpstreamMode::from_str_with_default("REDIRECT"), UpstreamMode::Redirect);
        assert_eq!(UpstreamMode::from_str_with_default("REDIRECT_AND_CACHE"), UpstreamMode::RedirectAndCache);

        // Test hyphen variant
        assert_eq!(UpstreamMode::from_str_with_default("redirect-and-cache"), UpstreamMode::RedirectAndCache);

        // Test default fallback for invalid values
        assert_eq!(UpstreamMode::from_str_with_default("invalid"), UpstreamMode::Proxy);
        assert_eq!(UpstreamMode::from_str_with_default(""), UpstreamMode::Proxy);
    }

    #[test]
    fn test_upstream_mode_methods() {
        // Test is_redirect
        assert!(!UpstreamMode::Proxy.is_redirect());
        assert!(UpstreamMode::Redirect.is_redirect());
        assert!(UpstreamMode::RedirectAndCache.is_redirect());

        // Test caches_in_background
        assert!(!UpstreamMode::Proxy.caches_in_background());
        assert!(!UpstreamMode::Redirect.caches_in_background());
        assert!(UpstreamMode::RedirectAndCache.caches_in_background());

        // Test as_str
        assert_eq!(UpstreamMode::Proxy.as_str(), "proxy");
        assert_eq!(UpstreamMode::Redirect.as_str(), "redirect");
        assert_eq!(UpstreamMode::RedirectAndCache.as_str(), "redirect_and_cache");
    }
}
