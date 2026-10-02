//! Configuration: command line, environment, and dotenv config file.
//!
//! Every setting is one clap argument. Its environment name is derived from
//! the flag (`--upload-access` <-> `ALMOND_UPLOAD_ACCESS`, see [`env_name`]),
//! so the two can never drift apart. Precedence, highest first:
//! command line > process environment > config file > built-in default.
//!
//! Startup order in `main`: [`load_env_file`] (config file and deprecated
//! pre-0.5 names into the process environment, before any thread exists),
//! then [`Config::load`] (parse + cross-field validation).

use std::{ffi::OsString, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use clap::{builder::BoolishValueParser, ArgAction, CommandFactory, FromArgMatches, Parser};
use nostr_sdk::prelude::{FromBech32, PublicKey};

use crate::models::{FeatureMode, ReportAction, UpstreamMode};
use crate::services::cashu::PaidOperation;

/// A configuration error — unreadable config file or cross-field violation.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

const ENV_PREFIX: &str = "ALMOND_";

/// `upload-access` -> `ALMOND_UPLOAD_ACCESS`.
pub(crate) fn env_name(long: &str) -> String {
    format!(
        "{ENV_PREFIX}{}",
        long.to_ascii_uppercase().replace('-', "_")
    )
}

/// `--help` sections: first flag of each group, in declaration order.
const HELP_HEADINGS: &[(&str, &str)] = &[
    ("bind-addr", "Network"),
    ("storage-path", "Storage and retention"),
    ("upload-access", "Access"),
    ("chunk-max-size", "Uploads"),
    ("upstream-servers", "Upstream"),
    ("metrics-token", "Metrics and static files"),
    ("cashu-paid", "Cashu payments (BUD-07)"),
    ("server-list-cache-ttl", "Nostr discovery and filters"),
];

const AFTER_HELP: &str = "\
Every option can also be set as environment variable ALMOND_<OPTION>
(e.g. --upload-access = ALMOND_UPLOAD_ACCESS) or in the --config file.
Precedence: command line > environment > config file > default.

Durations: 0, 30s, 5m, 24h, 7d.  Sizes: 0, 512KiB, 100MiB, 2GiB.
Booleans: true/false, yes/no, on/off, 1/0; a bare flag (--tls-enabled) means true.
Lists: comma-separated, or repeat the flag.
Logging: RUST_LOG=warn or RUST_LOG=almond=debug (default: info).

Examples:
  Public server behind a TLS reverse proxy:
    almond --public-url https://blossom.example.com --storage-path /var/lib/almond
  Private server, only the listed npubs may upload and mirror:
    almond --public-url https://blossom.example.com --allowed-npubs npub1...,npub1...

Terms (Blossom, BUD-xx, WoT, DVM, Cashu) and every setting in detail:
https://github.com/flox1an/almond#readme";

/// Every Almond setting, parsed and validated.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(
    name = "almond",
    version,
    about = "Almond — a Blossom media server",
    after_help = AFTER_HELP
)]
pub struct Config {
    /// dotenv file with ALMOND_* settings; defaults to ./.env if present
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    // ── Network ────────────────────────────────────────────────────────────
    /// Listen address (literal IP:port)
    #[arg(long, value_name = "IP:PORT", default_value = "127.0.0.1:3000")]
    pub bind_addr: SocketAddr,
    /// Public origin used in blob descriptors, no trailing slash;
    /// defaults to http://127.0.0.1:3000 (https:// with --tls-enabled)
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
    /// Origins allowed to read discovery and metrics responses cross-origin
    #[arg(long, value_name = "ORIGIN", value_delimiter = ',', value_parser = trimmed::<String>)]
    pub cors_origins: Vec<String>,
    /// Terminate TLS in Almond itself (leave off behind a TLS proxy)
    #[arg(long, value_name = "BOOL", default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
    pub tls_enabled: bool,
    /// TLS certificate path
    #[arg(long, value_name = "PATH", default_value = "./cert.pem")]
    pub tls_cert: PathBuf,
    /// TLS private key path
    #[arg(long, value_name = "PATH", default_value = "./key.pem")]
    pub tls_key: PathBuf,
    /// Generate a self-signed pair when cert/key are missing (development only)
    #[arg(long, value_name = "BOOL", default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
    pub tls_self_signed: bool,

    // ── Storage and retention ──────────────────────────────────────────────
    /// Storage root (uploads/, upstream-cache/, temp/, quarantine/, reports/)
    #[arg(long, value_name = "PATH", default_value = "./files")]
    pub storage_path: PathBuf,
    /// Total size limit across uploads and upstream cache, 0 = unlimited
    #[arg(long, value_name = "SIZE", default_value = "0", value_parser = parse_size)]
    pub storage_max_size: u64,
    /// Total number of completed blobs, 0 = unlimited
    #[arg(long, value_name = "COUNT", default_value_t = 0)]
    pub storage_max_files: usize,
    /// Refuse writes (HTTP 507) below this much free disk space
    #[arg(long, value_name = "SIZE", default_value = "256MiB", value_parser = parse_size)]
    pub storage_min_free: u64,
    /// Absolute size limit for one blob
    #[arg(long, value_name = "SIZE", default_value = "500MiB", value_parser = parse_size)]
    pub blob_max_size: u64,
    /// Expiry and capacity cleanup cadence (> 0)
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_duration)]
    pub cleanup_interval: Duration,
    /// Maximum age of uploaded blobs, 0 = keep forever
    #[arg(long, value_name = "DURATION", default_value = "0", value_parser = parse_duration)]
    pub upload_max_age: Duration,
    /// Lifetime of transparent upstream cache entries, 0 = no TTL
    #[arg(long, value_name = "DURATION", default_value = "1d", value_parser = parse_duration)]
    pub upstream_cache_ttl: Duration,
    /// S3-compatible endpoint (set all four S3 options or none)
    #[arg(long, value_name = "URL")]
    pub s3_endpoint: Option<String>,
    /// S3 bucket
    #[arg(long, value_name = "NAME")]
    pub s3_bucket: Option<String>,
    /// S3 access key id (prefer env/config file: arguments are visible in ps)
    #[arg(long, value_name = "SECRET")]
    pub s3_access_key_id: Option<String>,
    /// S3 secret access key (prefer env/config file: arguments are visible in ps)
    #[arg(long, value_name = "SECRET")]
    pub s3_secret_access_key: Option<String>,

    // ── Access ─────────────────────────────────────────────────────────────
    /// Who may upload
    #[arg(
        long,
        value_name = "MODE",
        value_enum,
        ignore_case = true,
        default_value = "public"
    )]
    pub upload_access: FeatureMode,
    /// Who may mirror
    #[arg(
        long,
        value_name = "MODE",
        value_enum,
        ignore_case = true,
        default_value = "public"
    )]
    pub mirror_access: FeatureMode,
    /// Who may supply a custom upstream origin
    #[arg(
        long,
        value_name = "MODE",
        value_enum,
        ignore_case = true,
        default_value = "off"
    )]
    pub custom_origin_access: FeatureMode,
    /// Who may file BUD-09 reports
    #[arg(
        long,
        value_name = "MODE",
        value_enum,
        ignore_case = true,
        default_value = "off"
    )]
    pub report_access: FeatureMode,
    /// What a report does to a local blob
    #[arg(
        long,
        value_name = "ACTION",
        value_enum,
        ignore_case = true,
        default_value = "quarantine"
    )]
    pub report_action: ReportAction,
    /// Serve /list
    #[arg(long, value_name = "BOOL", default_value_t = true, num_args = 0..=1, default_missing_value = "true", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
    pub list_enabled: bool,
    /// Serve the homepage
    #[arg(long, value_name = "BOOL", default_value_t = true, num_args = 0..=1, default_missing_value = "true", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
    pub homepage_enabled: bool,
    /// Whitelisted npubs; also the web-of-trust roots
    #[arg(long, value_name = "NPUB", value_delimiter = ',', value_parser = parse_npub)]
    pub allowed_npubs: Vec<PublicKey>,
    /// Longest accepted authorization token lifetime (> 0)
    #[arg(long, value_name = "DURATION", default_value = "24h", value_parser = parse_duration)]
    pub auth_max_ttl: Duration,
    /// Tolerated clock skew for authorization events
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_duration)]
    pub auth_clock_skew: Duration,
    /// Require a matching server tag in authorization events
    #[arg(long, value_name = "BOOL", default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
    pub auth_require_server_tag: bool,

    // ── Uploads ────────────────────────────────────────────────────────────
    /// Size limit for one resumable-upload chunk (<= --blob-max-size)
    #[arg(long, value_name = "SIZE", default_value = "100MiB", value_parser = parse_size)]
    pub chunk_max_size: u64,
    /// Abandoned resumable uploads are removed after this long
    #[arg(long, value_name = "DURATION", default_value = "30m", value_parser = parse_duration)]
    pub chunk_session_timeout: Duration,
    /// Concurrent resumable upload sessions
    #[arg(long, value_name = "COUNT", default_value_t = 128)]
    pub chunk_max_sessions: usize,
    /// Concurrent resumable upload sessions per pubkey
    #[arg(long, value_name = "COUNT", default_value_t = 8)]
    pub chunk_max_sessions_per_pubkey: usize,
    /// Parallel segment fetches for an explicit HLS mirror
    #[arg(long, value_name = "COUNT", default_value_t = 4)]
    pub hls_mirror_concurrency: usize,

    // ── Upstream ───────────────────────────────────────────────────────────
    /// Trusted upstream Blossom origins
    #[arg(long, value_name = "URL", value_delimiter = ',', value_parser = trimmed::<String>)]
    pub upstream_servers: Vec<String>,
    /// How upstream blobs are delivered
    #[arg(
        long,
        value_name = "MODE",
        value_enum,
        ignore_case = true,
        default_value = "proxy"
    )]
    pub upstream_mode: UpstreamMode,
    /// Largest upstream blob that is cached (larger ones are only proxied)
    #[arg(long, value_name = "SIZE", default_value = "100MiB", value_parser = parse_size)]
    pub upstream_max_download_size: u64,

    // ── Metrics and static files ───────────────────────────────────────────
    /// Bearer token for /metrics; /metrics is disabled while unset
    #[arg(long, value_name = "SECRET")]
    pub metrics_token: Option<String>,
    /// Directory of static files served alongside blobs
    #[arg(long, value_name = "PATH")]
    pub serve_files_path: Option<PathBuf>,
    /// Manifest fallback directory when --serve-files-path is read-only
    #[arg(long, value_name = "PATH", default_value = "/tmp")]
    pub serve_files_manifest_dir: PathBuf,
    /// Manifest file name
    #[arg(long, value_name = "NAME", default_value = "manifest-sha256.txt")]
    pub serve_files_manifest_name: String,
    /// Manifest rebuild interval
    #[arg(long, value_name = "DURATION", default_value = "1h", value_parser = parse_duration)]
    pub serve_files_refresh_interval: Duration,

    // ── Cashu payments (BUD-07) ────────────────────────────────────────────
    /// Operations that require payment
    #[arg(
        long,
        value_name = "OP",
        value_enum,
        ignore_case = true,
        value_delimiter = ','
    )]
    pub cashu_paid: Vec<PaidOperation>,
    /// Price in sats per MiB
    #[arg(long, value_name = "SATS", default_value_t = 1)]
    pub cashu_price_per_mib: u64,
    /// The one accepted mint; required when --cashu-paid is set
    #[arg(long, value_name = "URL")]
    pub cashu_mint: Option<String>,
    /// Wallet database path
    #[arg(long, value_name = "PATH", default_value = "./cashu_wallet.db")]
    pub cashu_wallet_path: PathBuf,

    // ── Nostr discovery and filters ────────────────────────────────────────
    /// Cache lifetime of an author's Blossom server list
    #[arg(long, value_name = "DURATION", default_value = "24h", value_parser = parse_duration)]
    pub server_list_cache_ttl: Duration,
    /// BUD-11 filter algorithm
    #[arg(long, value_name = "ALGORITHM", default_value = "binary-fuse-16", value_parser = ["bloom", "binary-fuse-8", "binary-fuse-16", "binary-fuse-32"])]
    pub filter_algorithm: String,
    /// DVM kinds admitted by dvm access mode; required when upload or mirror access is dvm
    #[arg(long, value_name = "KIND", value_delimiter = ',', value_parser = trimmed::<u16>)]
    pub dvm_kinds: Vec<u16>,
    /// Relays queried for DVM announcements
    #[arg(long, value_name = "URL", value_delimiter = ',', value_parser = trimmed::<String>)]
    pub dvm_relays: Vec<String>,
    /// DVM announcement refresh interval
    #[arg(long, value_name = "DURATION", default_value = "5m", value_parser = parse_duration)]
    pub dvm_refresh_interval: Duration,
}

impl Config {
    /// Parse the real command line and process environment.
    ///
    /// Prints help/version or a clap usage error and exits on its own;
    /// returns `Err` only for cross-field violations.
    pub fn load() -> Result<Self, ConfigError> {
        let matches = Self::command_with_env().get_matches();
        Self::from_arg_matches(&matches)
            .map_err(|e| ConfigError::new(e.to_string()))?
            .validate()
    }

    /// The clap command with every argument bound to its `ALMOND_*` variable.
    fn command_with_env() -> clap::Command {
        let mut heading = None;
        Self::command().mut_args(move |arg| {
            let Some(long) = arg.get_long() else {
                return arg;
            };
            let name = env_name(long);
            if let Some((_, h)) = HELP_HEADINGS.iter().find(|(first, _)| *first == long) {
                heading = Some(*h);
            }
            arg.env(name).hide_env_values(true).help_heading(heading)
        })
    }

    /// The default `public_url` depends on `tls_enabled`.
    #[must_use]
    pub fn public_url(&self) -> String {
        self.public_url.clone().unwrap_or_else(|| {
            let scheme = if self.tls_enabled { "https" } else { "http" };
            format!("{scheme}://127.0.0.1:3000")
        })
    }

    fn validate(mut self) -> Result<Self, ConfigError> {
        let s3 = [
            &self.s3_endpoint,
            &self.s3_bucket,
            &self.s3_access_key_id,
            &self.s3_secret_access_key,
        ];
        let s3_set = s3.iter().filter(|o| o.is_some()).count();
        if s3_set != 0 && s3_set != s3.len() {
            return Err(ConfigError::new(
                "Incomplete S3 configuration: ALMOND_S3_ENDPOINT, ALMOND_S3_BUCKET, \
                 ALMOND_S3_ACCESS_KEY_ID and ALMOND_S3_SECRET_ACCESS_KEY must be set together",
            ));
        }
        if self.cleanup_interval.is_zero() {
            return Err(ConfigError::new(
                "ALMOND_CLEANUP_INTERVAL must be greater than zero",
            ));
        }
        if self.auth_max_ttl.is_zero() {
            return Err(ConfigError::new(
                "ALMOND_AUTH_MAX_TTL must be greater than zero",
            ));
        }
        if self.chunk_max_size > self.blob_max_size {
            return Err(ConfigError::new(
                "ALMOND_CHUNK_MAX_SIZE must not exceed ALMOND_BLOB_MAX_SIZE",
            ));
        }
        if !self.cashu_paid.is_empty()
            && !self.cashu_mint.as_deref().is_some_and(|m| !m.contains(','))
        {
            return Err(ConfigError::new(
                "ALMOND_CASHU_PAID requires exactly one ALMOND_CASHU_MINT",
            ));
        }
        if (self.upload_access.requires_dvm() || self.mirror_access.requires_dvm())
            && self.dvm_kinds.is_empty()
        {
            return Err(ConfigError::new(
                "ALMOND_DVM_KINDS must be set when any access mode is 'dvm'",
            ));
        }
        // A blank token would enable /metrics behind an empty secret.
        self.metrics_token = self.metrics_token.filter(|t| !t.trim().is_empty());
        Ok(self)
    }

    /// Parse from `ALMOND_*` pairs without touching the process environment.
    /// Empty values count as unset, exactly as for real env vars.
    #[cfg(test)]
    pub fn from_map(map: &std::collections::HashMap<String, String>) -> Result<Self, ConfigError> {
        let mut argv = vec![OsString::from("almond")];
        for (name, value) in map.iter().filter(|(_, v)| !v.is_empty()) {
            let flag = name
                .strip_prefix(ENV_PREFIX)
                .ok_or_else(|| ConfigError::new(format!("not an ALMOND_ name: {name}")))?
                .to_ascii_lowercase()
                .replace('_', "-");
            argv.push(format!("--{flag}={value}").into());
        }
        let matches = Self::command()
            .try_get_matches_from(argv)
            .map_err(|e| ConfigError::new(e.to_string()))?;
        Self::from_arg_matches(&matches)
            .map_err(|e| ConfigError::new(e.to_string()))?
            .validate()
    }
}

// ---------------------------------------------------------------------------
// Config file and deprecated names
// ---------------------------------------------------------------------------

/// Load the config file into the process environment, then translate
/// deprecated pre-0.5 names. Real env vars always win over the file;
/// an empty `ALMOND_*` value counts as unset everywhere.
///
/// Must run before any other thread exists (it calls `set_var`).
/// Returns deprecation warnings to log once tracing is up.
pub fn load_env_file() -> Result<Vec<String>, ConfigError> {
    remove_empty_almond_vars();
    let path = config_path_from_args(std::env::args_os())
        .or_else(|| std::env::var_os(env_name("config")).map(PathBuf::from))
        .or_else(|| Some(PathBuf::from(".env")).filter(|p| p.exists()));
    if let Some(path) = path {
        dotenvy::from_path(&path).map_err(|e| {
            ConfigError::new(format!("Cannot load config file {}: {e}", path.display()))
        })?;
    }
    remove_empty_almond_vars();

    let (translated, warnings) = translate_legacy(|name| std::env::var(name).ok());
    for (name, value) in translated {
        std::env::set_var(name, value);
    }
    Ok(warnings)
}

/// clap would reject `ALMOND_X=` as "value required"; treat it as unset.
fn remove_empty_almond_vars() {
    for (name, value) in std::env::vars_os() {
        if value.is_empty() && name.to_str().is_some_and(|n| n.starts_with(ENV_PREFIX)) {
            std::env::remove_var(name);
        }
    }
}

/// `--config FILE` / `--config=FILE`, found before clap runs because the
/// file has to be in the environment by then.
fn config_path_from_args(args: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--config=")) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

enum Legacy {
    /// Same value under the new name.
    Same,
    /// Old value was a bare number in this unit; append it.
    Unit(&'static str),
    /// Old feature switch also accepted true/false.
    Mode,
}

// ponytail: deprecated aliases for pre-0.5 deployments; delete this table
// and `translate_legacy` once those are migrated.
const LEGACY: &[(&str, &str, Legacy)] = &[
    ("BIND_ADDR", "BIND_ADDR", Legacy::Same),
    ("PUBLIC_URL", "PUBLIC_URL", Legacy::Same),
    ("CORS_ALLOWED_ORIGINS", "CORS_ORIGINS", Legacy::Same),
    ("ENABLE_HTTPS", "TLS_ENABLED", Legacy::Same),
    ("TLS_CERT_PATH", "TLS_CERT", Legacy::Same),
    ("TLS_KEY_PATH", "TLS_KEY", Legacy::Same),
    ("TLS_AUTO_GENERATE", "TLS_SELF_SIGNED", Legacy::Same),
    ("STORAGE_PATH", "STORAGE_PATH", Legacy::Same),
    ("MAX_TOTAL_SIZE", "STORAGE_MAX_SIZE", Legacy::Unit("MiB")),
    ("MAX_TOTAL_FILES", "STORAGE_MAX_FILES", Legacy::Same),
    ("MIN_FREE_DISK_MB", "STORAGE_MIN_FREE", Legacy::Unit("MiB")),
    ("MAX_BLOB_SIZE_MB", "BLOB_MAX_SIZE", Legacy::Unit("MiB")),
    (
        "CLEANUP_INTERVAL_SECS",
        "CLEANUP_INTERVAL",
        Legacy::Unit("s"),
    ),
    ("MAX_FILE_AGE_DAYS", "UPLOAD_MAX_AGE", Legacy::Unit("d")),
    (
        "MAX_UPSTREAM_CACHE_TTL_DAYS",
        "UPSTREAM_CACHE_TTL",
        Legacy::Unit("d"),
    ),
    ("FEATURE_UPLOAD_ENABLED", "UPLOAD_ACCESS", Legacy::Mode),
    ("FEATURE_MIRROR_ENABLED", "MIRROR_ACCESS", Legacy::Mode),
    (
        "FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED",
        "CUSTOM_ORIGIN_ACCESS",
        Legacy::Mode,
    ),
    ("FEATURE_REPORT_ENABLED", "REPORT_ACCESS", Legacy::Mode),
    ("REPORT_ACTION", "REPORT_ACTION", Legacy::Same),
    ("FEATURE_LIST_ENABLED", "LIST_ENABLED", Legacy::Same),
    ("FEATURE_HOMEPAGE_ENABLED", "HOMEPAGE_ENABLED", Legacy::Same),
    ("ALLOWED_NPUBS", "ALLOWED_NPUBS", Legacy::Same),
    ("AUTH_MAX_TTL_SECS", "AUTH_MAX_TTL", Legacy::Unit("s")),
    ("AUTH_CLOCK_SKEW_SECS", "AUTH_CLOCK_SKEW", Legacy::Unit("s")),
    (
        "AUTH_REQUIRE_SERVER_TAG",
        "AUTH_REQUIRE_SERVER_TAG",
        Legacy::Same,
    ),
    ("MAX_CHUNK_SIZE_MB", "CHUNK_MAX_SIZE", Legacy::Unit("MiB")),
    (
        "CHUNK_CLEANUP_TIMEOUT_MINUTES",
        "CHUNK_SESSION_TIMEOUT",
        Legacy::Unit("m"),
    ),
    (
        "MAX_CHUNK_UPLOAD_SESSIONS",
        "CHUNK_MAX_SESSIONS",
        Legacy::Same,
    ),
    (
        "MAX_CHUNK_UPLOAD_SESSIONS_PER_PUBKEY",
        "CHUNK_MAX_SESSIONS_PER_PUBKEY",
        Legacy::Same,
    ),
    (
        "HLS_MIRROR_CONCURRENCY",
        "HLS_MIRROR_CONCURRENCY",
        Legacy::Same,
    ),
    ("UPSTREAM_SERVERS", "UPSTREAM_SERVERS", Legacy::Same),
    ("UPSTREAM_MODE", "UPSTREAM_MODE", Legacy::Same),
    (
        "MAX_UPSTREAM_DOWNLOAD_SIZE_MB",
        "UPSTREAM_MAX_DOWNLOAD_SIZE",
        Legacy::Unit("MiB"),
    ),
    ("METRICS_BEARER_TOKEN", "METRICS_TOKEN", Legacy::Same),
    ("SERVE_FILES_PATH", "SERVE_FILES_PATH", Legacy::Same),
    (
        "SERVE_FILES_MANIFEST_DIR",
        "SERVE_FILES_MANIFEST_DIR",
        Legacy::Same,
    ),
    (
        "SERVE_FILES_MANIFEST_NAME",
        "SERVE_FILES_MANIFEST_NAME",
        Legacy::Same,
    ),
    (
        "SERVE_FILES_REFRESH_INTERVAL_SECS",
        "SERVE_FILES_REFRESH_INTERVAL",
        Legacy::Unit("s"),
    ),
    ("CASHU_PRICE_PER_MB", "CASHU_PRICE_PER_MIB", Legacy::Same),
    ("CASHU_ACCEPTED_MINTS", "CASHU_MINT", Legacy::Same),
    ("CASHU_WALLET_PATH", "CASHU_WALLET_PATH", Legacy::Same),
    (
        "BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS",
        "SERVER_LIST_CACHE_TTL",
        Legacy::Unit("h"),
    ),
    ("FILTER_ALGORITHM", "FILTER_ALGORITHM", Legacy::Same),
    ("DVM_ALLOWED_KINDS", "DVM_KINDS", Legacy::Same),
    ("DVM_RELAYS", "DVM_RELAYS", Legacy::Same),
    (
        "DVM_REFRESH_INTERVAL_MINS",
        "DVM_REFRESH_INTERVAL",
        Legacy::Unit("m"),
    ),
];

const LEGACY_PAID: [(&str, &str); 3] = [
    ("FEATURE_PAID_UPLOAD", "upload"),
    ("FEATURE_PAID_MIRROR", "mirror"),
    ("FEATURE_PAID_DOWNLOAD", "download"),
];

/// New `(ALMOND_* name, value)` pairs for every set deprecated name whose
/// new name is unset, plus one warning per deprecated name seen.
fn translate_legacy(get: impl Fn(&str) -> Option<String>) -> (Vec<(String, String)>, Vec<String>) {
    let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());

    // (old name, new name, translated value)
    let mut found: Vec<(&str, String, String)> = Vec::new();
    for (old, new, kind) in LEGACY {
        let Some(value) = get(old) else { continue };
        let value = value.trim();
        let value = match kind {
            Legacy::Same => value.to_owned(),
            Legacy::Unit(unit) => format!("{value}{unit}"),
            Legacy::Mode => match value.to_ascii_lowercase().as_str() {
                "true" => "public".to_owned(),
                "false" => "off".to_owned(),
                _ => value.to_owned(),
            },
        };
        found.push((old, format!("{ENV_PREFIX}{new}"), value));
    }
    let paid: Vec<(&str, String)> = LEGACY_PAID
        .iter()
        .filter_map(|(old, op)| get(old).map(|v| (*op, v)))
        .collect();
    if !paid.is_empty() {
        let ops: Vec<&str> = paid
            .iter()
            .filter(|(_, v)| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "on"))
            .map(|(op, _)| *op)
            .collect();
        found.push(("FEATURE_PAID_*", env_name("cashu-paid"), ops.join(",")));
    }

    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (old, new, value) in found {
        if get(&new).is_some() {
            warnings.push(format!(
                "{old} is deprecated and ignored because {new} is set"
            ));
        } else if value.is_empty() {
            // Only possible for FEATURE_PAID_* that are all off.
            warnings.push(format!("{old} is deprecated, use {new}"));
        } else {
            warnings.push(format!("{old} is deprecated, use {new}={value}"));
            out.push((new, value));
        }
    }
    (out, warnings)
}

// ---------------------------------------------------------------------------
// Value parsers
// ---------------------------------------------------------------------------

/// List element: surrounding whitespace ignored, empty elements rejected.
fn trimmed<T: FromStr>(s: &str) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    let s = s.trim();
    if s.is_empty() {
        return Err("empty list element".to_owned());
    }
    s.parse().map_err(|e: T::Err| e.to_string())
}

fn parse_npub(s: &str) -> Result<PublicKey, String> {
    PublicKey::from_bech32(s.trim()).map_err(|e| format!("invalid npub: {e}"))
}

/// `0` or `<digits><unit>`; returns the number times the unit's factor.
fn parse_with_unit(s: &str, units: &[(&str, u64)], example: &str) -> Result<u64, String> {
    let s = s.trim();
    if s == "0" {
        return Ok(0);
    }
    let unit_names = units.iter().map(|(u, _)| *u).collect::<Vec<_>>().join(", ");
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("missing unit ({unit_names}), e.g. {example}"))?;
    let (number, unit) = s.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("expected <number><unit>, e.g. {example}"))?;
    let factor = units
        .iter()
        .find(|(u, _)| u.eq_ignore_ascii_case(unit))
        .map(|(_, f)| *f)
        .ok_or_else(|| format!("unknown unit '{unit}', use one of {unit_names}"))?;
    number
        .checked_mul(factor)
        .ok_or_else(|| "value too large".to_owned())
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    const UNITS: &[(&str, u64)] = &[("s", 1), ("m", 60), ("h", 3600), ("d", 86_400)];
    // `5M` could mean months; only the lowercase spelling is accepted.
    if s.chars().any(|c| c.is_ascii_uppercase()) {
        return Err("duration units are lowercase: s, m, h, d".to_owned());
    }
    parse_with_unit(s, UNITS, "30s").map(Duration::from_secs)
}

fn parse_size(s: &str) -> Result<u64, String> {
    const UNITS: &[(&str, u64)] = &[
        ("B", 1),
        ("KiB", 1 << 10),
        ("MiB", 1 << 20),
        ("GiB", 1 << 30),
        ("TiB", 1 << 40),
    ];
    parse_with_unit(s, UNITS, "100MiB")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use nostr_sdk::prelude::ToBech32;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<HashMap<_, _>>();
        Config::from_map(&map)
    }

    fn err(pairs: &[(&str, &str)]) -> String {
        cfg(pairs).unwrap_err().message
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.bind_addr.to_string(), "127.0.0.1:3000");
        assert_eq!(c.public_url(), "http://127.0.0.1:3000");
        assert_eq!(c.storage_max_size, 0);
        assert_eq!(c.blob_max_size, 500 << 20);
        assert_eq!(c.cleanup_interval, Duration::from_secs(30));
        assert_eq!(c.upload_max_age, Duration::ZERO);
        assert_eq!(c.upstream_cache_ttl, Duration::from_secs(86_400));
        assert_eq!(c.auth_max_ttl, Duration::from_secs(86_400));
        assert_eq!(c.upload_access, FeatureMode::Public);
        assert_eq!(c.custom_origin_access, FeatureMode::Off);
        assert_eq!(c.report_action, ReportAction::Quarantine);
        assert_eq!(c.upstream_mode, UpstreamMode::Proxy);
        assert!(c.list_enabled && c.homepage_enabled);
        assert!(!c.tls_enabled && !c.tls_self_signed);
        assert!(c.cashu_paid.is_empty());
    }

    #[test]
    fn cross_field_rules_are_startup_errors() {
        assert!(err(&[("ALMOND_CHUNK_MAX_SIZE", "600MiB")]).contains("ALMOND_CHUNK_MAX_SIZE"));
        assert!(err(&[("ALMOND_CASHU_PAID", "upload")]).contains("ALMOND_CASHU_MINT"));
        assert!(err(&[
            ("ALMOND_CASHU_PAID", "upload"),
            ("ALMOND_CASHU_MINT", "a,b")
        ])
        .contains("exactly one"));
        assert!(err(&[("ALMOND_UPLOAD_ACCESS", "dvm")]).contains("ALMOND_DVM_KINDS"));
        assert!(err(&[("ALMOND_CLEANUP_INTERVAL", "0")]).contains("ALMOND_CLEANUP_INTERVAL"));
        assert!(err(&[("ALMOND_AUTH_MAX_TTL", "0")]).contains("ALMOND_AUTH_MAX_TTL"));
        assert!(err(&[("ALMOND_S3_BUCKET", "b")]).contains("Incomplete S3"));
        assert!(cfg(&[
            ("ALMOND_CASHU_PAID", "upload,download"),
            ("ALMOND_CASHU_MINT", "https://m")
        ])
        .is_ok());
    }

    #[test]
    fn unknown_values_are_rejected_not_defaulted() {
        for (name, value) in [
            ("ALMOND_UPLOAD_ACCESS", "pubic"),
            ("ALMOND_UPSTREAM_MODE", "redirekt"),
            ("ALMOND_REPORT_ACTION", "nuke"),
            ("ALMOND_FILTER_ALGORITHM", "xor"),
            ("ALMOND_LIST_ENABLED", "flase"),
            ("ALMOND_ALLOWED_NPUBS", "not-an-npub"),
            ("ALMOND_BIND_ADDR", "localhost:3000"),
        ] {
            assert!(
                cfg(&[(name, value)]).is_err(),
                "{name}={value} was accepted"
            );
        }
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert!(parse_duration("30").is_err(), "bare number has no unit");
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("5M").is_err(), "minutes or months?");
        assert!(parse_duration("h").is_err());
        assert_eq!(parse_size("512KiB"), Ok(512 << 10));
        assert_eq!(parse_size("2gib"), Ok(2 << 30));
        assert!(parse_size("100MB").is_err(), "decimal units are ambiguous");
        assert!(parse_size("99999999999TiB").is_err());
    }

    #[test]
    fn lists_trim_and_parse_each_element() {
        let first = PublicKey::from_hex(&"11".repeat(32)).unwrap();
        let second = PublicKey::from_hex(&"22".repeat(32)).unwrap();
        let npubs = format!(
            "{}, {}",
            first.to_bech32().unwrap(),
            second.to_bech32().unwrap()
        );
        let c = cfg(&[
            ("ALMOND_ALLOWED_NPUBS", &npubs),
            ("ALMOND_UPSTREAM_SERVERS", "https://a.com, https://b.com"),
            ("ALMOND_DVM_KINDS", "5207, 5208"),
        ])
        .unwrap();
        assert_eq!(c.allowed_npubs, vec![first, second]);
        assert_eq!(c.upstream_servers, vec!["https://a.com", "https://b.com"]);
        assert_eq!(c.dvm_kinds, vec![5207, 5208]);
    }

    #[test]
    fn https_changes_public_url_default_and_blank_token_disables_metrics() {
        assert_eq!(
            cfg(&[("ALMOND_TLS_ENABLED", "true")]).unwrap().public_url(),
            "https://127.0.0.1:3000"
        );
        assert!(cfg(&[("ALMOND_METRICS_TOKEN", "  ")])
            .unwrap()
            .metrics_token
            .is_none());
    }

    #[test]
    fn config_path_is_found_in_both_flag_spellings() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            config_path_from_args(args(&["almond", "--config", "a.env"])),
            Some("a.env".into())
        );
        assert_eq!(
            config_path_from_args(args(&["almond", "--config=b.env"])),
            Some("b.env".into())
        );
        assert_eq!(
            config_path_from_args(args(&["almond", "--bind-addr", "x"])),
            None
        );
    }

    #[test]
    fn legacy_names_translate_and_never_override_new_names() {
        let old: HashMap<&str, &str> = [
            ("MAX_BLOB_SIZE_MB", "42"),
            ("CLEANUP_INTERVAL_SECS", "10"),
            ("FEATURE_UPLOAD_ENABLED", "true"),
            ("FEATURE_PAID_UPLOAD", "on"),
            ("FEATURE_PAID_MIRROR", "off"),
            ("FEATURE_PAID_DOWNLOAD", "1"),
            ("UPSTREAM_MODE", "redirect"),
            ("ALMOND_UPSTREAM_MODE", "proxy"),
            ("DVM_RELAYS", ""),
        ]
        .into();
        let (out, warnings) = translate_legacy(|n| old.get(n).map(|v| (*v).to_owned()));
        let out: HashMap<String, String> = out.into_iter().collect();
        assert_eq!(out["ALMOND_BLOB_MAX_SIZE"], "42MiB");
        assert_eq!(out["ALMOND_CLEANUP_INTERVAL"], "10s");
        assert_eq!(out["ALMOND_UPLOAD_ACCESS"], "public");
        assert_eq!(out["ALMOND_CASHU_PAID"], "upload,download");
        assert!(!out.contains_key("ALMOND_UPSTREAM_MODE"), "new name wins");
        assert!(
            !out.contains_key("ALMOND_DVM_RELAYS"),
            "empty old value is unset"
        );
        assert!(warnings
            .iter()
            .any(|w| w.contains("UPSTREAM_MODE is deprecated and ignored")));
        assert_eq!(warnings.len(), 5);
    }
}
