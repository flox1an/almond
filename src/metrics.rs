use prometheus::{Registry, IntCounter, IntCounterVec, IntGauge, Opts};
use std::path::Path;
use tracing::warn;

/// Container for all Prometheus metrics
#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,
    pub files_uploaded: IntCounter,
    pub files_downloaded: IntCounter,
    pub storage_bytes: IntGauge,
    pub total_files: IntGauge,
    pub served_bytes: IntCounter,
    pub downloaded_from_upstream_bytes: IntCounterVec,
    pub free_disk_space: IntGauge,
}

impl Metrics {
    /// Initialize all Prometheus metrics
    pub fn new() -> Self {
        let registry = Registry::new();

        let files_uploaded = IntCounter::with_opts(
            Opts::new("almond_files_uploaded_total", "Total number of files uploaded")
        ).expect("Failed to create metrics_files_uploaded counter");
        registry.register(Box::new(files_uploaded.clone())).expect("Failed to register metrics_files_uploaded");

        let files_downloaded = IntCounter::with_opts(
            Opts::new("almond_files_downloaded_total", "Total number of files downloaded")
        ).expect("Failed to create metrics_files_downloaded counter");
        registry.register(Box::new(files_downloaded.clone())).expect("Failed to register metrics_files_downloaded");

        let storage_bytes = IntGauge::with_opts(
            Opts::new("almond_storage_bytes", "Total storage used in bytes")
        ).expect("Failed to create metrics_storage_bytes gauge");
        registry.register(Box::new(storage_bytes.clone())).expect("Failed to register metrics_storage_bytes");

        let total_files = IntGauge::with_opts(
            Opts::new("almond_total_files", "Total number of files stored")
        ).expect("Failed to create metrics_total_files gauge");
        registry.register(Box::new(total_files.clone())).expect("Failed to register metrics_total_files");

        let served_bytes = IntCounter::with_opts(
            Opts::new("almond_served_bytes", "Total bytes served to users")
        ).expect("Failed to create metrics_served_bytes counter");
        registry.register(Box::new(served_bytes.clone())).expect("Failed to register metrics_served_bytes");

        let downloaded_from_upstream_bytes = IntCounterVec::new(
            Opts::new("almond_downloaded_from_upstream_bytes", "Total bytes downloaded from upstream servers"),
            &["upstream"]
        ).expect("Failed to create metrics_downloaded_from_upstream_bytes counter");
        registry.register(Box::new(downloaded_from_upstream_bytes.clone())).expect("Failed to register metrics_downloaded_from_upstream_bytes");

        let free_disk_space = IntGauge::with_opts(
            Opts::new("almond_free_disk_space_bytes", "Free disk space in bytes")
        ).expect("Failed to create metrics_free_disk_space gauge");
        registry.register(Box::new(free_disk_space.clone())).expect("Failed to register metrics_free_disk_space");

        Self {
            registry,
            files_uploaded,
            files_downloaded,
            storage_bytes,
            total_files,
            served_bytes,
            downloaded_from_upstream_bytes,
            free_disk_space,
        }
    }

    /// Update metrics from current state
    pub fn update(
        &self,
        files_uploaded: u64,
        files_downloaded: u64,
        total_size_bytes: u64,
        total_files: usize,
        upload_dir: &Path,
    ) {
        // Update file and storage metrics
        self.files_uploaded.inc_by(files_uploaded.saturating_sub(self.files_uploaded.get()));
        self.files_downloaded.inc_by(files_downloaded.saturating_sub(self.files_downloaded.get()));
        self.storage_bytes.set(total_size_bytes as i64);
        self.total_files.set(total_files as i64);

        // Calculate and update free disk space
        let free_disk_space_bytes = match fs2::free_space(upload_dir) {
            Ok(free) => free,
            Err(e) => {
                warn!("Failed to get free disk space: {}", e);
                0
            }
        };
        self.free_disk_space.set(free_disk_space_bytes as i64);
    }

    /// Track bytes served to a user
    pub fn track_served_bytes(&self, bytes: u64) {
        self.served_bytes.inc_by(bytes);
    }

    /// Track bytes downloaded from an upstream server
    pub fn track_upstream_download(&self, upstream_url: &str, bytes: u64) {
        // Extract hostname from URL for the metric label
        let upstream_host = upstream_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("unknown");

        self.downloaded_from_upstream_bytes
            .with_label_values(&[upstream_host])
            .inc_by(bytes);
    }
}
