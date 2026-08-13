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

pub mod config;
#[cfg(test)]
mod config_editor_tests;
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

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::signal;

use crate::models::AppState;
use crate::services::cashu;
use crate::trust_network::{refresh_dvm_pubkeys, refresh_trust_network};
use crate::utils::{
    build_file_index, cleanup_abandoned_chunks, cleanup_expired_blossom_server_lists,
    cleanup_expired_failed_lookups, enforce_storage_limits, initialize_storage,
    migrate_legacy_blobs,
};
use axum::Router;
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

    // Price discovery renders the same quote the 402 would, so a client cannot
    // derive a price the server disagrees with. One megabyte is the unit the
    // header advertises.
    if state.feature_paid_upload {
        let quote = cashu::quote(
            state.cashu_price_per_mb,
            &state.cashu_accepted_mints,
            1024 * 1024,
        );
        builder = builder
            .header("X-Price-Per-MB", quote.amount_sats.to_string())
            .header("X-Price-Unit", quote.unit)
            .header("X-Accepted-Mints", quote.mints.join(","));
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

async fn serve_config_editor(
    State(state): State<AppState>,
) -> Result<axum::response::Response<axum::body::Body>, StatusCode> {
    use axum::{http::header, response::Response};

    // Shares the homepage flag rather than introducing a dedicated one: this
    // is a static, self-contained page with no server-side state of its own,
    // so whoever turns off the homepage has already opted out of Almond
    // serving browser-facing pages at all.
    if !state.feature_homepage_enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(include_str!("config-editor.html")))
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
        .route("/config", get(serve_config_editor))
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

async fn build_app_state(cfg: &config::Config) -> AppState {
    let storage = models::StorageLayout::new(PathBuf::from(&cfg.storage_path));
    initialize_storage(&storage)
        .await
        .unwrap_or_else(|error| panic!("Failed to initialize storage layout: {error}"));
    info!("⚙️ Storage path: {}", storage.root.display());

    let native_s3 = if cfg.s3_endpoint.is_some() {
        let settings = services::native_storage::S3Settings {
            endpoint: cfg.s3_endpoint.clone().expect("validated"),
            bucket: cfg.s3_bucket.clone().expect("validated"),
            access_key_id: cfg.s3_access_key_id.clone().expect("validated"),
            secret_access_key: cfg.s3_secret_access_key.clone().expect("validated"),
        };
        info!("S3 native storage enabled for bucket {}", settings.bucket);
        Some(Arc::new(
            services::native_storage::NativeS3Storage::connect(settings).await,
        ))
    } else {
        None
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
    if let Some(path) = &cfg.serve_files_path {
        info!(
            "📁 Serve files enabled: {} (manifest: {}, refresh: {}s)",
            path.display(),
            cfg.serve_files_manifest_name,
            cfg.serve_files_refresh_interval_secs
        );

        if let Err(e) = services::serve_files::refresh_serve_file_index(
            path,
            &cfg.serve_files_manifest_name,
            &serve_file_index,
            &cfg.serve_files_manifest_dir,
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

    // Initialize Prometheus metrics
    let metrics = metrics::Metrics::new();
    info!("✅ Prometheus metrics initialized");

    // Handle HTTPS/TLS setup if enabled
    if cfg.enable_https {
        info!("🔐 HTTPS enabled");
        if let Err(e) = tls::ensure_tls_certificates(
            &cfg.tls_cert_path,
            &cfg.tls_key_path,
            cfg.tls_auto_generate,
        ) {
            error!("❌ Failed to setup TLS certificates: {}", e);
            std::process::exit(1);
        }
    } else {
        info!("⚠️  HTTPS disabled - running in HTTP mode");
    }

    let any_paid_feature =
        cfg.feature_paid_upload || cfg.feature_paid_mirror || cfg.feature_paid_download;

    if any_paid_feature {
        info!(
            "💰 Cashu payments enabled - Price: {} sats/MB, Mints: {:?}",
            cfg.cashu_price_per_mb, cfg.cashu_accepted_mints
        );
    }

    info!("HLS mirror concurrency: {}", cfg.hls_mirror_concurrency);

    let cashu_wallet = if any_paid_feature {
        match cashu::init_wallet(&cfg.cashu_wallet_path, &cfg.cashu_accepted_mints).await {
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

    info!(
        "⚙️ Blossom server list cache TTL: {} hours",
        cfg.blossom_server_list_cache_ttl_hours
    );

    info!("⚙️ Filter algorithm: {}", cfg.filter_algorithm);

    info!("⚙️ Feature flags - Upload: {}, Mirror: {}, List: {}, CustomUpstreamOrigin: {}, Homepage: {}, Report: {}",
          cfg.feature_upload_enabled.as_str(), cfg.feature_mirror_enabled.as_str(), cfg.feature_list_enabled,
          cfg.feature_custom_upstream_origin_enabled.as_str(), cfg.feature_homepage_enabled, cfg.feature_report_enabled.as_str());

    if cfg.feature_report_enabled.is_enabled() {
        info!("⚙️ Report action: {}", cfg.report_action.as_str());
    }

    if !cfg.dvm_allowed_kinds.is_empty() {
        info!("🤖 DVM allowed kinds: {:?}", cfg.dvm_allowed_kinds);
    }

    if cfg.feature_p2p_serve_enabled {
        info!(
            "⚙️ Hashtree P2P serving enabled - relays: {}, STUN servers: {}",
            if cfg.p2p_relays.is_empty() {
                "default".to_string()
            } else {
                format!("{:?}", cfg.p2p_relays)
            },
            if cfg.p2p_stun_servers.is_empty() {
                "default".to_string()
            } else {
                format!("{:?}", cfg.p2p_stun_servers)
            }
        );
    }

    if !cfg.upstream_servers.is_empty() {
        info!("⚙️ Upstream servers: {:?}", cfg.upstream_servers);
        info!("⚙️ Upstream mode: {}", cfg.upstream_mode.as_str());
        info!(
            "⚙️ Upstream download size limit: {} MB",
            cfg.max_upstream_download_size_mb
        );
    }

    AppState {
        native_s3,
        storage,
        blob_mutation_locks: Arc::new(models::BlobMutationLocks::default()),
        superseded_blob_deletions: Arc::new(RwLock::new(Vec::new())),
        file_index,
        serve_file_index,
        serve_files_path: cfg.serve_files_path.clone(),
        serve_files_manifest_dir: cfg.serve_files_manifest_dir.clone(),
        serve_files_manifest_name: cfg.serve_files_manifest_name.clone(),
        serve_files_refresh_interval_secs: cfg.serve_files_refresh_interval_secs,
        cors_allowed_origins: cfg.cors_allowed_origins.clone(),
        max_total_size: cfg.max_total_size,
        max_total_files: cfg.max_total_files,
        max_blob_size_bytes: cfg.max_blob_size_bytes,
        min_free_disk_bytes: cfg.min_free_disk_bytes,
        bind_addr: cfg.bind_addr.clone(),
        public_url: cfg.public_url.clone(),
        cleanup_interval_secs: cfg.cleanup_interval_secs,
        changes_pending: Arc::new(RwLock::new(true)),
        allowed_pubkeys: cfg.allowed_pubkeys.clone(),
        trusted_pubkeys: Arc::new(RwLock::new(HashMap::new())),
        dvm_pubkeys: Arc::new(RwLock::new(std::collections::HashSet::new())),
        dvm_allowed_kinds: cfg.dvm_allowed_kinds.clone(),
        dvm_relays: cfg.dvm_relays.clone(),
        dvm_refresh_interval_mins: cfg.dvm_refresh_interval_mins,
        max_file_age_days: cfg.max_file_age_days,
        max_upstream_cache_ttl_days: cfg.max_upstream_cache_ttl_days,
        filter_cache: Arc::new(RwLock::new(None)),
        upstream_servers: cfg.upstream_servers.clone(),
        upstream_mode: cfg.upstream_mode,
        max_upstream_download_size_mb: cfg.max_upstream_download_size_mb,
        upstream_client: services::upload::create_upstream_client()
            .expect("Failed to build upstream HTTP client"),
        max_chunk_size_mb: cfg.max_chunk_size_mb,
        chunk_cleanup_timeout_minutes: cfg.chunk_cleanup_timeout_minutes,
        max_chunk_upload_sessions: cfg.max_chunk_upload_sessions,
        max_chunk_upload_sessions_per_pubkey: cfg.max_chunk_upload_sessions_per_pubkey,
        feature_upload_enabled: cfg.feature_upload_enabled,
        feature_mirror_enabled: cfg.feature_mirror_enabled,
        feature_list_enabled: cfg.feature_list_enabled,
        feature_custom_upstream_origin_enabled: cfg.feature_custom_upstream_origin_enabled,
        feature_homepage_enabled: cfg.feature_homepage_enabled,
        feature_p2p_serve_enabled: cfg.feature_p2p_serve_enabled,
        p2p_nsec: cfg.p2p_nsec.clone(),
        p2p_relays: cfg.p2p_relays.clone(),
        p2p_stun_servers: cfg.p2p_stun_servers.clone(),
        p2p_request_timeout_ms: cfg.p2p_request_timeout_ms,
        p2p_hello_interval_ms: cfg.p2p_hello_interval_ms,
        p2p_debug: cfg.p2p_debug,
        ongoing_downloads: Arc::new(RwLock::new(HashMap::new())),
        upstream_negotiations: Arc::new(RwLock::new(HashMap::new())),
        chunk_sessions: Arc::new(services::chunk_sessions::ChunkSessions::new(
            services::chunk_sessions::SessionLimits {
                max_sessions: cfg.max_chunk_upload_sessions,
                max_sessions_per_pubkey: cfg.max_chunk_upload_sessions_per_pubkey,
            },
        )),
        failed_upstream_lookups: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_lists: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_list_cache_ttl_hours: cfg.blossom_server_list_cache_ttl_hours,
        filter_algorithm: cfg.filter_algorithm.clone(),
        metrics,
        report_action: cfg.report_action,
        feature_report_enabled: cfg.feature_report_enabled,
        auth_max_ttl_secs: cfg.auth_max_ttl_secs,
        auth_max_age_secs: cfg.auth_max_age_secs,
        auth_clock_skew_secs: cfg.auth_clock_skew_secs,
        auth_require_server_tag: cfg.auth_require_server_tag,
        metrics_bearer_token: cfg.metrics_bearer_token.clone(),
        destructive_event_replays: Arc::new(RwLock::new(HashMap::new())),
        feature_paid_upload: cfg.feature_paid_upload,
        feature_paid_mirror: cfg.feature_paid_mirror,
        feature_paid_download: cfg.feature_paid_download,
        cashu_price_per_mb: cfg.cashu_price_per_mb,
        cashu_accepted_mints: cfg.cashu_accepted_mints.clone(),
        cashu_wallet_path: cfg.cashu_wallet_path.clone(),
        cashu_wallet,
        hls_mirror_concurrency: cfg.hls_mirror_concurrency,
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

    // Parse and validate configuration — boot errors exit here.
    let cfg = match config::Config::from_env() {
        Ok(cfg) => cfg,
        Err(error) => {
            error!("❌ Configuration error: {error}");
            std::process::exit(1);
        }
    };

    let addr = cfg
        .bind_addr
        .parse::<SocketAddr>()
        .expect("Invalid address format");

    let state = build_app_state(&cfg).await;

    start_cleanup_job(state.clone());
    start_chunk_cleanup_job(state.clone());

    // Only spawn jobs whose features are enabled.
    if cfg.feature_upload_enabled.requires_wot()
        || cfg.feature_mirror_enabled.requires_wot()
        || cfg.feature_custom_upstream_origin_enabled.requires_wot()
    {
        start_trust_network_refresh_job(state.clone());
    }

    if (cfg.feature_upload_enabled.requires_dvm() || cfg.feature_mirror_enabled.requires_dvm())
        && !cfg.dvm_allowed_kinds.is_empty()
    {
        start_dvm_refresh_job(state.clone());
    }

    if let Some(path) = &cfg.serve_files_path {
        services::serve_files::start_refresh_job(
            path.clone(),
            cfg.serve_files_manifest_name.clone(),
            cfg.serve_files_refresh_interval_secs,
            state.serve_file_index.clone(),
            cfg.serve_files_manifest_dir.clone(),
        );
    }

    if cfg.feature_p2p_serve_enabled {
        services::p2p::start_p2p_serve_job(state.clone());
    }

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
    if cfg.enable_https {
        info!("🎧 blossom server listening on https://{}", addr);

        match tls::load_tls_config(&cfg.tls_cert_path, &cfg.tls_key_path).await {
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
