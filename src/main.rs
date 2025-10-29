pub mod constants;
pub mod handlers;
pub mod helpers;
pub mod middleware;
pub mod models;
pub mod trust_network;
pub mod utils;

use std::{collections::HashMap, env, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::signal;

use crate::models::AppState;
use crate::trust_network::refresh_trust_network;
use crate::utils::{build_file_index, enforce_storage_limits, cleanup_abandoned_chunks, cleanup_expired_failed_lookups};
use axum::Router;
use axum_server;
use dotenv::dotenv;
use nostr_relay_pool::prelude::*;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber;

use axum::{
    extract::DefaultBodyLimit,
    middleware::from_fn,
    routing::{delete, get, put},
};
use handlers::*;
use middleware::cors_middleware;

// Missing handler functions
async fn head_upload() -> &'static str {
    "Method not allowed"
}

async fn options_upload() -> &'static str {
    "Method not allowed"
}

async fn serve_index() -> axum::response::Response<axum::body::Body> {
    use axum::{
        http::{header, StatusCode},
        response::Response,
    };
    
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(include_str!("index.html")))
        .unwrap()
}

async fn method_not_allowed() -> &'static str {
    "Method not allowed"
}

pub async fn create_app(state: AppState) -> Router {
    // Calculate max chunk size in bytes
    let max_chunk_size_bytes = (state.max_chunk_size_mb * 1024 * 1024) as usize;
    
    Router::new()
        .route("/upload", put(upload_file).head(head_upload).options(options_upload).patch(patch_upload))
        .route("/list", get(list_blobs))
        .route("/list/:id", get(list_blobs))
        .route("/mirror", put(mirror_blob))
        .route("/_stats", get(get_stats))
        .route("/_bloom", get(get_bloom))
        .route("/_upstream", get(get_upstream))
        .route("/", get(serve_index))
        .route("/index.html", get(serve_index))
        .route("/:filename", delete(method_not_allowed))
        .route(
            "/:filename",
            get(handle_file_request).head(handle_file_request),
        )
        .layer(DefaultBodyLimit::max(max_chunk_size_bytes))
        .layer(from_fn(cors_middleware))
        .with_state(state)
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

    let public_url = env::var("PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());

    // Parse storage path from environment variable
    let storage_path = env::var("STORAGE_PATH").unwrap_or_else(|_| "./files".to_string());
    let upload_dir = PathBuf::from(&storage_path);
    fs::create_dir_all(&upload_dir).await.unwrap();
    info!("⚙️ Storage path: {}", upload_dir.display());

    let file_index = Arc::new(RwLock::new(HashMap::new()));
    build_file_index(&upload_dir, &file_index).await;

    let cleanup_interval_secs = env::var("CLEANUP_INTERVAL_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .expect("Invalid value for CLEANUP_INTERVAL_SECS");

    let max_file_age_days = env::var("MAX_FILE_AGE_DAYS")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .expect("Invalid value for MAX_FILE_AGE_DAYS");

    // Parse max upstream download size in MB
    let max_upstream_download_size_mb = env::var("MAX_UPSTREAM_DOWNLOAD_SIZE_MB")
        .unwrap_or_else(|_| "100".to_string()) // Default: 100MB
        .parse()
        .expect("Invalid value for MAX_UPSTREAM_DOWNLOAD_SIZE_MB");

    // Parse max chunk size in MB for chunked uploads
    let max_chunk_size_mb = env::var("MAX_CHUNK_SIZE_MB")
        .unwrap_or_else(|_| "100".to_string()) // Default: 100MB
        .parse()
        .expect("Invalid value for MAX_CHUNK_SIZE_MB");

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

    if !upstream_servers.is_empty() {
        info!("⚙️ Upstream servers: {:?}", upstream_servers);
        info!(
            "⚙️ Upstream download size limit: {} MB",
            max_upstream_download_size_mb
        );
    }

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

    AppState {
        upload_dir,
        file_index,
        max_total_size,
        max_total_files,
        bind_addr,
        public_url,
        cleanup_interval_secs,
        changes_pending: Arc::new(RwLock::new(true)),
        allowed_pubkeys,
        trusted_pubkeys: Arc::new(RwLock::new(HashMap::new())),
        max_file_age_days,
        files_uploaded: Arc::new(RwLock::new(0)),
        files_downloaded: Arc::new(RwLock::new(0)),
        upload_throughput_data: Arc::new(RwLock::new(Vec::new())),
        download_throughput_data: Arc::new(RwLock::new(Vec::new())),
        upstream_servers,
        max_upstream_download_size_mb,
        max_chunk_size_mb,
        chunk_cleanup_timeout_minutes,
        ongoing_downloads: Arc::new(RwLock::new(HashMap::new())),
        chunk_uploads: Arc::new(RwLock::new(HashMap::new())),
        failed_upstream_lookups: Arc::new(RwLock::new(HashMap::new())),
    }
}

fn start_cleanup_job(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            state.cleanup_interval_secs,
        ));
        loop {
            interval.tick().await;
            let mut changes = state.changes_pending.write().await;
            if *changes {
                enforce_storage_limits(&state).await;
                *changes = false;
            }
            
            // Clean up expired failed upstream lookups
            cleanup_expired_failed_lookups(&state).await;
        }
    });
}

fn start_chunk_cleanup_job(state: AppState) {
    tokio::spawn(async move {
        // Run chunk cleanup every 5 minutes
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5 * 60));
        loop {
            interval.tick().await;
            cleanup_abandoned_chunks(&state).await;
        }
    });
}

fn start_trust_network_refresh_job(state: AppState) {
    tokio::spawn(async move {
        // Only run if ALLOW_WOT is enabled
        if env::var("ALLOW_WOT").is_err() {
            return;
        }

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(4 * 3600));
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = load_app_state().await;
    let addr = state
        .bind_addr
        .parse::<SocketAddr>()
        .expect("Invalid address format");

    start_cleanup_job(state.clone());
    start_chunk_cleanup_job(state.clone());
    start_trust_network_refresh_job(state.clone());

    let app = create_app(state).await;

    info!("🎧 blossom server listening on {}", addr);

    // Create a shutdown signal handler
    let shutdown = signal::ctrl_c();

    // Start the server with graceful shutdown
    let server = axum_server::bind(addr).serve(app.into_make_service());

    // Wait for either the server to complete or a shutdown signal
    tokio::select! {
        _ = server => {
            info!("🎧 Server completed");
        }
        _ = shutdown => {
            info!("🎧 Shutting down gracefully...");
        }
    }
}
