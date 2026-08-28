//! Pure configuration parsing and validation.
//!
//! `Config` holds every tunable currently read from the environment,
//! already parsed into the correct types and validated.  No I/O, no
//! filesystem, no network, no `std::process::exit` — errors are values.
//!
//! # Source of truth
//! All env-var names are defined here.  The build step in [`main`](crate::main)
//! reads `Config` and performs the process-side effects (storage init, wallet
//! setup, …) in the correct order.

use std::{collections::HashMap, path::PathBuf};

use crate::models::{FeatureMode, ReportAction, UpstreamMode};
use nostr_sdk::prelude::{FromBech32, PublicKey};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// A configuration error — parse failure or cross-field invariant violation.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    fn parse(name: &'static str, value: impl std::fmt::Display, detail: String) -> Self {
        Self {
            message: format!("Failed to parse {name}: '{value}' — {detail}"),
        }
    }

    fn validation(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Helpers (private)
// ---------------------------------------------------------------------------

/// Read a value from the map, falling back to `default`, then parse it.
fn parse_str<'a>(
    map: &'a HashMap<String, String>,
    name: &'static str,
    default: &'static str,
) -> Result<&'a str, ConfigError> {
    let val = map.get(name).map(String::as_str).unwrap_or(default);
    Ok(val)
}

/// Parse a value through a fallible conversion function.
fn parse_into<T>(
    map: &HashMap<String, String>,
    name: &'static str,
    default: &'static str,
    convert: fn(&str) -> Result<T, String>,
) -> Result<T, ConfigError> {
    let raw = map.get(name).map(String::as_str).unwrap_or(default);
    convert(raw).map_err(|detail| ConfigError::parse(name, raw, detail))
}

/// Parse an optional value.
fn parse_opt<T>(
    map: &HashMap<String, String>,
    name: &'static str,
    convert: fn(&str) -> Result<T, String>,
) -> Result<Option<T>, ConfigError> {
    match map.get(name) {
        Some(raw) if !raw.trim().is_empty() => convert(raw)
            .map(Some)
            .map_err(|detail| ConfigError::parse(name, raw, detail)),
        _ => Ok(None),
    }
}

/// Read a boolean (true/false/1/0, case-insensitive).
fn parse_bool(
    map: &HashMap<String, String>,
    name: &'static str,
    default: &'static str,
) -> Result<bool, ConfigError> {
    parse_into(map, name, default, |s| {
        match s.trim().to_lowercase().as_str() {
            "true" | "1" | "on" => Ok(true),
            "false" | "0" | "off" => Ok(false),
            other => Err(format!("expected true/false, got '{other}'")),
        }
    })
}

/// Read a u64 whose env-var value is in mebibytes, result is in bytes.
fn parse_mebibytes(
    map: &HashMap<String, String>,
    name: &'static str,
    default: &'static str,
) -> Result<u64, ConfigError> {
    let mb: u64 = parse_into(map, name, default, |s| {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer: {e}"))
    })?;
    mb.checked_mul(1024 * 1024)
        .ok_or_else(|| ConfigError::parse(name, mb, "value too large".into()))
}

/// Parse a comma-separated list, applying a per-element conversion.
fn parse_list<T>(
    map: &HashMap<String, String>,
    name: &'static str,
    convert: fn(&str) -> Result<T, String>,
) -> Result<Vec<T>, ConfigError> {
    let raw = map.get(name).map(String::as_str).unwrap_or("");
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| convert(s).map_err(|detail| ConfigError::parse(name, s, detail)))
        .collect()
}

/// Parse a u64 directly.
fn parse_u64(
    map: &HashMap<String, String>,
    name: &'static str,
    default: &'static str,
) -> Result<u64, ConfigError> {
    parse_into(map, name, default, |s| {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer: {e}"))
    })
}

/// Parse a `usize` directly.
fn parse_usize(
    map: &HashMap<String, String>,
    name: &'static str,
    default: &'static str,
) -> Result<usize, ConfigError> {
    parse_into(map, name, default, |s| {
        s.parse::<usize>()
            .map_err(|e| format!("invalid integer: {e}"))
    })
}

/// Parse the filter algorithm, falling back to `binary-fuse-16` on bad input.
fn parse_filter_algorithm(map: &HashMap<String, String>, name: &'static str) -> String {
    let raw = map
        .get(name)
        .map(String::as_str)
        .unwrap_or("binary-fuse-16")
        .to_lowercase();
    match raw.as_str() {
        "bloom" | "binary-fuse-8" | "binary-fuse-16" | "binary-fuse-32" => raw,
        _ => "binary-fuse-16".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Every tunable this server reads from the environment, parsed and validated.
///
/// Build one with [`Config::from_map`] (injectable) or [`Config::from_env`]
/// (real process environment).  Then pass it to the build step in `main`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // ── Storage ────────────────────────────────────────────────────────────
    pub storage_path: String,

    // ── S3 native storage (all-or-nothing) ─────────────────────────────────
    pub s3_endpoint: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_access_key_id: Option<String>,
    pub s3_secret_access_key: Option<String>,

    // ── Network ────────────────────────────────────────────────────────────
    pub bind_addr: String,
    pub enable_https: bool,
    pub tls_cert_path: PathBuf,
    pub tls_key_path: PathBuf,
    pub tls_auto_generate: bool,
    pub public_url: String,
    pub cors_allowed_origins: Vec<String>,

    // ── Serve-files ────────────────────────────────────────────────────────
    pub serve_files_path: Option<PathBuf>,
    /// Fallback directory for the generated manifest when `serve_files_path`
    /// is not writable. Defaults to `/tmp`.
    pub serve_files_manifest_dir: PathBuf,
    pub serve_files_manifest_name: String,
    pub serve_files_refresh_interval_secs: u64,

    // ── Limits ─────────────────────────────────────────────────────────────
    /// Total maximum storage across all files (bytes).
    pub max_total_size: u64,
    pub max_total_files: usize,
    /// Absolute ceiling for one blob (bytes).
    pub max_blob_size_bytes: u64,
    /// Minimum free disk space (bytes).
    pub min_free_disk_bytes: u64,
    pub cleanup_interval_secs: u64,
    pub max_file_age_days: u64,
    pub max_upstream_cache_ttl_days: u64,
    pub max_upstream_download_size_mb: u64,
    pub max_chunk_size_mb: u64,
    pub chunk_cleanup_timeout_minutes: u64,
    pub max_chunk_upload_sessions: usize,
    pub max_chunk_upload_sessions_per_pubkey: usize,

    // ── Upstream ───────────────────────────────────────────────────────────
    pub upstream_servers: Vec<String>,
    pub upstream_mode: UpstreamMode,

    // ── Features ───────────────────────────────────────────────────────────
    pub feature_upload_enabled: FeatureMode,
    pub feature_mirror_enabled: FeatureMode,
    pub feature_list_enabled: bool,
    pub feature_custom_upstream_origin_enabled: FeatureMode,
    pub feature_homepage_enabled: bool,
    pub feature_report_enabled: FeatureMode,
    pub report_action: ReportAction,

    // ── Cashu / paid features (BUD-07) ─────────────────────────────────────
    pub feature_paid_upload: bool,
    pub feature_paid_mirror: bool,
    pub feature_paid_download: bool,
    pub cashu_price_per_mb: u64,
    pub cashu_accepted_mints: Vec<String>,
    pub cashu_wallet_path: PathBuf,
    /// Max parallel HLS segment fetches.
    pub hls_mirror_concurrency: usize,

    // ── Bloom / Xor filters (BUD-11) ───────────────────────────────────────
    pub blossom_server_list_cache_ttl_hours: u64,
    pub filter_algorithm: String,

    // ── DVM ────────────────────────────────────────────────────────────────
    pub dvm_allowed_kinds: Vec<u16>,
    pub dvm_relays: Vec<String>,
    pub dvm_refresh_interval_mins: u64,

    // ── Auth ───────────────────────────────────────────────────────────────
    pub allowed_pubkeys: Vec<PublicKey>,
    pub auth_max_ttl_secs: u64,
    pub auth_clock_skew_secs: u64,
    pub auth_require_server_tag: bool,
    pub metrics_bearer_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Parse and validate
// ---------------------------------------------------------------------------

impl Config {
    /// Parse configuration from an injectable key-value map.
    ///
    /// Every validation invariant that was previously a `panic!` or
    /// `std::process::exit(1)` is a returned [`ConfigError`] here.
    pub fn from_map(map: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let enable_https = parse_bool(map, "ENABLE_HTTPS", "false")?;

        // Read values first so we can validate cross-field invariants.
        let max_total_size = parse_mebibytes(map, "MAX_TOTAL_SIZE", "99999")?;
        let max_total_files = parse_usize(map, "MAX_TOTAL_FILES", "99999999")?;
        let bind_addr = parse_str(map, "BIND_ADDR", "127.0.0.1:3000")?;
        bind_addr.parse::<std::net::SocketAddr>().map_err(|e| {
            ConfigError::parse(
                "BIND_ADDR",
                bind_addr,
                format!("must be a valid IP:port socket address: {e}"),
            )
        })?;
        let bind_addr = bind_addr.to_owned();
        let tls_cert_path = parse_path(map, "TLS_CERT_PATH", "./cert.pem");
        let tls_key_path = parse_path(map, "TLS_KEY_PATH", "./key.pem");
        let tls_auto_generate = parse_bool(map, "TLS_AUTO_GENERATE", "true")?;
        let public_url_default = if enable_https {
            "https://127.0.0.1:3000"
        } else {
            "http://127.0.0.1:3000"
        };
        let public_url = parse_str(map, "PUBLIC_URL", public_url_default)?.to_owned();
        let cors_allowed_origins: Vec<String> =
            parse_list(map, "CORS_ALLOWED_ORIGINS", |s| Ok(s.to_owned()))?;
        let storage_path = parse_str(map, "STORAGE_PATH", "./files")?.to_owned();

        // S3: all-or-nothing validation
        let s3_endpoint = parse_opt(map, "ALMOND_S3_ENDPOINT", |s| Ok(s.to_owned()))?;
        let s3_bucket = parse_opt(map, "ALMOND_S3_BUCKET", |s| Ok(s.to_owned()))?;
        let s3_access_key_id = parse_opt(map, "ALMOND_S3_ACCESS_KEY_ID", |s| Ok(s.to_owned()))?;
        let s3_secret_access_key =
            parse_opt(map, "ALMOND_S3_SECRET_ACCESS_KEY", |s| Ok(s.to_owned()))?;
        let s3_count = [
            &s3_endpoint,
            &s3_bucket,
            &s3_access_key_id,
            &s3_secret_access_key,
        ]
        .iter()
        .filter(|o| o.is_some())
        .count();
        match s3_count {
            0 => {} // all None → S3 disabled
            4 => {} // all Some → S3 enabled
            _ => {
                return Err(ConfigError::validation(
                    "Incomplete S3 configuration: all of ALMOND_S3_ENDPOINT, \
                     ALMOND_S3_BUCKET, ALMOND_S3_ACCESS_KEY_ID, and \
                     ALMOND_S3_SECRET_ACCESS_KEY must be set together",
                ));
            }
        }

        let serve_files_path =
            parse_opt(map, "SERVE_FILES_PATH", |s: &str| Ok(s.trim().to_owned()))?
                .filter(|v| !v.is_empty())
                .map(PathBuf::from);
        let serve_files_manifest_dir = parse_path(map, "SERVE_FILES_MANIFEST_DIR", "/tmp");
        let serve_files_manifest_name =
            parse_str(map, "SERVE_FILES_MANIFEST_NAME", "manifest-sha256.txt")?.to_owned();
        let serve_files_refresh_interval_secs =
            parse_u64(map, "SERVE_FILES_REFRESH_INTERVAL_SECS", "3600")?;

        let cleanup_interval_secs = parse_u64(map, "CLEANUP_INTERVAL_SECS", "30")?;
        if cleanup_interval_secs == 0 {
            return Err(ConfigError::validation(
                "CLEANUP_INTERVAL_SECS must be greater than zero",
            ));
        }

        let max_file_age_days = parse_u64(map, "MAX_FILE_AGE_DAYS", "0")?;
        let max_upstream_cache_ttl_days = parse_u64(map, "MAX_UPSTREAM_CACHE_TTL_DAYS", "1")?;
        let max_upstream_download_size_mb = parse_u64(map, "MAX_UPSTREAM_DOWNLOAD_SIZE_MB", "100")?;
        let max_chunk_size_mb = parse_u64(map, "MAX_CHUNK_SIZE_MB", "100")?;
        let max_blob_size_bytes = parse_mebibytes(map, "MAX_BLOB_SIZE_MB", "100")?;
        if max_chunk_size_mb
            .checked_mul(1024 * 1024)
            .is_some_and(|size| size > max_blob_size_bytes)
        {
            return Err(ConfigError::validation(
                "MAX_CHUNK_SIZE_MB cannot exceed MAX_BLOB_SIZE_MB",
            ));
        }

        let min_free_disk_bytes = parse_mebibytes(map, "MIN_FREE_DISK_MB", "256")?;
        let max_chunk_upload_sessions = parse_usize(map, "MAX_CHUNK_UPLOAD_SESSIONS", "128")?;
        let max_chunk_upload_sessions_per_pubkey =
            parse_usize(map, "MAX_CHUNK_UPLOAD_SESSIONS_PER_PUBKEY", "8")?;
        let chunk_cleanup_timeout_minutes = parse_u64(map, "CHUNK_CLEANUP_TIMEOUT_MINUTES", "30")?;

        let upstream_servers: Vec<String> =
            parse_list(map, "UPSTREAM_SERVERS", |s| Ok(s.to_owned()))?;
        let upstream_mode = parse_into(map, "UPSTREAM_MODE", "proxy", |s| {
            Ok(UpstreamMode::from_str_with_default(s))
        })?;

        let feature_upload_enabled = parse_into(map, "FEATURE_UPLOAD_ENABLED", "public", |s| {
            Ok(FeatureMode::from_str_with_default(s, FeatureMode::Public))
        })?;
        let feature_mirror_enabled = parse_into(map, "FEATURE_MIRROR_ENABLED", "public", |s| {
            Ok(FeatureMode::from_str_with_default(s, FeatureMode::Public))
        })?;
        let feature_list_enabled = parse_bool(map, "FEATURE_LIST_ENABLED", "true")?;
        let feature_custom_upstream_origin_enabled =
            parse_into(map, "FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED", "off", |s| {
                Ok(FeatureMode::from_str_with_default(s, FeatureMode::Off))
            })?;
        let feature_homepage_enabled = parse_bool(map, "FEATURE_HOMEPAGE_ENABLED", "true")?;

        let feature_report_enabled = parse_into(map, "FEATURE_REPORT_ENABLED", "off", |s| {
            Ok(FeatureMode::from_str_with_default(s, FeatureMode::Off))
        })?;
        let report_action = parse_into(map, "REPORT_ACTION", "quarantine", |s| {
            Ok(ReportAction::from_str_with_default(s))
        })?;

        let feature_paid_upload = parse_bool(map, "FEATURE_PAID_UPLOAD", "off")?;
        let feature_paid_mirror = parse_bool(map, "FEATURE_PAID_MIRROR", "off")?;
        let feature_paid_download = parse_bool(map, "FEATURE_PAID_DOWNLOAD", "off")?;
        let cashu_price_per_mb = parse_u64(map, "CASHU_PRICE_PER_MB", "1")?;
        let cashu_accepted_mints: Vec<String> =
            parse_list(map, "CASHU_ACCEPTED_MINTS", |s| Ok(s.to_owned()))?;
        let cashu_wallet_path = parse_path(map, "CASHU_WALLET_PATH", "./cashu_wallet.db");
        let hls_mirror_concurrency: usize = parse_usize(map, "HLS_MIRROR_CONCURRENCY", "4")?;

        // Validate: if any paid feature is on, exactly one mint must be configured.
        let any_paid_feature = feature_paid_upload || feature_paid_mirror || feature_paid_download;
        if any_paid_feature && cashu_accepted_mints.len() != 1 {
            return Err(ConfigError::validation(
                "Exactly one CASHU_ACCEPTED_MINTS value is required when paid features are enabled",
            ));
        }

        let blossom_server_list_cache_ttl_hours =
            parse_u64(map, "BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS", "24")?;
        let filter_algorithm = parse_filter_algorithm(map, "FILTER_ALGORITHM");

        let dvm_allowed_kinds = parse_list(map, "DVM_ALLOWED_KINDS", |s| {
            s.parse::<u16>()
                .map_err(|e| format!("invalid DVM kind: {e}"))
        })?;

        // Validate: if any feature uses DVM mode, kinds must be configured.
        let needs_dvm =
            feature_upload_enabled.requires_dvm() || feature_mirror_enabled.requires_dvm();
        if needs_dvm && dvm_allowed_kinds.is_empty() {
            return Err(ConfigError::validation(
                "DVM_ALLOWED_KINDS must be set when any feature uses 'dvm' mode",
            ));
        }

        let dvm_relays: Vec<String> = parse_list(map, "DVM_RELAYS", |s| Ok(s.to_owned()))?;
        let dvm_refresh_interval_mins = parse_u64(map, "DVM_REFRESH_INTERVAL_MINS", "5")?;

        // An unparseable npub is logged and skipped rather than refused, which
        // is what the boot procedure did before this seam existed. It is a
        // footgun - a typo silently shrinks the whitelist - but changing it
        // would turn a redeploy of an already-misconfigured server into a
        // failure to start, which is not this refactor's call to make.
        let allowed_pubkeys: Vec<PublicKey> = map
            .get("ALLOWED_NPUBS")
            .map(String::as_str)
            .unwrap_or_default()
            .split(',')
            .filter(|npub| !npub.trim().is_empty())
            .filter_map(|npub| match PublicKey::from_bech32(npub.trim()) {
                Ok(pubkey) => Some(pubkey),
                Err(error) => {
                    tracing::error!("Failed to parse npub {npub}: {error}");
                    None
                }
            })
            .collect();

        let auth_max_ttl_secs = parse_u64(map, "AUTH_MAX_TTL_SECS", "86400")?;
        let auth_clock_skew_secs = parse_u64(map, "AUTH_CLOCK_SKEW_SECS", "30")?;
        let auth_require_server_tag = parse_bool(map, "AUTH_REQUIRE_SERVER_TAG", "false")?;
        let metrics_bearer_token = parse_opt(map, "METRICS_BEARER_TOKEN", |s: &str| {
            Ok(s.trim().to_owned())
        })?
        .filter(|t| !t.is_empty());

        Ok(Self {
            storage_path,
            s3_endpoint,
            s3_bucket,
            s3_access_key_id,
            s3_secret_access_key,
            bind_addr,
            enable_https,
            tls_cert_path,
            tls_key_path,
            tls_auto_generate,
            public_url,
            cors_allowed_origins,
            serve_files_path,
            serve_files_manifest_dir,
            serve_files_manifest_name,
            serve_files_refresh_interval_secs,
            max_total_size,
            max_total_files,
            max_blob_size_bytes,
            min_free_disk_bytes,
            cleanup_interval_secs,
            max_file_age_days,
            max_upstream_cache_ttl_days,
            max_upstream_download_size_mb,
            max_chunk_size_mb,
            chunk_cleanup_timeout_minutes,
            max_chunk_upload_sessions,
            max_chunk_upload_sessions_per_pubkey,
            upstream_servers,
            upstream_mode,
            feature_upload_enabled,
            feature_mirror_enabled,
            feature_list_enabled,
            feature_custom_upstream_origin_enabled,
            feature_homepage_enabled,
            feature_report_enabled,
            report_action,
            feature_paid_upload,
            feature_paid_mirror,
            feature_paid_download,
            cashu_price_per_mb,
            cashu_accepted_mints,
            cashu_wallet_path,
            hls_mirror_concurrency,
            blossom_server_list_cache_ttl_hours,
            filter_algorithm,
            dvm_allowed_kinds,
            dvm_relays,
            dvm_refresh_interval_mins,
            allowed_pubkeys,
            auth_max_ttl_secs,
            auth_clock_skew_secs,
            auth_require_server_tag,
            metrics_bearer_token,
        })
    }

    /// Parse configuration from the real process environment.
    ///
    /// Calls `dotenvy::dotenv()` first so a `.env` file is loaded.
    pub fn from_env() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        let map: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&map)
    }
}

// ---------------------------------------------------------------------------
// Private
// ---------------------------------------------------------------------------

fn parse_path(map: &HashMap<String, String>, name: &'static str, default: &'static str) -> PathBuf {
    PathBuf::from(map.get(name).map(String::as_str).unwrap_or(default))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::models::{FeatureMode, ReportAction, UpstreamMode};
    use nostr_sdk::prelude::ToBech32;

    #[test]
    fn defaults_when_env_is_empty() {
        let cfg = Config::from_map(&HashMap::new()).unwrap();
        assert_eq!(cfg.bind_addr, "127.0.0.1:3000");
        assert!(!cfg.enable_https);
        assert_eq!(cfg.storage_path, "./files");
        assert_eq!(cfg.max_total_size, 99999 * 1024 * 1024);
        assert_eq!(cfg.max_total_files, 99999999);
        assert_eq!(cfg.max_blob_size_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.min_free_disk_bytes, 256 * 1024 * 1024);
        assert_eq!(cfg.cleanup_interval_secs, 30);
        assert_eq!(cfg.max_file_age_days, 0);
        assert_eq!(cfg.max_upstream_cache_ttl_days, 1);
        assert_eq!(cfg.max_upstream_download_size_mb, 100);
        assert_eq!(cfg.max_chunk_size_mb, 100);
        assert_eq!(cfg.chunk_cleanup_timeout_minutes, 30);
        assert_eq!(cfg.max_chunk_upload_sessions, 128);
        assert_eq!(cfg.max_chunk_upload_sessions_per_pubkey, 8);
        assert_eq!(cfg.public_url, "http://127.0.0.1:3000");
        assert!(cfg.cors_allowed_origins.is_empty());
        assert!(cfg.serve_files_path.is_none());
        assert_eq!(cfg.serve_files_manifest_dir, PathBuf::from("/tmp"));
        assert_eq!(cfg.serve_files_manifest_name, "manifest-sha256.txt");
        assert_eq!(cfg.serve_files_refresh_interval_secs, 3600);
        assert!(cfg.upstream_servers.is_empty());
        assert_eq!(cfg.upstream_mode, UpstreamMode::Proxy);
        assert_eq!(cfg.feature_upload_enabled, FeatureMode::Public);
        assert_eq!(cfg.feature_mirror_enabled, FeatureMode::Public);
        assert!(cfg.feature_list_enabled);
        assert_eq!(cfg.feature_custom_upstream_origin_enabled, FeatureMode::Off);
        assert!(cfg.feature_homepage_enabled);
        assert_eq!(cfg.feature_report_enabled, FeatureMode::Off);
        assert_eq!(cfg.report_action, ReportAction::Quarantine);
        assert!(!cfg.feature_paid_upload);
        assert!(!cfg.feature_paid_mirror);
        assert!(!cfg.feature_paid_download);
        assert_eq!(cfg.cashu_price_per_mb, 1);
        assert!(cfg.cashu_accepted_mints.is_empty());
        assert_eq!(cfg.cashu_wallet_path, PathBuf::from("./cashu_wallet.db"));
        assert_eq!(cfg.blossom_server_list_cache_ttl_hours, 24);
        assert_eq!(cfg.filter_algorithm, "binary-fuse-16");
        assert!(cfg.dvm_allowed_kinds.is_empty());
        assert!(cfg.dvm_relays.is_empty());
        assert_eq!(cfg.dvm_refresh_interval_mins, 5);
        assert!(cfg.allowed_pubkeys.is_empty());
        assert_eq!(cfg.auth_max_ttl_secs, 86400);
        assert_eq!(cfg.auth_clock_skew_secs, 30);
        assert!(!cfg.auth_require_server_tag);
        assert!(cfg.metrics_bearer_token.is_none());
        assert!(cfg.s3_endpoint.is_none());
    }

    // ── Validation invariants ──────────────────────────────────────────────

    #[test]
    fn chunk_size_cannot_exceed_blob_size() {
        let mut map = HashMap::new();
        map.insert("MAX_CHUNK_SIZE_MB".to_owned(), "200".to_owned());
        map.insert("MAX_BLOB_SIZE_MB".to_owned(), "100".to_owned());
        let err = Config::from_map(&map).unwrap_err();
        assert!(
            err.message
                .contains("MAX_CHUNK_SIZE_MB cannot exceed MAX_BLOB_SIZE_MB"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn paid_feature_requires_exactly_one_mint() {
        // Zero mints with paid feature on → error
        let mut map = HashMap::new();
        map.insert("FEATURE_PAID_UPLOAD".to_owned(), "on".to_owned());
        let err = Config::from_map(&map).unwrap_err();
        assert!(
            err.message.contains("Exactly one CASHU_ACCEPTED_MINTS"),
            "{err}"
        );

        // Two mints → error
        let mut map2 = HashMap::new();
        map2.insert("FEATURE_PAID_MIRROR".to_owned(), "on".to_owned());
        map2.insert("CASHU_ACCEPTED_MINTS".to_owned(), "a,b".to_owned());
        let err2 = Config::from_map(&map2).unwrap_err();
        assert!(
            err2.message.contains("Exactly one CASHU_ACCEPTED_MINTS"),
            "{err2}"
        );
    }

    #[test]
    fn dvm_mode_requires_dvm_kinds() {
        let mut map = HashMap::new();
        map.insert("FEATURE_UPLOAD_ENABLED".to_owned(), "dvm".to_owned());
        let err = Config::from_map(&map).unwrap_err();
        assert!(
            err.message.contains("DVM_ALLOWED_KINDS must be set"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn cleanup_interval_must_be_positive() {
        let mut map = HashMap::new();
        map.insert("CLEANUP_INTERVAL_SECS".to_owned(), "0".to_owned());
        let err = Config::from_map(&map).unwrap_err();
        assert!(
            err.message
                .contains("CLEANUP_INTERVAL_SECS must be greater than zero"),
            "got: {}",
            err.message
        );
    }

    // ── Parsing specific shapes ────────────────────────────────────────────

    #[test]
    fn parse_comma_separated_upstream_servers() {
        let mut map = HashMap::new();
        map.insert(
            "UPSTREAM_SERVERS".to_owned(),
            "https://a.com, https://b.com".to_owned(),
        );
        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.upstream_servers, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn parse_comma_separated_allowed_pubkeys() {
        // Build real npubs rather than inventing bech32: an invalid checksum
        // is skipped, so fabricated strings would make this assert nothing.
        let first = PublicKey::from_hex(&"11".repeat(32)).unwrap();
        let second = PublicKey::from_hex(&"22".repeat(32)).unwrap();
        let mut map = HashMap::new();
        map.insert(
            "ALLOWED_NPUBS".to_owned(),
            format!(
                "{}, {}",
                first.to_bech32().unwrap(),
                second.to_bech32().unwrap()
            ),
        );

        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.allowed_pubkeys, vec![first, second]);
    }

    #[test]
    fn an_unparseable_npub_is_skipped_rather_than_refused() {
        // Preserved from the boot procedure this replaced: a typo shrinks the
        // whitelist, it does not stop the server from starting.
        let valid = PublicKey::from_hex(&"33".repeat(32)).unwrap();
        let mut map = HashMap::new();
        map.insert(
            "ALLOWED_NPUBS".to_owned(),
            format!("not-an-npub, {}", valid.to_bech32().unwrap()),
        );

        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.allowed_pubkeys, vec![valid]);
    }

    #[test]
    fn parse_upstream_mode() {
        let mut map = HashMap::new();
        map.insert("UPSTREAM_MODE".to_owned(), "redirect".to_owned());
        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.upstream_mode, UpstreamMode::Redirect);
    }

    #[test]
    fn parse_feature_mode_dvm() {
        let mut map = HashMap::new();
        map.insert("FEATURE_UPLOAD_ENABLED".to_owned(), "dvm".to_owned());
        map.insert("DVM_ALLOWED_KINDS".to_owned(), "5207".to_owned());
        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.feature_upload_enabled, FeatureMode::Dvm);
        assert_eq!(cfg.dvm_allowed_kinds, vec![5207]);
    }

    #[test]
    fn parse_mebibytes() {
        let mut map = HashMap::new();
        map.insert("MAX_BLOB_SIZE_MB".to_owned(), "42".to_owned());
        // The chunk ceiling defaults to 100 MB, which would exceed a 42 MB
        // blob limit and trip the invariant below.
        map.insert("MAX_CHUNK_SIZE_MB".to_owned(), "10".to_owned());
        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.max_blob_size_bytes, 42 * 1024 * 1024);
    }

    #[test]
    fn optional_stays_none_when_empty() {
        // METRICS_BEARER_TOKEN is unset → None; setting to empty string → still None
        let cfg1 = Config::from_map(&HashMap::new()).unwrap();
        assert!(cfg1.metrics_bearer_token.is_none());

        let mut map = HashMap::new();
        map.insert("METRICS_BEARER_TOKEN".to_owned(), "".to_owned());
        let cfg2 = Config::from_map(&map).unwrap();
        assert!(cfg2.metrics_bearer_token.is_none());

        // Setting it to a real value → Some
        let mut map2 = HashMap::new();
        map2.insert("METRICS_BEARER_TOKEN".to_owned(), "secret".to_owned());
        let cfg3 = Config::from_map(&map2).unwrap();
        assert_eq!(cfg3.metrics_bearer_token.as_deref(), Some("secret"));
    }

    #[test]
    fn public_url_defaults_to_https_when_https_enabled() {
        let mut map = HashMap::new();
        map.insert("ENABLE_HTTPS".to_owned(), "true".to_owned());
        let cfg = Config::from_map(&map).unwrap();
        assert_eq!(cfg.public_url, "https://127.0.0.1:3000");
    }

    #[test]
    fn bind_addr_validation() {
        let mut map = HashMap::new();
        map.insert("BIND_ADDR".to_owned(), "not-a-valid-addr".to_owned());
        let err = Config::from_map(&map).unwrap_err();
        assert!(err.message.contains("BIND_ADDR"), "{err}");
        assert!(err.message.contains("socket address"), "{err}");
    }
}
