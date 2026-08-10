// Crate-level lint overrides. Deliberate casting choices acknowledged;
// promote cast lints back to `deny` once all refactors are complete.
// `uninlined_format_args`: 500+ tracing calls with emoji prefixes — a bulk
// clippy --fix would produce a 2000-line diff with zero behavioural change.
#![allow(
    clippy::uninlined_format_args,
    clippy::let_underscore_must_use,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::large_enum_variant,
    clippy::type_complexity,
    clippy::struct_excessive_bools,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::implicit_hasher,
    clippy::needless_pass_by_value,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::doc_markdown,
    clippy::must_use_candidate
)]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod constants;
pub mod error;
pub mod handlers;
pub mod helpers;
pub mod metrics;
pub mod middleware;
pub mod models;
pub mod services;
pub mod tls;
pub mod trust_network;
pub mod utils;

use std::{collections::HashMap, env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::signal;

use crate::models::AppState;
use crate::trust_network::{refresh_dvm_pubkeys, refresh_trust_network};
use crate::utils::{
    build_file_index, cleanup_abandoned_chunks, cleanup_expired_blossom_server_lists,
    cleanup_expired_failed_lookups, enforce_storage_limits, initialize_storage,
    migrate_legacy_blobs,
};
use axum::Router;
use dotenvy::dotenv;
use nostr_relay_pool::prelude::*;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use axum::{
    extract::State,
    http::StatusCode,
    middleware::from_fn_with_state,
    routing::{delete, get, put},
};
use handlers::{
    delete_blob, get_filter, get_metrics, get_upstream, get_wot, handle_file_request, list_blobs,
    mirror_blob, patch_upload, report_blob, upload_file,
};
use middleware::cors_middleware;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

// HEAD /upload handler for price discovery (BUD-07)
async fn head_upload(
    State(state): State<AppState>,
) -> Result<axum::response::Response<axum::body::Body>, StatusCode> {
    use axum::http::header;
    use axum::response::Response;

    // Build response with server capabilities
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT, "application/octet-stream");

    // Add payment info if paid uploads are enabled
    if state.feature_paid_upload {
        builder = builder
            .header("X-Price-Per-MB", state.cashu_price_per_mb.to_string())
            .header("X-Price-Unit", "sat")
            .header("X-Accepted-Mints", state.cashu_accepted_mints.join(","));
    }

    builder
        .body(axum::body::Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn options_upload() -> &'static str {
    "Method not allowed"
}

async fn serve_index(
    State(state): State<AppState>,
) -> Result<axum::response::Response<axum::body::Body>, StatusCode> {
    use axum::{http::header, response::Response};

    // Check if homepage feature is enabled
    if !state.feature_homepage_enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(include_str!("index.html")))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn serve_filter_test() -> Result<axum::response::Response<axum::body::Body>, StatusCode> {
    use axum::{http::header, response::Response};

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(include_str!("filter-test.html")))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn create_app(state: AppState) -> Router {
    let max_blob_size = usize::try_from(state.max_blob_size_bytes)
        .expect("MAX_BLOB_SIZE_MB does not fit the platform request-body limit");
    Router::new()
        .route(
            "/upload",
            put(upload_file)
                .head(head_upload)
                .options(options_upload)
                .patch(patch_upload),
        )
        .route("/list", get(list_blobs))
        .route("/list/{id}", get(list_blobs))
        .route(
            "/mirror",
            put(mirror_blob).layer(RequestBodyLimitLayer::new(64 * 1024)),
        )
        .route("/_wot", get(get_wot))
        .route("/report", put(report_blob))
        .route("/filter", get(get_filter))
        .route("/_upstream", get(get_upstream))
        .route("/_metrics", get(get_metrics))
        .route("/metrics", get(get_metrics))
        .route("/", get(serve_index))
        .route("/index.html", get(serve_index))
        .route("/filter-test.html", get(serve_filter_test))
        .route("/{filename}", delete(delete_blob))
        .route(
            "/{filename}",
            get(handle_file_request).head(handle_file_request),
        )
        .layer(RequestBodyLimitLayer::new(max_blob_size))
        .layer(from_fn_with_state(state.clone(), cors_middleware))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .layer(ConcurrencyLimitLayer::new(256))
        .with_state(state)
}

/// Clear temp directory recursively, removing all files and subdirectories
async fn clear_temp_directory(temp_dir: &PathBuf) -> Result<(), std::io::Error> {
    if !temp_dir.exists() {
        return Ok(());
    }

    let mut entries = fs::read_dir(temp_dir).await?;
    let mut removed_count = 0;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();

        if path.is_dir() {
            // Recursively remove directory
            fs::remove_dir_all(&path).await?;
            removed_count += 1;
            info!("🗑️  Removed temp directory: {}", path.display());
        } else if path.is_file() {
            // Remove file
            fs::remove_file(&path).await?;
            removed_count += 1;
            info!("🗑️  Removed temp file: {}", path.display());
        }
    }

    if removed_count > 0 {
        info!("✅ Cleared {} items from temp directory", removed_count);
    }

    Ok(())
}

async fn load_app_state() -> AppState {
    dotenv().ok();

    let max_total_size = env::var("MAX_TOTAL_SIZE")
        .unwrap_or_else(|_| "99999".to_string())
        .parse::<u64>()
        .expect("Invalid value for MAX_TOTAL_SIZE")
        .checked_mul(1024 * 1024)
        .expect("MAX_TOTAL_SIZE value too large");

    let max_total_files = env::var("MAX_TOTAL_FILES")
        .unwrap_or_else(|_| "99999999".to_string())
        .parse::<usize>()
        .expect("Invalid value for MAX_TOTAL_FILES");

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());

    // HTTPS/TLS configuration
    let enable_https = env::var("ENABLE_HTTPS")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    let tls_cert_path =
        PathBuf::from(env::var("TLS_CERT_PATH").unwrap_or_else(|_| "./cert.pem".to_string()));

    let tls_key_path =
        PathBuf::from(env::var("TLS_KEY_PATH").unwrap_or_else(|_| "./key.pem".to_string()));

    let tls_auto_generate = env::var("TLS_AUTO_GENERATE")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    let public_url = env::var("PUBLIC_URL").unwrap_or_else(|_| {
        if enable_https {
            "https://127.0.0.1:3000".to_string()
        } else {
            "http://127.0.0.1:3000".to_string()
        }
    });
    let cors_allowed_origins = env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();

    // Storage uses explicit roots so completed uploads and upstream cache
    // entries cannot be reconstructed into the same namespace.
    let storage_path = env::var("STORAGE_PATH").unwrap_or_else(|_| "./files".to_string());
    let storage = models::StorageLayout::new(PathBuf::from(&storage_path));
    initialize_storage(&storage)
        .await
        .unwrap_or_else(|error| panic!("Failed to initialize storage layout: {error}"));
    info!("⚙️ Storage path: {}", storage.root.display());
    let native_s3 = match services::native_storage::S3Settings::from_env() {
        Ok(Some(settings)) => {
            info!("S3 native storage enabled for bucket {}", settings.bucket);
            Some(Arc::new(
                services::native_storage::NativeS3Storage::connect(settings).await,
            ))
        }
        Ok(None) => None,
        Err(message) => {
            error!("{message}");
            std::process::exit(1);
        }
    };

    // Clear temp directory on startup.
    if storage.temp.exists() {
        info!(
            "🧹 Clearing temp directory on startup: {}",
            storage.temp.display()
        );
        if let Err(error) = clear_temp_directory(&storage.temp).await {
            error!(
                "⚠️  Failed to clear temp directory {}: {}",
                storage.temp.display(),
                error
            );
            warn!("⚠️  Continuing startup with existing temp files (they may be orphaned)");
        }
    }

    migrate_legacy_blobs(&storage)
        .await
        .unwrap_or_else(|error| panic!("Failed to migrate legacy storage: {error}"));
    let file_index = Arc::new(services::blob_index::BlobIndex::new());
    build_file_index(&storage, &file_index)
        .await
        .unwrap_or_else(|error| panic!("Failed to reconstruct blob index: {error}"));
    if let Some(s3) = &native_s3 {
        for (sha256, metadata) in s3
            .list_all()
            .await
            .unwrap_or_else(|error| panic!("Failed to build S3 blob index: {error}"))
        {
            // Local uploads retain precedence over same-hash S3 uploads.
            if !file_index.contains(&sha256).await {
                file_index.insert(sha256, metadata).await;
            }
        }
    }

    let serve_file_index = Arc::new(RwLock::new(HashMap::new()));
    let serve_files_path = env::var("SERVE_FILES_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let serve_files_manifest_name = env::var("SERVE_FILES_MANIFEST_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "manifest-sha256.txt".to_string());
    let serve_files_refresh_interval_secs = env::var("SERVE_FILES_REFRESH_INTERVAL_SECS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse()
        .expect("Invalid value for SERVE_FILES_REFRESH_INTERVAL_SECS");

    if let Some(path) = &serve_files_path {
        info!(
            "📁 Serve files enabled: {} (manifest: {}, refresh: {}s)",
            path.display(),
            serve_files_manifest_name,
            serve_files_refresh_interval_secs
        );

        if let Err(e) = services::serve_files::refresh_serve_file_index(
            path,
            &serve_files_manifest_name,
            &serve_file_index,
        )
        .await
        {
            warn!(
                "⚠️ Failed to build serve files index for {}: {}",
                path.display(),
                e
            );
        }
    }

    let cleanup_interval_secs: u64 = env::var("CLEANUP_INTERVAL_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .expect("Invalid value for CLEANUP_INTERVAL_SECS");
    assert!(
        cleanup_interval_secs > 0,
        "CLEANUP_INTERVAL_SECS must be greater than zero"
    );

    let max_file_age_days = env::var("MAX_FILE_AGE_DAYS")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .expect("Invalid value for MAX_FILE_AGE_DAYS");

    let max_upstream_cache_ttl_days = env::var("MAX_UPSTREAM_CACHE_TTL_DAYS")
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .expect("Invalid value for MAX_UPSTREAM_CACHE_TTL_DAYS");

    // Parse max upstream download size in MB
    let max_upstream_download_size_mb = env::var("MAX_UPSTREAM_DOWNLOAD_SIZE_MB")
        .unwrap_or_else(|_| "100".to_string()) // Default: 100MB
        .parse()
        .expect("Invalid value for MAX_UPSTREAM_DOWNLOAD_SIZE_MB");

    // Parse max chunk size in MB for chunked uploads
    let max_chunk_size_mb: u64 = env::var("MAX_CHUNK_SIZE_MB")
        .unwrap_or_else(|_| "100".to_string()) // Default: 100MB
        .parse()
        .expect("Invalid value for MAX_CHUNK_SIZE_MB");

    let max_blob_size_bytes = env::var("MAX_BLOB_SIZE_MB")
        .unwrap_or_else(|_| "100".to_string())
        .parse::<u64>()
        .expect("Invalid value for MAX_BLOB_SIZE_MB")
        .checked_mul(1024 * 1024)
        .expect("MAX_BLOB_SIZE_MB value too large");
    let min_free_disk_bytes = env::var("MIN_FREE_DISK_MB")
        .unwrap_or_else(|_| "256".to_string())
        .parse::<u64>()
        .expect("Invalid value for MIN_FREE_DISK_MB")
        .checked_mul(1024 * 1024)
        .expect("MIN_FREE_DISK_MB value too large");
    let max_chunk_upload_sessions = env::var("MAX_CHUNK_UPLOAD_SESSIONS")
        .unwrap_or_else(|_| "128".to_string())
        .parse::<usize>()
        .expect("Invalid value for MAX_CHUNK_UPLOAD_SESSIONS");
    let max_chunk_upload_sessions_per_pubkey = env::var("MAX_CHUNK_UPLOAD_SESSIONS_PER_PUBKEY")
        .unwrap_or_else(|_| "8".to_string())
        .parse::<usize>()
        .expect("Invalid value for MAX_CHUNK_UPLOAD_SESSIONS_PER_PUBKEY");
    assert!(
        max_chunk_size_mb
            .checked_mul(1024 * 1024)
            .is_some_and(|size| size <= max_blob_size_bytes),
        "MAX_CHUNK_SIZE_MB cannot exceed MAX_BLOB_SIZE_MB"
    );

    // Parse chunk cleanup timeout in minutes
    let chunk_cleanup_timeout_minutes = env::var("CHUNK_CLEANUP_TIMEOUT_MINUTES")
        .unwrap_or_else(|_| "30".to_string()) // Default: 30 minutes
        .parse()
        .expect("Invalid value for CHUNK_CLEANUP_TIMEOUT_MINUTES");

    // Parse upstream servers from environment variable
    let upstream_servers: Vec<String> = env::var("UPSTREAM_SERVERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|server| {
            let server = server.trim();
            if server.is_empty() {
                None
            } else {
                Some(server.to_string())
            }
        })
        .collect();

    // Parse upstream mode from environment variable (default: proxy)
    let upstream_mode = models::UpstreamMode::from_str_with_default(
        &env::var("UPSTREAM_MODE").unwrap_or_else(|_| "proxy".to_string()),
    );

    if !upstream_servers.is_empty() {
        info!("⚙️ Upstream servers: {:?}", upstream_servers);
        info!("⚙️ Upstream mode: {}", upstream_mode.as_str());
        info!(
            "⚙️ Upstream download size limit: {} MB",
            max_upstream_download_size_mb
        );
    }

    // Parse feature flags
    // Upload: default to "public" (enabled for everyone)
    let feature_upload_enabled = models::FeatureMode::from_str_with_default(
        &env::var("FEATURE_UPLOAD_ENABLED").unwrap_or_else(|_| "public".to_string()),
        models::FeatureMode::Public,
    );

    // Mirror: default to "public" (enabled for everyone)
    let feature_mirror_enabled = models::FeatureMode::from_str_with_default(
        &env::var("FEATURE_MIRROR_ENABLED").unwrap_or_else(|_| "public".to_string()),
        models::FeatureMode::Public,
    );

    // List: keep as boolean for now
    let feature_list_enabled = env::var("FEATURE_LIST_ENABLED")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    // Custom upstream origin: default to "off" (disabled)
    let feature_custom_upstream_origin_enabled = models::FeatureMode::from_str_with_default(
        &env::var("FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED").unwrap_or_else(|_| "off".to_string()),
        models::FeatureMode::Off,
    );

    let feature_homepage_enabled = env::var("FEATURE_HOMEPAGE_ENABLED")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    let feature_p2p_serve_enabled = env::var("FEATURE_P2P_SERVE_ENABLED")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    let p2p_nsec = env::var("P2P_NSEC")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let p2p_relays: Vec<String> = env::var("P2P_RELAYS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|relay| {
            let relay = relay.trim();
            if relay.is_empty() {
                None
            } else {
                Some(relay.to_string())
            }
        })
        .collect();

    let p2p_stun_servers: Vec<String> = env::var("P2P_STUN_SERVERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|server| {
            let server = server.trim();
            if server.is_empty() {
                None
            } else {
                Some(server.to_string())
            }
        })
        .collect();

    let p2p_request_timeout_ms = env::var("P2P_REQUEST_TIMEOUT_MS")
        .unwrap_or_else(|_| "10000".to_string())
        .parse()
        .expect("Invalid value for P2P_REQUEST_TIMEOUT_MS");

    let p2p_hello_interval_ms = env::var("P2P_HELLO_INTERVAL_MS")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("Invalid value for P2P_HELLO_INTERVAL_MS");

    let p2p_debug = env::var("P2P_DEBUG")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    if feature_p2p_serve_enabled {
        info!(
            "⚙️ Hashtree P2P serving enabled - relays: {}, STUN servers: {}",
            if p2p_relays.is_empty() {
                "default".to_string()
            } else {
                format!("{:?}", p2p_relays)
            },
            if p2p_stun_servers.is_empty() {
                "default".to_string()
            } else {
                format!("{:?}", p2p_stun_servers)
            }
        );
    }

    // Report feature: default to "off" (disabled)
    let feature_report_enabled = models::FeatureMode::from_str_with_default(
        &env::var("FEATURE_REPORT_ENABLED").unwrap_or_else(|_| "off".to_string()),
        models::FeatureMode::Off,
    );

    // Report action: quarantine (default) or delete
    let report_action = models::ReportAction::from_str_with_default(
        &env::var("REPORT_ACTION").unwrap_or_else(|_| "quarantine".to_string()),
    );

    info!("⚙️ Feature flags - Upload: {}, Mirror: {}, List: {}, CustomUpstreamOrigin: {}, Homepage: {}, Report: {}",
          feature_upload_enabled.as_str(), feature_mirror_enabled.as_str(), feature_list_enabled,
          feature_custom_upstream_origin_enabled.as_str(), feature_homepage_enabled, feature_report_enabled.as_str());

    if feature_report_enabled.is_enabled() {
        info!("⚙️ Report action: {}", report_action.as_str());
    }

    // Parse Cashu payment configuration (BUD-07)
    let feature_paid_upload = env::var("FEATURE_PAID_UPLOAD")
        .unwrap_or_else(|_| "off".to_string())
        .to_lowercase()
        == "on";

    let feature_paid_mirror = env::var("FEATURE_PAID_MIRROR")
        .unwrap_or_else(|_| "off".to_string())
        .to_lowercase()
        == "on";

    let feature_paid_download = env::var("FEATURE_PAID_DOWNLOAD")
        .unwrap_or_else(|_| "off".to_string())
        .to_lowercase()
        == "on";

    let cashu_price_per_mb = env::var("CASHU_PRICE_PER_MB")
        .unwrap_or_else(|_| "1".to_string())
        .parse::<u64>()
        .expect("Invalid value for CASHU_PRICE_PER_MB");

    let cashu_accepted_mints: Vec<String> = env::var("CASHU_ACCEPTED_MINTS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|m| {
            let m = m.trim();
            if m.is_empty() {
                None
            } else {
                Some(m.to_string())
            }
        })
        .collect();

    let cashu_wallet_path = PathBuf::from(
        env::var("CASHU_WALLET_PATH").unwrap_or_else(|_| "./cashu_wallet.db".to_string()),
    );
    let any_paid_feature = feature_paid_upload || feature_paid_mirror || feature_paid_download;

    // Validate: if any paid feature is on, mints must be configured
    // The wallet is intentionally single-mint.  Accepting several mints while
    // crediting all tokens to one wallet is not a valid settlement model.
    assert!(
        !(any_paid_feature && cashu_accepted_mints.len() != 1),
        "Exactly one CASHU_ACCEPTED_MINTS value is required when paid features are enabled"
    );

    if any_paid_feature {
        info!(
            "💰 Cashu payments enabled - Price: {} sats/MB, Mints: {:?}",
            cashu_price_per_mb, cashu_accepted_mints
        );
    }

    // Initialize Cashu wallet if any paid feature is enabled

    let hls_mirror_concurrency: usize = env::var("HLS_MIRROR_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    info!("HLS mirror concurrency: {}", hls_mirror_concurrency);

    let cashu_wallet = if any_paid_feature {
        match services::cashu::init_wallet(&cashu_wallet_path, &cashu_accepted_mints).await {
            Ok(wallet) => {
                info!("💰 Cashu wallet ready for payments");
                Some(wallet)
            }
            Err(e) => {
                error!("💰 Failed to initialize Cashu wallet: {}", e);
                error!(
                    "💰 Cannot start with paid features enabled but wallet initialization failed"
                );
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // Parse blossom server list cache TTL in hours (default: 24 hours)
    let blossom_server_list_cache_ttl_hours = env::var("BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS")
        .unwrap_or_else(|_| "24".to_string())
        .parse()
        .expect("Invalid value for BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS");

    info!(
        "⚙️ Blossom server list cache TTL: {} hours",
        blossom_server_list_cache_ttl_hours
    );

    // Parse filter algorithm from environment variable (default: binary-fuse-16)
    let filter_algorithm = env::var("FILTER_ALGORITHM")
        .unwrap_or_else(|_| "binary-fuse-16".to_string())
        .to_lowercase();

    // Validate filter algorithm
    let filter_algorithm = match filter_algorithm.as_str() {
        "bloom" | "binary-fuse-8" | "binary-fuse-16" | "binary-fuse-32" => filter_algorithm,
        _ => {
            warn!(
                "⚠️ Invalid FILTER_ALGORITHM '{}', defaulting to 'binary-fuse-16'",
                filter_algorithm
            );
            "binary-fuse-16".to_string()
        }
    };
    info!("⚙️ Filter algorithm: {}", filter_algorithm);

    // Parse DVM allowed kinds from environment variable
    let dvm_allowed_kinds: Vec<u16> = env::var("DVM_ALLOWED_KINDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|k| {
            let k = k.trim();
            if k.is_empty() {
                None
            } else {
                match k.parse::<u16>() {
                    Ok(kind) => Some(kind),
                    Err(e) => {
                        error!("Failed to parse DVM kind '{}': {}", k, e);
                        None
                    }
                }
            }
        })
        .collect();

    // Validate: if any feature uses DVM mode, kinds must be configured
    let needs_dvm = feature_upload_enabled.requires_dvm() || feature_mirror_enabled.requires_dvm();
    assert!(
        !(needs_dvm && dvm_allowed_kinds.is_empty()),
        "DVM_ALLOWED_KINDS must be set when any feature uses 'dvm' mode"
    );

    if !dvm_allowed_kinds.is_empty() {
        info!("🤖 DVM allowed kinds: {:?}", dvm_allowed_kinds);
    }

    // Parse DVM relays from environment variable
    let dvm_relays: Vec<String> = env::var("DVM_RELAYS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Parse DVM refresh interval from environment variable (default: 5 minutes)
    let dvm_refresh_interval_mins: u64 = env::var("DVM_REFRESH_INTERVAL_MINS")
        .unwrap_or_else(|_| "5".to_string())
        .parse()
        .unwrap_or(5);

    // Parse allowed pubkeys from environment variable
    let allowed_pubkeys: Vec<PublicKey> = env::var("ALLOWED_NPUBS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|npub| {
            if npub.trim().is_empty() {
                None
            } else {
                match PublicKey::from_bech32(npub.trim()) {
                    Ok(pk) => Some(pk),
                    Err(e) => {
                        error!("Failed to parse npub {}: {}", npub, e);
                        None
                    }
                }
            }
        })
        .collect();

    let auth_max_ttl_secs = env::var("AUTH_MAX_TTL_SECS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .expect("Invalid value for AUTH_MAX_TTL_SECS");
    let auth_max_age_secs = env::var("AUTH_MAX_AGE_SECS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .expect("Invalid value for AUTH_MAX_AGE_SECS");
    let auth_clock_skew_secs = env::var("AUTH_CLOCK_SKEW_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .expect("Invalid value for AUTH_CLOCK_SKEW_SECS");
    let auth_require_server_tag = env::var("AUTH_REQUIRE_SERVER_TAG")
        .unwrap_or_else(|_| "false".to_string())
        .parse()
        .expect("Invalid value for AUTH_REQUIRE_SERVER_TAG");
    let metrics_bearer_token = env::var("METRICS_BEARER_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());

    // Initialize Prometheus metrics
    let metrics = metrics::Metrics::new();
    info!("✅ Prometheus metrics initialized");

    // Handle HTTPS/TLS setup if enabled
    if enable_https {
        info!("🔐 HTTPS enabled");
        if let Err(e) =
            tls::ensure_tls_certificates(&tls_cert_path, &tls_key_path, tls_auto_generate)
        {
            error!("❌ Failed to setup TLS certificates: {}", e);
            std::process::exit(1);
        }
    } else {
        info!("⚠️  HTTPS disabled - running in HTTP mode");
    }

    AppState {
        native_s3,
        storage,
        blob_mutation_locks: Arc::new(models::BlobMutationLocks::default()),
        superseded_blob_deletions: Arc::new(RwLock::new(Vec::new())),
        file_index,
        serve_file_index,
        serve_files_path,
        serve_files_manifest_name,
        serve_files_refresh_interval_secs,
        cors_allowed_origins,
        max_total_size,
        max_total_files,
        max_blob_size_bytes,
        min_free_disk_bytes,
        bind_addr,
        public_url,
        cleanup_interval_secs,
        changes_pending: Arc::new(RwLock::new(true)),
        allowed_pubkeys,
        trusted_pubkeys: Arc::new(RwLock::new(HashMap::new())),
        dvm_pubkeys: Arc::new(RwLock::new(std::collections::HashSet::new())),
        dvm_allowed_kinds,
        dvm_relays,
        dvm_refresh_interval_mins,
        max_file_age_days,
        max_upstream_cache_ttl_days,
        filter_cache: Arc::new(RwLock::new(None)),
        upstream_servers,
        upstream_mode,
        max_upstream_download_size_mb,
        upstream_client: services::upload::create_upstream_client()
            .expect("Failed to build upstream HTTP client"),
        max_chunk_size_mb,
        chunk_cleanup_timeout_minutes,
        max_chunk_upload_sessions,
        max_chunk_upload_sessions_per_pubkey,
        feature_upload_enabled,
        feature_mirror_enabled,
        feature_list_enabled,
        feature_custom_upstream_origin_enabled,
        feature_homepage_enabled,
        feature_p2p_serve_enabled,
        p2p_nsec,
        p2p_relays,
        p2p_stun_servers,
        p2p_request_timeout_ms,
        p2p_hello_interval_ms,
        p2p_debug,
        ongoing_downloads: Arc::new(RwLock::new(HashMap::new())),
        upstream_negotiations: Arc::new(RwLock::new(HashMap::new())),
        chunk_sessions: Arc::new(
            services::chunk_sessions::ChunkSessions::new(
                services::chunk_sessions::SessionLimits {
                    max_sessions: max_chunk_upload_sessions,
                    max_sessions_per_pubkey: max_chunk_upload_sessions_per_pubkey,
                },
            ),
        ),
        failed_upstream_lookups: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_lists: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_list_cache_ttl_hours,
        filter_algorithm,
        metrics,
        report_action,
        feature_report_enabled,
        auth_max_ttl_secs,
        auth_max_age_secs,
        auth_clock_skew_secs,
        auth_require_server_tag,
        metrics_bearer_token,
        destructive_event_replays: Arc::new(RwLock::new(HashMap::new())),
        feature_paid_upload,
        feature_paid_mirror,
        feature_paid_download,
        cashu_price_per_mb,
        cashu_accepted_mints,
        cashu_wallet_path,
        cashu_wallet,
        hls_mirror_concurrency,
    }
}

fn start_cleanup_job(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(state.cleanup_interval_secs));
        loop {
            interval.tick().await;
            // Expiry must run even during idle periods so cache TTL is bounded
            // by one configured cleanup interval.
            enforce_storage_limits(&state).await;
            *state.changes_pending.write().await = false;

            cleanup_expired_failed_lookups(&state).await;
            cleanup_expired_blossom_server_lists(&state).await;
        }
    });
}

fn start_chunk_cleanup_job(state: AppState) {
    tokio::spawn(async move {
        // Run chunk cleanup every 5 minutes
        let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            interval.tick().await;
            cleanup_abandoned_chunks(&state).await;
        }
    });
}

fn start_trust_network_refresh_job(state: AppState) {
    tokio::spawn(async move {
        // Only run if any feature is using WOT mode
        let needs_wot = state.feature_upload_enabled.requires_wot()
            || state.feature_mirror_enabled.requires_wot()
            || state.feature_custom_upstream_origin_enabled.requires_wot();

        if !needs_wot {
            info!("⚠️ Trust network refresh disabled - no features using WOT mode");
            return;
        }

        info!("✅ Trust network refresh enabled - features using WOT mode");

        let mut interval = tokio::time::interval(Duration::from_secs(4 * 3600));
        loop {
            interval.tick().await;
            if !state.allowed_pubkeys.is_empty() {
                match refresh_trust_network(&state.allowed_pubkeys).await {
                    Ok(trusted) => {
                        let mut trusted_pubkeys = state.trusted_pubkeys.write().await;
                        *trusted_pubkeys = trusted;
                    }
                    Err(e) => {
                        error!("Failed to refresh trust network: {}", e);
                    }
                }
            }
        }
    });
}

fn start_dvm_refresh_job(state: AppState) {
    tokio::spawn(async move {
        let needs_dvm = state.feature_upload_enabled.requires_dvm()
            || state.feature_mirror_enabled.requires_dvm();

        if !needs_dvm || state.dvm_allowed_kinds.is_empty() {
            info!("⚠️ DVM refresh disabled - no features using DVM mode");
            return;
        }

        info!(
            "✅ DVM refresh enabled - allowed kinds: {:?}, interval: {}m",
            state.dvm_allowed_kinds, state.dvm_refresh_interval_mins
        );

        // Refresh periodically
        let mut interval =
            tokio::time::interval(Duration::from_secs(state.dvm_refresh_interval_mins * 60));
        loop {
            interval.tick().await;
            match refresh_dvm_pubkeys(&state.dvm_allowed_kinds, &state.dvm_relays).await {
                Ok(pubkeys) => {
                    info!("🤖 DVM refresh complete: {} pubkeys", pubkeys.len());
                    let mut dvm_pubkeys = state.dvm_pubkeys.write().await;
                    *dvm_pubkeys = pubkeys;
                }
                Err(e) => {
                    error!("Failed to refresh DVM pubkeys: {}", e);
                }
            }
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Install default crypto provider for rustls (required for HTTPS)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let state = load_app_state().await;
    let addr = state
        .bind_addr
        .parse::<SocketAddr>()
        .expect("Invalid address format");

    // Get HTTPS configuration
    let enable_https = env::var("ENABLE_HTTPS")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    start_cleanup_job(state.clone());
    start_chunk_cleanup_job(state.clone());
    start_trust_network_refresh_job(state.clone());
    start_dvm_refresh_job(state.clone());
    if let Some(path) = state.serve_files_path.clone() {
        services::serve_files::start_refresh_job(
            path,
            state.serve_files_manifest_name.clone(),
            state.serve_files_refresh_interval_secs,
            state.serve_file_index.clone(),
        );
    }
    services::p2p::start_p2p_serve_job(state.clone());

    let app = create_app(state.clone()).await;

    // Spawn a task to handle shutdown signals - exit immediately when received
    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => {
                info!("🛑 Received SIGINT (Ctrl+C) - exiting immediately");
                std::process::exit(0);
            },
            () = terminate => {
                info!("🛑 Received SIGTERM - exiting immediately");
                std::process::exit(0);
            },
        }
    });

    // Start server with HTTPS or HTTP
    if enable_https {
        let tls_cert_path =
            PathBuf::from(env::var("TLS_CERT_PATH").unwrap_or_else(|_| "./cert.pem".to_string()));
        let tls_key_path =
            PathBuf::from(env::var("TLS_KEY_PATH").unwrap_or_else(|_| "./key.pem".to_string()));

        info!("🎧 blossom server listening on https://{}", addr);

        match tls::load_tls_config(&tls_cert_path, &tls_key_path).await {
            Ok(config) => {
                if let Err(e) = axum_server::bind_rustls(addr, config)
                    .serve(app.into_make_service())
                    .await
                {
                    error!("❌ HTTPS server error: {}", e);
                }
            }
            Err(e) => {
                error!("❌ Failed to load TLS configuration: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        info!("🎧 blossom server listening on http://{}", addr);

        // Create a TcpListener for HTTP
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind to address");

        // Start the server (no graceful shutdown - exit immediately on signal)
        if let Err(e) = axum::serve(listener, app).await {
            error!("❌ Server error: {}", e);
        }
    }
}
