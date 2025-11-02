use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, Method, StatusCode},
    response::Response,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::constants::*;
use crate::helpers::*;
use crate::models::{AppState, FileRequestQuery};
use crate::utils::{find_file, parse_range_header};

/// Handle file requests (GET/HEAD)
pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<FileRequestQuery>,
    req: Request,
) -> Result<Response, StatusCode> {
    // Extract range header for logging
    let range_header = req.headers().get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());

    // Extract origin parameter if provided and feature is enabled
    let custom_origin = if state.feature_custom_upstream_origin_enabled {
        query.origin.as_deref()
    } else {
        if query.origin.is_some() {
            warn!("Origin parameter provided but FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED is disabled, ignoring");
        }
        None
    };

    if let Some(origin) = custom_origin {
        info!("GET request for url: {} (range: {}) with custom origin: {}", filename, range_header, origin);
    } else {
        info!("GET request for url: {} (range: {})", filename, range_header);
    }

    if let Some(filename) = crate::utils::get_sha256_hash_from_filename(&filename) {
        debug!("Found file hash: {}", filename);

        match find_file(&state.file_index, &filename).await {
            Some(file_metadata) => {
                if req.method() == Method::HEAD {
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            header::CONTENT_TYPE,
                            file_metadata
                                .mime_type
                                .unwrap_or_else(|| DEFAULT_MIME_TYPE.into()),
                        )
                        .header(header::CONTENT_LENGTH, file_metadata.size)
                        .body(Body::empty())
                        .unwrap())
                } else {
                    // Track download statistics
                    track_download_stats(&state, file_metadata.size).await;
                    serve_file_with_range(file_metadata.path, req.headers().clone()).await
                }
            }
            None => {
                // File not found locally, check if we've already tried upstream servers recently
                // Skip cache check if a custom origin is provided, as different origins may yield different results
                let should_check_cache = custom_origin.is_none();
                if should_check_cache {
                    let failed_lookups = state.failed_upstream_lookups.read().await;
                    if let Some(failed_time) = failed_lookups.get(&filename) {
                        let one_hour_ago = std::time::Instant::now() - std::time::Duration::from_secs(3600);
                        if *failed_time > one_hour_ago {
                            debug!(
                                "File {} not found in upstream servers recently (cached), returning 404",
                                filename
                            );
                            return Err(StatusCode::NOT_FOUND);
                        }
                    }
                } else {
                    debug!("Skipping failed lookups cache check because custom origin is provided");
                }

                // File not found locally, try upstream servers
                debug!(
                    "File not found locally, checking upstream servers for: {}",
                    filename
                );
                match crate::handlers::upstream::try_upstream_servers(&state, &filename, req.headers(), custom_origin).await {
                    Ok(response) => Ok(response),
                    Err(_) => {
                        // Add to failed lookups cache only if no custom origin was used
                        // (since custom origins may have different success/failure patterns)
                        if custom_origin.is_none() {
                            let mut failed_lookups = state.failed_upstream_lookups.write().await;
                            failed_lookups.insert(filename.clone(), std::time::Instant::now());
                            debug!("Added {} to failed upstream lookups cache", filename);
                        } else {
                            debug!("Skipping failed lookups cache because custom origin was used");
                        }
                        Err(StatusCode::NOT_FOUND)
                    }
                }
            }
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Serve file with range support
async fn serve_file_with_range(path: std::path::PathBuf, headers: axum::http::HeaderMap) -> Result<Response, StatusCode> {
    use axum::http::header::RANGE;
    let range_header = headers.get(RANGE).and_then(|r| r.to_str().ok());
    
    debug!("Serving file: {} (range: {})", path.display(), range_header.unwrap_or("none"));

    let expires_dt = chrono::Utc::now() + chrono::Duration::days(365);
    let expires_str = expires_dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let expires_header = hyper::http::HeaderValue::from_str(&expires_str).unwrap();

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename);

    let mut file = File::open(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_size = metadata.len();

    if let Some(range_header) = range_header {
        if let Some(range) = parse_range_header(range_header, total_size) {
            let (start, end) = range;
            let length = end - start + 1;
            debug!("Serving range: bytes {}-{}/{} (length: {})", start, end, total_size, length);
            
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            let mime = mime_guess::from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or(DEFAULT_MIME_TYPE.into());

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
                .header(axum::http::header::EXPIRES, expires_header.clone())
                .header(header::CONTENT_DISPOSITION, content_disposition.clone())
                .body(body)
                .unwrap());
        }
    }

    info!("Serving full file: {} (size: {} bytes)", path.display(), total_size);
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            mime_guess::from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or(DEFAULT_MIME_TYPE.into()),
        )
        .header(axum::http::header::CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE)
        .header(axum::http::header::EXPIRES, expires_header.clone())
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .body(body)
        .unwrap())
}
