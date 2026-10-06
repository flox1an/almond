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

pub use config::{Config, ConfigError};
pub use models::AppState;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::task::JoinSet;

use crate::error::AppError;
use crate::services::{authorization, cashu, file_storage, intake};
use crate::trust_network::{refresh_dvm_pubkeys, refresh_trust_network};
use crate::utils::{
    build_file_index, cleanup_abandoned_chunks, cleanup_expired_blossom_server_lists,
    cleanup_expired_failed_lookups, enforce_storage_limits, initialize_storage,
    migrate_legacy_blobs,
};
use axum::Router;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::from_fn_with_state,
    response::IntoResponse,
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

/// Evaluate the same admission policy as `PUT /upload` without accepting bytes.
///
/// BUD-06 defines this as a preflight only: a client may skip it, and any
/// Cashu token on this HEAD request is ignored rather than redeemed.
async fn head_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Response<axum::body::Body>, AppError> {
    let sha256 = headers
        .get("X-SHA-256")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing X-SHA-256 header".to_owned()))?;
    file_storage::validate_sha256_format(sha256)?;

    let _content_type = headers
        .get("X-Content-Type")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing X-Content-Type header".to_owned()))?;
    let declared_size = headers
        .get("X-Content-Length")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::LengthRequired("Missing X-Content-Length header".to_owned()))?
        .parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid X-Content-Length header".to_owned()))?;

    let authorized =
        authorization::authorize(&headers, &state, authorization::Operation::Upload).await?;
    authorized.bind(&state, sha256).await?;

    let max_size = intake::size_limit(&state, intake::Intake::ClientUpload);
    if declared_size > max_size {
        return Err(AppError::PayloadTooLarge(format!(
            "Blob size {declared_size} exceeds the {max_size}-byte limit"
        )));
    }
    file_storage::ensure_storage_capacity(&state, declared_size).await?;

    if state.feature_paid_upload {
        let quote = cashu::quote(
            state.cashu_price_per_mb,
            &state.cashu_accepted_mints,
            declared_size,
        );
        let mut response = AppError::PaymentRequired {
            amount_sats: quote.amount_sats,
            unit: quote.unit.to_owned(),
            mints: quote.mints.clone(),
        }
        .into_response();
        let response_headers = response.headers_mut();
        response_headers.insert(
            "X-Price-Per-MB",
            quote
                .amount_sats
                .to_string()
                .parse()
                .expect("price is a valid header value"),
        );
        response_headers.insert(
            "X-Price-Unit",
            quote.unit.parse().expect("unit is a valid header value"),
        );
        response_headers.insert(
            "X-Accepted-Mints",
            quote
                .mints
                .join(",")
                .parse()
                .expect("mint list is a valid header value"),
        );
        return Ok(response);
    }

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::empty())
        .map_err(|_| AppError::InternalError("Failed to build HEAD /upload response".to_owned()))
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

/// The Blossom endpoints (BUD-01/02/04/06/09), including the root
/// `/{filename}` blob route. Almond's own pages and diagnostics are in
/// [`standalone_extras`].
pub fn create_app(state: AppState) -> Router {
    let routes = Router::new()
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
        .route("/report", put(report_blob))
        .route("/{filename}", delete(delete_blob))
        .route(
            "/{filename}",
            get(handle_file_request).head(handle_file_request),
        );
    with_common_layers(routes, state)
}

/// Almond's homepage, config editor, filter and diagnostics endpoints.
/// The standalone binary merges these onto [`create_app`].
pub fn standalone_extras(state: AppState) -> Router {
    let routes = Router::new()
        .route("/_wot", get(get_wot))
        .route("/filter", get(get_filter))
        .route("/_upstream", get(get_upstream))
        .route("/_metrics", get(get_metrics))
        .route("/metrics", get(get_metrics))
        .route("/", get(serve_index))
        .route("/index.html", get(serve_index))
        .route("/filter-test.html", get(serve_filter_test))
        .route("/config", get(serve_config_editor));
    with_common_layers(routes, state)
}

/// axum applies `Router::layer` per route, so layering each router
/// separately behaves exactly like layering their merge.
fn with_common_layers(routes: Router<AppState>, state: AppState) -> Router {
    let max_blob_size = usize::try_from(state.max_blob_size_bytes)
        .expect("MAX_BLOB_SIZE_MB does not fit the platform request-body limit");
    routes
        .layer(RequestBodyLimitLayer::new(max_blob_size))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .layer(ConcurrencyLimitLayer::new(256))
        .layer(from_fn_with_state(state.clone(), cors_middleware))
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

/// Errors carry the binary's historic panic messages. A Cashu wallet
/// failure is returned as the boxed [`AppError`] so the binary can keep
/// exiting with status 1 for it.
pub async fn build_state(
    cfg: &Config,
) -> Result<AppState, Box<dyn std::error::Error + Send + Sync>> {
    let storage = models::StorageLayout::new(cfg.storage_path.clone());
    initialize_storage(&storage)
        .await
        .map_err(|error| format!("Failed to initialize storage layout: {error}"))?;
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
        .map_err(|error| format!("Failed to migrate legacy storage: {error}"))?;
    let file_index = Arc::new(services::blob_index::BlobIndex::new());
    build_file_index(&storage, &file_index)
        .await
        .map_err(|error| format!("Failed to reconstruct blob index: {error}"))?;
    if let Some(s3) = &native_s3 {
        for (sha256, metadata) in s3
            .list_all()
            .await
            .map_err(|error| format!("Failed to build S3 blob index: {error}"))?
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
            cfg.serve_files_refresh_interval.as_secs()
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

    let any_paid_feature = !cfg.cashu_paid.is_empty();
    let cashu_accepted_mints: Vec<String> = cfg.cashu_mint.iter().cloned().collect();

    if any_paid_feature {
        info!(
            "💰 Cashu payments enabled for {:?} - Price: {} sats/MiB, Mint: {:?}",
            cfg.cashu_paid, cfg.cashu_price_per_mib, cfg.cashu_mint
        );
    }

    info!("HLS mirror concurrency: {}", cfg.hls_mirror_concurrency);

    #[cfg(feature = "cashu")]
    let cashu_wallet = if any_paid_feature {
        match cashu::init_wallet(&cfg.cashu_wallet_path, &cashu_accepted_mints).await {
            Ok(wallet) => {
                info!("💰 Cashu wallet ready for payments");
                Some(wallet)
            }
            Err(e) => {
                error!("💰 Failed to initialize Cashu wallet: {}", e);
                error!(
                    "💰 Cannot start with paid features enabled but wallet initialization failed"
                );
                return Err(Box::new(e));
            }
        }
    } else {
        None
    };

    info!(
        "⚙️ Blossom server list cache TTL: {}s",
        cfg.server_list_cache_ttl.as_secs()
    );

    info!("⚙️ Filter algorithm: {}", cfg.filter_algorithm);

    info!(
        "⚙️ Access - Upload: {}, Mirror: {}, List: {}, CustomOrigin: {}, Homepage: {}, Report: {}",
        cfg.upload_access.as_str(),
        cfg.mirror_access.as_str(),
        cfg.list_enabled,
        cfg.custom_origin_access.as_str(),
        cfg.homepage_enabled,
        cfg.report_access.as_str()
    );

    if cfg.report_access.is_enabled() {
        info!("⚙️ Report action: {}", cfg.report_action.as_str());
    }

    if !cfg.dvm_kinds.is_empty() {
        info!("🤖 DVM allowed kinds: {:?}", cfg.dvm_kinds);
    }

    if !cfg.upstream_servers.is_empty() {
        info!("⚙️ Upstream servers: {:?}", cfg.upstream_servers);
        info!("⚙️ Upstream mode: {}", cfg.upstream_mode.as_str());
        info!(
            "⚙️ Upstream download size limit: {} bytes",
            cfg.upstream_max_download_size
        );
    }

    let upstream_client = services::upload::create_upstream_client()
        .map_err(|error| format!("Failed to build upstream HTTP client: {error}"))?;

    Ok(AppState {
        native_s3,
        storage,
        blob_mutation_locks: Arc::new(models::BlobMutationLocks::default()),
        superseded_blob_deletions: Arc::new(RwLock::new(Vec::new())),
        file_index,
        serve_file_index,
        serve_files_path: cfg.serve_files_path.clone(),
        serve_files_manifest_dir: cfg.serve_files_manifest_dir.clone(),
        serve_files_manifest_name: cfg.serve_files_manifest_name.clone(),
        serve_files_refresh_interval_secs: cfg.serve_files_refresh_interval.as_secs(),
        cors_allowed_origins: cfg.cors_origins.clone(),
        max_total_size: cfg.storage_max_size,
        max_total_files: cfg.storage_max_files,
        max_blob_size_bytes: cfg.blob_max_size,
        min_free_disk_bytes: cfg.storage_min_free,
        bind_addr: cfg.bind_addr.to_string(),
        public_url: cfg.public_url(),
        cleanup_interval_secs: cfg.cleanup_interval.as_secs(),
        changes_pending: Arc::new(RwLock::new(true)),
        allowed_pubkeys: cfg.allowed_npubs.clone(),
        trusted_pubkeys: Arc::new(RwLock::new(HashMap::new())),
        dvm_pubkeys: Arc::new(RwLock::new(std::collections::HashSet::new())),
        dvm_allowed_kinds: cfg.dvm_kinds.clone(),
        dvm_relays: cfg.dvm_relays.clone(),
        dvm_refresh_interval: cfg.dvm_refresh_interval,
        max_file_age_secs: cfg.upload_max_age.as_secs(),
        max_upstream_cache_ttl_secs: cfg.upstream_cache_ttl.as_secs(),
        filter_cache: Arc::new(RwLock::new(None)),
        upstream_servers: cfg.upstream_servers.clone(),
        upstream_mode: cfg.upstream_mode,
        max_upstream_download_size_bytes: cfg.upstream_max_download_size,
        upstream_client,
        max_chunk_size_bytes: cfg.chunk_max_size,
        chunk_cleanup_timeout: cfg.chunk_session_timeout,
        max_chunk_upload_sessions: cfg.chunk_max_sessions,
        max_chunk_upload_sessions_per_pubkey: cfg.chunk_max_sessions_per_pubkey,
        feature_upload_enabled: cfg.upload_access,
        feature_mirror_enabled: cfg.mirror_access,
        feature_list_enabled: cfg.list_enabled,
        feature_custom_upstream_origin_enabled: cfg.custom_origin_access,
        feature_homepage_enabled: cfg.homepage_enabled,
        ongoing_downloads: Arc::new(RwLock::new(HashMap::new())),
        upstream_negotiations: Arc::new(RwLock::new(HashMap::new())),
        chunk_sessions: Arc::new(services::chunk_sessions::ChunkSessions::new(
            services::chunk_sessions::SessionLimits {
                max_sessions: cfg.chunk_max_sessions,
                max_sessions_per_pubkey: cfg.chunk_max_sessions_per_pubkey,
            },
        )),
        failed_upstream_lookups: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_lists: Arc::new(RwLock::new(HashMap::new())),
        blossom_server_list_cache_ttl: cfg.server_list_cache_ttl,
        filter_algorithm: cfg.filter_algorithm.clone(),
        metrics,
        report_action: cfg.report_action,
        feature_report_enabled: cfg.report_access,
        auth_max_ttl_secs: cfg.auth_max_ttl.as_secs(),
        auth_clock_skew_secs: cfg.auth_clock_skew.as_secs(),
        auth_require_server_tag: cfg.auth_require_server_tag,
        metrics_bearer_token: cfg.metrics_token.clone(),
        destructive_event_replays: Arc::new(RwLock::new(HashMap::new())),
        feature_paid_upload: cfg.cashu_paid.contains(&cashu::PaidOperation::Upload),
        feature_paid_mirror: cfg.cashu_paid.contains(&cashu::PaidOperation::Mirror),
        feature_paid_download: cfg.cashu_paid.contains(&cashu::PaidOperation::Download),
        cashu_price_per_mb: cfg.cashu_price_per_mib,
        cashu_accepted_mints,
        cashu_wallet_path: cfg.cashu_wallet_path.clone(),
        #[cfg(feature = "cashu")]
        cashu_wallet,
        hls_mirror_concurrency: cfg.hls_mirror_concurrency,
    })
}

/// Spawn the periodic jobs the enabled features need. Dropping the returned
/// set aborts them all, so hold it for as long as the state is served.
pub fn spawn_background_tasks(state: &AppState, cfg: &Config) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    start_cleanup_job(&mut tasks, state.clone());
    start_chunk_cleanup_job(&mut tasks, state.clone());

    // Only spawn jobs whose features are enabled.
    if cfg.upload_access.requires_wot()
        || cfg.mirror_access.requires_wot()
        || cfg.custom_origin_access.requires_wot()
    {
        start_trust_network_refresh_job(&mut tasks, state.clone());
    }

    if (cfg.upload_access.requires_dvm() || cfg.mirror_access.requires_dvm())
        && !cfg.dvm_kinds.is_empty()
    {
        start_dvm_refresh_job(&mut tasks, state.clone());
    }

    if let Some(path) = &cfg.serve_files_path {
        services::serve_files::start_refresh_job(
            &mut tasks,
            path.clone(),
            cfg.serve_files_manifest_name.clone(),
            cfg.serve_files_refresh_interval.as_secs(),
            state.serve_file_index.clone(),
            cfg.serve_files_manifest_dir.clone(),
        );
    }
    tasks
}

fn start_cleanup_job(tasks: &mut JoinSet<()>, state: AppState) {
    tasks.spawn(async move {
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

fn start_chunk_cleanup_job(tasks: &mut JoinSet<()>, state: AppState) {
    tasks.spawn(async move {
        // Run chunk cleanup every 5 minutes
        let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            interval.tick().await;
            cleanup_abandoned_chunks(&state).await;
        }
    });
}

fn start_trust_network_refresh_job(tasks: &mut JoinSet<()>, state: AppState) {
    tasks.spawn(async move {
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

fn start_dvm_refresh_job(tasks: &mut JoinSet<()>, state: AppState) {
    tasks.spawn(async move {
        info!(
            "✅ DVM refresh enabled - allowed kinds: {:?}, interval: {}s",
            state.dvm_allowed_kinds,
            state.dvm_refresh_interval.as_secs()
        );

        // Refresh periodically
        let mut interval = tokio::time::interval(state.dvm_refresh_interval);
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

#[cfg(test)]
mod homepage_tests {
    /// The homepage is a fully static, self-contained page: every fact on it
    /// is hand-written prose. It is public and must not call back into the
    /// server it is served from, so drift here is a silent information leak
    /// or a stale claim about Almond's capabilities.
    const INDEX: &str = include_str!("index.html");

    #[test]
    fn no_network_calls_or_external_resources() {
        for forbidden in [
            "http://",
            "https://",
            "fetch",
            "XMLHttpRequest",
            "WebSocket",
            "EventSource",
            "sendBeacon",
            "src=",
            "href=",
        ] {
            assert!(
                !INDEX.contains(forbidden),
                "homepage must not reference external resources or server endpoints: {forbidden}"
            );
        }
    }

    #[test]
    fn logo_typing_and_cursor_survived_the_redesign() {
        assert!(INDEX.contains("id=\"logo\""));
        assert!(INDEX.contains("id=\"cursor\""));
        assert!(INDEX.contains("id=\"text\""));
    }

    #[test]
    fn eye_candy_respects_reduced_motion() {
        assert!(INDEX.contains("prefers-reduced-motion"));
    }

    #[test]
    fn stale_claims_are_gone() {
        for stale in [
            "no manual delete",
            "(BUD-1,", // the old incomplete BUD list
            "filesystem only, no database",
        ] {
            assert!(
                !INDEX.contains(stale),
                "outdated claim is still on the homepage: {stale}"
            );
        }
    }
}
