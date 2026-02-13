use prometheus::{Registry, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts};
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
    pub max_total_files: IntGauge,
    pub max_storage_bytes: IntGauge,
    pub max_age: IntGauge,
    pub feature_enabled: IntGaugeVec,
    pub storage_usage_percent: IntGauge,
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

        let max_total_files = IntGauge::with_opts(
            Opts::new("almond_max_total_files", "Maximum total number of files allowed")
        ).expect("Failed to create metrics_max_total_files gauge");
        registry.register(Box::new(max_total_files.clone())).expect("Failed to register metrics_max_total_files");

        let max_storage_bytes = IntGauge::with_opts(
            Opts::new("almond_max_storage_bytes", "Maximum total storage in bytes allowed")
        ).expect("Failed to create metrics_max_storage_bytes gauge");
        registry.register(Box::new(max_storage_bytes.clone())).expect("Failed to register metrics_max_storage_bytes");

        let max_age = IntGauge::with_opts(
            Opts::new("almond_max_age", "Maximum file age in days")
        ).expect("Failed to create metrics_max_age gauge");
        registry.register(Box::new(max_age.clone())).expect("Failed to register metrics_max_age");

        let feature_enabled = IntGaugeVec::new(
            Opts::new("almond_feature_enabled", "Feature enabled status (0=off, 1=wot, 2=public)"),
            &["feature"]
        ).expect("Failed to create metrics_feature_enabled gauge");
        registry.register(Box::new(feature_enabled.clone())).expect("Failed to register metrics_feature_enabled");

        let storage_usage_percent = IntGauge::with_opts(
            Opts::new("almond_storage_usage_percent", "Storage usage as a percentage (0-100)")
        ).expect("Failed to create metrics_storage_usage_percent gauge");
        registry.register(Box::new(storage_usage_percent.clone())).expect("Failed to register metrics_storage_usage_percent");

        Self {
            registry,
            files_uploaded,
            files_downloaded,
            storage_bytes,
            total_files,
            served_bytes,
            downloaded_from_upstream_bytes,
            free_disk_space,
            max_total_files,
            max_storage_bytes,
            max_age,
            feature_enabled,
            storage_usage_percent,
        }
    }

    /// Update metrics from current state
    pub fn update(
        &self,
        files_uploaded: u64,
        files_downloaded: u64,
        total_size_bytes: u64,
        total_files: usize,
        max_total_files: usize,
        max_total_size: u64,
        max_file_age_days: u64,
        upload_dir: &Path,
    ) {
        // Update file and storage metrics
        self.files_uploaded.inc_by(files_uploaded.saturating_sub(self.files_uploaded.get()));
        self.files_downloaded.inc_by(files_downloaded.saturating_sub(self.files_downloaded.get()));
        self.storage_bytes.set(total_size_bytes as i64);
        self.total_files.set(total_files as i64);

        // Update config metrics
        self.max_total_files.set(max_total_files as i64);
        self.max_storage_bytes.set(max_total_size as i64);
        self.max_age.set(max_file_age_days as i64);

        // Calculate and update free disk space
        let free_disk_space_bytes = match fs2::free_space(upload_dir) {
            Ok(free) => free,
            Err(e) => {
                warn!("Failed to get free disk space: {}", e);
                0
            }
        };
        self.free_disk_space.set(free_disk_space_bytes as i64);

        // Calculate and update storage usage percentage
        let storage_usage_percent = if max_total_size > 0 {
            ((total_size_bytes as f64 / max_total_size as f64) * 100.0) as i64
        } else {
            0
        };
        self.storage_usage_percent.set(storage_usage_percent);
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

    /// Update feature flag metrics
    pub fn update_feature_flags(
        &self,
        upload: &crate::models::FeatureMode,
        mirror: &crate::models::FeatureMode,
        custom_upstream: &crate::models::FeatureMode,
    ) {
        // Map FeatureMode to numeric values: 0=off, 1=wot, 2=public, 3=dvm
        let to_metric_value = |mode: &crate::models::FeatureMode| -> i64 {
            match mode {
                crate::models::FeatureMode::Off => 0,
                crate::models::FeatureMode::Wot => 1,
                crate::models::FeatureMode::Public => 2,
                crate::models::FeatureMode::Dvm => 3,
            }
        };

        self.feature_enabled.with_label_values(&["upload"]).set(to_metric_value(upload));
        self.feature_enabled.with_label_values(&["mirror"]).set(to_metric_value(mirror));
        self.feature_enabled.with_label_values(&["custom_upstream"]).set(to_metric_value(custom_upstream));
    }
}
