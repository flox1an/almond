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

use crate::constants::{
    CACHE_CONTROL_IMMUTABLE, DEFAULT_MIME_TYPE, FILE_STREAM_BUFFER_SIZE,
    FILE_STREAM_MIN_BUFFER_SIZE,
};
use crate::error::AppError;
use crate::helpers::{get_mime_type, track_download_stats};
use crate::models::{AppState, FileLocation, FileRequestQuery};
use crate::services::blossom_servers;
use crate::services::cashu;
use crate::services::file_storage;
use crate::utils::{find_file, parse_range_header, RangeSpec};

/// Handle file requests (GET/HEAD)
pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<FileRequestQuery>,
    req: Request,
) -> Result<Response, AppError> {
    // Extract range header for logging
    let range_header = req
        .headers()
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map_or_else(|| "none".to_string(), ToString::to_string);

    // First, check if file exists locally - if it does, serve it immediately without upstream lookup
    if let Some(file_hash) = crate::utils::get_sha256_hash_from_filename(&filename) {
        debug!("Found file hash: {}", file_hash);

        let native_file = find_file(&state.file_index, file_hash).await;
        let file_metadata = if native_file.is_some() {
            native_file
        } else if let Some(s3) = &state.native_s3 {
            match s3.find(file_hash).await? {
                Some(metadata) => Some(
                    file_storage::publish_existing_metadata(&state, file_hash.to_owned(), metadata)
                        .await?,
                ),
                None => None,
            }
        } else {
            None
        };
        if let Some(file_metadata) = file_metadata {
            // File is available locally - serve it immediately, skip all upstream logic
            debug!(
                "File {} found locally, serving immediately (skipping upstream lookup)",
                file_hash
            );

            let etag = blob_etag(file_hash);
            if let Some(response) = check_not_modified(req.headers(), &etag)? {
                return Ok(response);
            }

            if req.method() == Method::HEAD {
                return build_blob_head_response(
                    file_metadata.mime_type.as_deref(),
                    file_metadata.size,
                    &etag,
                );
            }

            // Check payment for download if required
            if state.feature_paid_download {
                let required_sats =
                    cashu::calculate_price(file_metadata.size, state.cashu_price_per_mb);
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
                        cashu::verify_token_basics(
                            &token,
                            required_sats,
                            &state.cashu_accepted_mints,
                        )?;

                        if let Some(wallet) = &state.cashu_wallet {
                            cashu::receive_token(wallet, &token).await?;
                        }
                    }
                }
            }

            // Track download statistics
            track_download_stats(&state, file_metadata.size);
            match &file_metadata.location {
                FileLocation::Local(path) => {
                    serve_file_with_range(
                        path,
                        file_metadata.mime_type.as_deref(),
                        req.headers(),
                        &etag,
                    )
                    .await
                }
                FileLocation::S3 { key } => {
                    serve_s3_with_range(
                        state
                            .native_s3
                            .as_ref()
                            .expect("S3 indexed without configured backend"),
                        key,
                        file_metadata.mime_type.as_deref(),
                        file_metadata.size,
                        req.headers(),
                        &etag,
                    )
                    .await
                }
            }
        } else {
            if let Some(serve_file_metadata) =
                crate::services::serve_files::get_serve_file(&state.serve_file_index, file_hash)
                    .await
            {
                debug!(
                    "File {} found in serve files index, serving read-only",
                    file_hash
                );

                let etag = blob_etag(file_hash);
                if let Some(response) = check_not_modified(req.headers(), &etag)? {
                    return Ok(response);
                }

                if req.method() == Method::HEAD {
                    return build_blob_head_response(
                        serve_file_metadata.mime_type.as_deref(),
                        serve_file_metadata.size,
                        &etag,
                    );
                }

                if state.feature_paid_download {
                    let required_sats =
                        cashu::calculate_price(serve_file_metadata.size, state.cashu_price_per_mb);
                    let cashu_header = cashu::extract_cashu_header(req.headers());

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
                            cashu::verify_token_basics(
                                &token,
                                required_sats,
                                &state.cashu_accepted_mints,
                            )?;

                            if let Some(wallet) = &state.cashu_wallet {
                                cashu::receive_token(wallet, &token).await?;
                            }
                        }
                    }
                }

                track_download_stats(&state, serve_file_metadata.size);
                return serve_file_with_range(
                    &serve_file_metadata.path,
                    serve_file_metadata.mime_type.as_deref(),
                    req.headers(),
                    &etag,
                )
                .await;
            }

            // File not found locally - now do upstream server lookup
            debug!(
                "File {} not found locally, checking upstream servers",
                file_hash
            );

            // Check if custom upstream origin feature is enabled
            let upstream_feature_enabled =
                state.feature_custom_upstream_origin_enabled.is_enabled();
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
                            debug!(
                                "Parsed author pubkey: {} (from as parameter)",
                                pubkey.to_hex()
                            );

                            // If WOT mode is enabled, validate the pubkey is in WOT
                            if upstream_requires_wot {
                                let is_authorized =
                                    crate::services::auth::is_pubkey_authorized(&pubkey, &state)
                                        .await;
                                if !is_authorized {
                                    warn!("Author pubkey {} not in Web of Trust, rejecting upstream lookup", pubkey.to_hex());
                                    return Err(AppError::Forbidden(
                                        "Author pubkey not in Web of Trust".to_string(),
                                    ));
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
                info!(
                    "GET request for url: {} (range: {}) with custom origin: {}",
                    filename, range_header, origin
                );
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
                info!(
                    "GET request for url: {} (range: {}) with author pubkey for lazy server fetch",
                    filename, range_header
                );
            } else {
                info!(
                    "GET request for url: {} (range: {})",
                    filename, range_header
                );
            }
            // Check if we've already tried upstream servers recently
            // Skip cache check if custom origin or xs servers are provided, as different servers may yield different results
            let should_check_cache = custom_origin.is_none() && xs_servers_to_use.is_none();
            if should_check_cache {
                let failed_lookups = state.failed_upstream_lookups.read().await;
                if let Some(failed_time) = failed_lookups.get(file_hash) {
                    let one_hour_ago = std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_secs(3600))
                        .unwrap();
                    if *failed_time > one_hour_ago {
                        debug!(
                            "File {} not found in upstream servers recently (cached), returning 404",
                            file_hash
                        );
                        return Err(AppError::NotFound(
                            "File not found (cached upstream failure)".to_string(),
                        ));
                    }
                }
            } else {
                debug!("Skipping failed lookups cache check because custom origin or xs servers are provided");
            }

            // Try upstream servers with prioritization: xs → UPSTREAM_SERVERS → user servers (lazy)
            // Branch based on upstream mode: proxy vs redirect
            let upstream_result = if state.upstream_mode.is_redirect() {
                // Redirect mode: HEAD check then 302 redirect
                debug!(
                    "Using upstream redirect mode (cache_in_background: {})",
                    state.upstream_mode.caches_in_background()
                );
                crate::handlers::upstream::try_upstream_redirect(
                    &state,
                    &filename,
                    custom_origin,
                    xs_servers_to_use,
                    author_pubkey.as_ref(),
                    state.upstream_mode.caches_in_background(),
                )
                .await
            } else {
                // Proxy mode: stream from upstream while saving locally (default)
                crate::handlers::upstream::try_upstream_servers(
                    &state,
                    &filename,
                    req.headers(),
                    req.method(),
                    custom_origin,
                    xs_servers_to_use,
                    author_pubkey.as_ref(),
                )
                .await
            };

            if let Ok(response) = upstream_result {
                Ok(response)
            } else {
                // Add to failed lookups cache only if no custom origin or xs servers were used
                // (since custom servers may have different success/failure patterns)
                if custom_origin.is_none() && xs_servers_to_use.is_none() {
                    let mut failed_lookups = state.failed_upstream_lookups.write().await;
                    failed_lookups.insert(file_hash.to_string(), std::time::Instant::now());
                    debug!("Added {} to failed upstream lookups cache", file_hash);
                } else {
                    debug!("Skipping failed lookups cache because custom origin or xs servers were used");
                }
                Err(AppError::NotFound("File not found".to_string()))
            }
        }
    } else {
        // Invalid filename format (no hash found)
        Err(AppError::NotFound("Invalid filename format".to_string()))
    }
}

/// `ETag` for a content-addressed blob. The SHA-256 *is* the strong validator,
/// so revalidation never needs to touch the file.
fn blob_etag(sha256: &str) -> String {
    format!("\"{}\"", sha256)
}

/// RFC 9110 §8.8.3.2 weak comparison of an entity-tag list against our tag.
fn etag_list_matches(list: &str, etag: &str) -> bool {
    list.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/") == etag
    })
}

/// Answer `If-None-Match` with `304` when the client already holds the blob.
/// Blobs are immutable and hash-addressed, so a tag match is always current.
fn check_not_modified(
    headers: &axum::http::HeaderMap,
    etag: &str,
) -> Result<Option<Response>, AppError> {
    let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    if !etag_list_matches(if_none_match, etag) {
        return Ok(None);
    }

    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .body(Body::empty())
        .map(Some)
        .map_err(|e| AppError::InternalError(format!("Failed to build 304 response: {}", e)))
}

fn build_blob_head_response(
    mime_type: Option<&str>,
    size: u64,
    etag: &str,
) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type.unwrap_or(DEFAULT_MIME_TYPE))
        .header(header::CONTENT_LENGTH, size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .body(Body::empty())
        .map_err(|e| AppError::InternalError(format!("Failed to build HEAD response: {}", e)))
}

/// Read-buffer size for a body of `len` bytes: large enough to amortise the
/// blocking-pool dispatch behind every `tokio::fs` read, never larger than the
/// response itself.
fn stream_buffer_size(len: u64) -> usize {
    (len.min(FILE_STREAM_BUFFER_SIZE as u64) as usize).max(FILE_STREAM_MIN_BUFFER_SIZE)
}

/// Serve file with range support.
///
/// `mime_type` comes from the index when known; deriving it from the path is
/// only a fallback, since `mime_guess` allocates on every call.
async fn serve_file_with_range(
    path: &std::path::Path,
    mime_type: Option<&str>,
    headers: &axum::http::HeaderMap,
    etag: &str,
) -> Result<Response, AppError> {
    let range_header = headers.get(header::RANGE).and_then(|r| r.to_str().ok());

    debug!(
        "Serving file: {} (range: {})",
        path.display(),
        range_header.unwrap_or("none")
    );

    let expires_header = crate::helpers::immutable_expires_header();
    let content_type = match mime_type {
        Some(mime) => mime.to_string(),
        None => get_mime_type(path),
    };

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename);

    let mut file = File::open(path)
        .await
        .map_err(|e| AppError::IoError(format!("Failed to open file: {}", e)))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|e| AppError::IoError(format!("Failed to read file metadata: {}", e)))?;
    let total_size = metadata.len();

    // A stale `If-Range` validator means the client's partial copy is not the
    // blob we hold, so the range must be ignored and the full body sent.
    let honor_range = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| etag_list_matches(v, etag));

    let range = match (honor_range, range_header) {
        (true, Some(value)) => parse_range_header(value, total_size),
        _ => RangeSpec::Ignore,
    };

    match range {
        RangeSpec::Satisfiable { start, end } => {
            let length = end - start + 1;
            debug!(
                "Serving range: bytes {}-{}/{} (length: {})",
                start, end, total_size, length
            );

            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| AppError::IoError(format!("Failed to seek in file: {}", e)))?;
            let stream = ReaderStream::with_capacity(file.take(length), stream_buffer_size(length));
            let body = Body::from_stream(stream);

            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .header(header::CONTENT_LENGTH, length)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, etag)
                .header(header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
                .header(header::EXPIRES, expires_header)
                .header(header::CONTENT_DISPOSITION, content_disposition)
                .body(body)
                .map_err(|e| {
                    AppError::InternalError(format!("Failed to build range response: {}", e))
                })
        }
        RangeSpec::Unsatisfiable => {
            debug!(
                "Unsatisfiable range {:?} for {} ({} bytes)",
                range_header,
                path.display(),
                total_size
            );
            Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", total_size))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, etag)
                .body(Body::empty())
                .map_err(|e| {
                    AppError::InternalError(format!("Failed to build 416 response: {}", e))
                })
        }
        RangeSpec::Ignore => {
            debug!(
                "Serving full file: {} (size: {} bytes)",
                path.display(),
                total_size
            );
            let stream = ReaderStream::with_capacity(file, stream_buffer_size(total_size));
            let body = Body::from_stream(stream);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, total_size)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, etag)
                .header(header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
                .header(header::EXPIRES, expires_header)
                .header(header::CONTENT_DISPOSITION, content_disposition)
                .body(body)
                .map_err(|e| {
                    AppError::InternalError(format!("Failed to build file response: {}", e))
                })
        }
    }
}

async fn serve_s3_with_range(
    storage: &crate::services::native_storage::NativeS3Storage,
    key: &str,
    mime_type: Option<&str>,
    total_size: u64,
    headers: &axum::http::HeaderMap,
    etag: &str,
) -> Result<Response, AppError> {
    let honor_range = headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| etag_list_matches(value, etag));
    let range = match (
        honor_range,
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
    ) {
        (true, Some(value)) => parse_range_header(value, total_size),
        _ => RangeSpec::Ignore,
    };
    let (s3_range, status, content_length, content_range) = match range {
        RangeSpec::Satisfiable { start, end } => (
            Some(format!("bytes={start}-{end}")),
            StatusCode::PARTIAL_CONTENT,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{total_size}")),
        ),
        RangeSpec::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{total_size}"))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, etag)
                .body(Body::empty())
                .map_err(|error| {
                    AppError::InternalError(format!("Failed to build S3 range response: {error}"))
                });
        }
        RangeSpec::Ignore => (None, StatusCode::OK, total_size, None),
    };
    let output = storage.get(key, s3_range.as_deref()).await?;
    let stream = ReaderStream::new(output.body.into_async_read());
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime_type.unwrap_or(DEFAULT_MIME_TYPE))
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .body(Body::from_stream(stream))
        .map_err(|error| {
            AppError::InternalError(format!("Failed to build S3 response: {error}"))
        })?;
    if let Some(content_range) = content_range {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            content_range.parse().map_err(|error| {
                AppError::InternalError(format!("Invalid S3 content range: {error}"))
            })?,
        );
    }
    Ok(response)
}
