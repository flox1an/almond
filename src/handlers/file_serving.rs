use axum::{
    body::Body,
    extract::{Path as AxumPath, Request, State},
    http::{header, Method, StatusCode},
    response::Response,
};
use axum_extra::extract::Query;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::constants::*;
use crate::helpers::{get_mime_type, track_download_stats};
use crate::models::{AppState, FileRequestQuery};
use crate::utils::{find_file, parse_range_header};
use crate::services::blossom_servers;
use crate::services::cashu;
use crate::error::AppError;

/// Handle file requests (GET/HEAD)
pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<FileRequestQuery>,
    req: Request,
) -> Result<Response, AppError> {
    // Extract range header for logging
    let range_header = req.headers().get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());

    // First, check if file exists locally - if it does, serve it immediately without upstream lookup
    if let Some(file_hash) = crate::utils::get_sha256_hash_from_filename(&filename) {
        debug!("Found file hash: {}", file_hash);

        match find_file(&state.file_index, &file_hash).await {
            Some(file_metadata) => {
                // File is available locally - serve it immediately, skip all upstream logic
                info!("File {} found locally, serving immediately (skipping upstream lookup)", file_hash);
                
                if req.method() == Method::HEAD {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            header::CONTENT_TYPE,
                            file_metadata
                                .mime_type
                                .unwrap_or_else(|| DEFAULT_MIME_TYPE.into()),
                        )
                        .header(header::CONTENT_LENGTH, file_metadata.size)
                        .header(header::ACCEPT_RANGES, "bytes")
                        .body(Body::empty())
                        .map_err(|e| AppError::InternalError(format!("Failed to build HEAD response: {}", e)))
                } else {
                    // Check payment for download if required
                    if state.feature_paid_download {
                        let required_sats = cashu::calculate_price(file_metadata.size, state.cashu_price_per_mb);
                        let headers = req.headers();
                        let cashu_header = cashu::extract_cashu_header(headers);

                        match cashu_header {
                            None => {
                                return Err(AppError::PaymentRequired {
                                    amount_sats: required_sats,
                                    unit: "sat".to_string(),
                                    mints: state.cashu_accepted_mints.clone(),
                                });
                            }
                            Some(token_str) => {
                                let token = cashu::parse_token(&token_str)?;
                                cashu::verify_token_basics(&token, required_sats, &state.cashu_accepted_mints)?;

                                if let Some(wallet) = &state.cashu_wallet {
                                    cashu::receive_token(wallet, &token).await?;
                                }
                            }
                        }
                    }

                    // Track download statistics
                    track_download_stats(&state, file_metadata.size).await;
                    serve_file_with_range(file_metadata.path, req.headers().clone()).await
                }
            }
            None => {
                // File not found locally - now do upstream server lookup
                info!("File {} not found locally, checking upstream servers", file_hash);

                // Check if custom upstream origin feature is enabled
                let upstream_feature_enabled = state.feature_custom_upstream_origin_enabled.is_enabled();
                let upstream_requires_wot = state.feature_custom_upstream_origin_enabled.requires_wot();

                // Extract custom origin (single server) if provided and feature is enabled
                let custom_origin = if upstream_feature_enabled {
                    query.origin.as_deref()
                } else {
                    if query.origin.is_some() {
                        warn!("Origin parameter provided but FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED is disabled, ignoring");
                    }
                    None
                };

                // Extract xs (servers) parameters - multiple xs query parameters can be provided per BUD-01
                // xs takes priority, then fall back to legacy servers parameter
                let xs_servers = if upstream_feature_enabled {
                    if !query.xs.is_empty() {
                        Some(&query.xs[..])
                    } else if !query.servers.is_empty() {
                        Some(&query.servers[..])
                    } else {
                        None
                    }
                } else {
                    if !query.xs.is_empty() || !query.servers.is_empty() {
                        warn!("Server parameters provided but FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED is disabled, ignoring");
                    }
                    None
                };

                // Parse author pubkey if provided (BUD-03) - we'll fetch servers lazily only if needed
                let author_pubkey = if query.author_pubkey.is_some() && upstream_feature_enabled {
                    if let Some(author_str) = &query.author_pubkey {
                        match blossom_servers::parse_pubkey(author_str) {
                            Ok(pubkey) => {
                                debug!("Parsed author pubkey: {} (from as parameter)", pubkey.to_hex());

                                // If WOT mode is enabled, validate the pubkey is in WOT
                                if upstream_requires_wot {
                                    let is_authorized = crate::services::auth::is_pubkey_authorized(&pubkey, &state).await;
                                    if !is_authorized {
                                        warn!("Author pubkey {} not in Web of Trust, rejecting upstream lookup", pubkey.to_hex());
                                        return Err(AppError::Forbidden("Author pubkey not in Web of Trust".to_string()));
                                    }
                                    debug!("Author pubkey {} validated in WOT", pubkey.to_hex());
                                }

                                Some(pubkey)
                            }
                            Err(e) => {
                                warn!("Invalid pubkey in 'as' parameter: {} ({})", author_str, e);
                                None
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                // We only prepare xs servers here, NOT user servers (lazy fetch in upstream.rs)
                let xs_servers_to_use = xs_servers;

                // Log the request with appropriate context
                if let Some(origin) = custom_origin {
                    info!("GET request for url: {} (range: {}) with custom origin: {}", filename, range_header, origin);
                } else if let Some(servers) = xs_servers_to_use {
                    info!(
                        "GET request for url: {} (range: {}) with xs servers ({} servers): {:?}",
                        filename,
                        range_header,
                        servers.len(),
                        servers
                    );
                    if author_pubkey.is_some() {
                        debug!("Request includes author pubkey (as) for lazy fetch if needed");
                    }
                } else if author_pubkey.is_some() {
                    info!("GET request for url: {} (range: {}) with author pubkey for lazy server fetch", filename, range_header);
                } else {
                    info!("GET request for url: {} (range: {})", filename, range_header);
                }
                // Check if we've already tried upstream servers recently
                // Skip cache check if custom origin or xs servers are provided, as different servers may yield different results
                let should_check_cache = custom_origin.is_none() && xs_servers_to_use.is_none();
                if should_check_cache {
                    let failed_lookups = state.failed_upstream_lookups.read().await;
                    if let Some(failed_time) = failed_lookups.get(&file_hash) {
                        let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
                        if *failed_time > one_hour_ago {
                            debug!(
                                "File {} not found in upstream servers recently (cached), returning 404",
                                file_hash
                            );
                            return Err(AppError::NotFound("File not found (cached upstream failure)".to_string()));
                        }
                    }
                } else {
                    debug!("Skipping failed lookups cache check because custom origin or xs servers are provided");
                }

                // Try upstream servers with prioritization: xs → UPSTREAM_SERVERS → user servers (lazy)
                // Branch based on upstream mode: proxy vs redirect
                let upstream_result = if state.upstream_mode.is_redirect() {
                    // Redirect mode: HEAD check then 302 redirect
                    info!("Using upstream redirect mode (cache_in_background: {})", state.upstream_mode.caches_in_background());
                    crate::handlers::upstream::try_upstream_redirect(
                        &state,
                        &filename,
                        custom_origin,
                        xs_servers_to_use,
                        author_pubkey.as_ref(),
                        state.upstream_mode.caches_in_background(),
                    ).await
                } else {
                    // Proxy mode: stream from upstream while saving locally (default)
                    crate::handlers::upstream::try_upstream_servers(
                        &state,
                        &filename,
                        req.headers(),
                        custom_origin,
                        xs_servers_to_use,
                        author_pubkey.as_ref(),
                    ).await
                };

                match upstream_result {
                    Ok(response) => Ok(response),
                    Err(_) => {
                        // Add to failed lookups cache only if no custom origin or xs servers were used
                        // (since custom servers may have different success/failure patterns)
                        if custom_origin.is_none() && xs_servers_to_use.is_none() {
                            let mut failed_lookups = state.failed_upstream_lookups.write().await;
                            failed_lookups.insert(file_hash.clone(), std::time::Instant::now());
                            debug!("Added {} to failed upstream lookups cache", file_hash);
                        } else {
                            debug!("Skipping failed lookups cache because custom origin or xs servers were used");
                        }
                        Err(AppError::NotFound("File not found".to_string()))
                    }
                }
            }
        }
    } else {
        // Invalid filename format (no hash found)
        Err(AppError::NotFound("Invalid filename format".to_string()))
    }
}

/// Serve file with range support
async fn serve_file_with_range(path: std::path::PathBuf, headers: axum::http::HeaderMap) -> Result<Response, AppError> {
    use axum::http::header::RANGE;
    let range_header = headers.get(RANGE).and_then(|r| r.to_str().ok());

    debug!("Serving file: {} (range: {})", path.display(), range_header.unwrap_or("none"));

    let expires_dt = chrono::Utc::now() + chrono::Duration::days(365);
    let expires_str = expires_dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let expires_header = hyper::http::HeaderValue::from_str(&expires_str)
        .map_err(|e| AppError::InternalError(format!("Failed to create expires header: {}", e)))?;

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename);

    let mut file = File::open(&path)
        .await
        .map_err(|e| AppError::IoError(format!("Failed to open file: {}", e)))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|e| AppError::IoError(format!("Failed to read file metadata: {}", e)))?;
    let total_size = metadata.len();

    if let Some(range_header) = range_header {
        if let Some(range) = parse_range_header(range_header, total_size) {
            let (start, end) = range;
            let length = end - start + 1;
            debug!("Serving range: bytes {}-{}/{} (length: {})", start, end, total_size, length);

            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| AppError::IoError(format!("Failed to seek in file: {}", e)))?;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            let mime = get_mime_type(&path);

            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
                .header(axum::http::header::EXPIRES, expires_header.clone())
                .header(header::CONTENT_DISPOSITION, content_disposition.clone())
                .body(body)
                .map_err(|e| AppError::InternalError(format!("Failed to build range response: {}", e)));
        }
    }

    info!("Serving full file: {} (size: {} bytes)", path.display(), total_size);
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, get_mime_type(&path))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .header(axum::http::header::EXPIRES, expires_header.clone())
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .body(body)
        .map_err(|e| AppError::InternalError(format!("Failed to build file response: {}", e)))
}
