use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
    Json,
};
use futures_util::StreamExt;
use reqwest::{header as reqwest_header, Client};
use serde_json::json;
use sha2::Digest;
use tokio::{fs::File, io::AsyncWriteExt};
use tracing::{debug, error, info, warn};

use crate::services::upstream_candidates;

use crate::constants::CACHE_CONTROL_IMMUTABLE;
use crate::helpers::{
    copy_headers_to_reqwest, copy_headers_without_range, extract_content_type_from_response,
    get_extension_from_mime,
};
use crate::models::{
    AppState, DownloadHandle, DownloadPhase, NegotiationPhase, UpstreamNegotiation,
};
use crate::services::download::{
    claim_upstream_negotiation, NegotiationClaim, NegotiationGuard, PreparedDownload,
};
use crate::services::tail::create_tailing_stream;
use crate::utils::{get_sha256_hash_from_filename, parse_range_header, RangeSpec};

const SEEK_AHEAD_LIMIT: u64 = 8 * 1024 * 1024;
const NEGOTIATION_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn coalescible_range_start(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(header::RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes=")?;
    if range.contains(',') {
        return None;
    }
    let (start, _) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    (start <= SEEK_AHEAD_LIMIT).then_some(start)
}

fn copy_headers_for_cold_fetch(
    headers: &HeaderMap,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    if coalescible_range_start(headers).is_some() {
        copy_headers_without_range(headers, request).header(reqwest_header::RANGE, "bytes=0-")
    } else {
        copy_headers_to_reqwest(headers, request)
    }
}

async fn wait_for_negotiation(
    negotiation: &UpstreamNegotiation,
    timeout: std::time::Duration,
) -> NegotiationPhase {
    let mut phase = negotiation.phase.subscribe();
    let wait = async {
        loop {
            let current = *phase.borrow_and_update();
            if current != NegotiationPhase::Pending {
                return current;
            }
            if phase.changed().await.is_err() {
                return NegotiationPhase::Failed;
            }
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .unwrap_or(NegotiationPhase::Pending)
}

async fn serve_existing_download(
    handle: &DownloadHandle,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
) -> Option<Result<Response<Body>, StatusCode>> {
    if *method == Method::HEAD {
        return Some(serve_head_download(handle, filename));
    }
    if headers.get(header::RANGE).is_none() {
        Some(serve_non_range_download(handle, filename).await)
    } else {
        serve_range_download(handle, filename, headers).await
    }
}

fn track_coalesced_response(state: &AppState, method: &Method, response: &Response<Body>) {
    state.metrics.track_coalesced_request();
    if *method == Method::HEAD {
        return;
    }
    if let Some(length) = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        state.metrics.track_download(length);
    } else {
        state.metrics.files_downloaded.inc();
    }
}

/// Handle upstream servers requests
pub async fn get_upstream(
    State(state): State<AppState>,
    _headers: HeaderMap,
) -> Json<serde_json::Value> {
    let upstream_servers = &state.upstream_servers;

    let response = json!({
        "upstream_servers": upstream_servers,
        "count": upstream_servers.len(),
        "max_download_size_mb": state.max_upstream_download_size_mb
    });

    Json(response)
}

/// Try to fetch file from upstream servers, stream it to client and save locally
/// Prioritization: `custom_origin` → `xs_servers` → `UPSTREAM_SERVERS` → user servers (lazy fetch)
pub async fn try_upstream_servers(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_relay_pool::prelude::PublicKey>,
) -> Result<Response, StatusCode> {
    // Forward range requests to upstream servers
    if headers.get(header::RANGE).is_some() {
        debug!("Range request detected, forwarding to upstream server");
    }

    // Extract hash from filename for internal tracking (ongoing downloads, file index, etc.)
    // But use the full filename (with extension) for upstream URL construction
    let file_hash = get_sha256_hash_from_filename(filename).unwrap_or(filename);

    if let Some(handle) = state.ongoing_downloads.read().await.get(file_hash).cloned() {
        if let Some(response) = serve_existing_download(&handle, filename, headers, method).await {
            if let Ok(response) = &response {
                track_coalesced_response(state, method, response);
            }
            debug!("Attaching request to in-flight download {}", file_hash);
            return response;
        }
        return proxy_request_to_upstream(
            state,
            filename,
            headers,
            custom_origin,
            xs_servers,
            author_pubkey,
        )
        .await;
    }

    let should_coalesce =
        headers.get(header::RANGE).is_none() || coalescible_range_start(headers).is_some();
    let negotiation = if should_coalesce {
        loop {
            match claim_upstream_negotiation(state, file_hash).await {
                NegotiationClaim::Leader(guard) => break Some(guard),
                NegotiationClaim::Follower(existing) => {
                    match wait_for_negotiation(&existing, NEGOTIATION_WAIT_TIMEOUT).await {
                        NegotiationPhase::Ready => {
                            if let Some(handle) =
                                state.ongoing_downloads.read().await.get(file_hash).cloned()
                            {
                                if let Some(response) =
                                    serve_existing_download(&handle, filename, headers, method)
                                        .await
                                {
                                    if let Ok(response) = &response {
                                        track_coalesced_response(state, method, response);
                                    }
                                    return response;
                                }
                                return proxy_request_to_upstream(
                                    state,
                                    filename,
                                    headers,
                                    custom_origin,
                                    xs_servers,
                                    author_pubkey,
                                )
                                .await;
                            }
                        }
                        NegotiationPhase::Failed => continue,
                        NegotiationPhase::Pending => {
                            debug!(
                                "Timed out waiting for upstream negotiation for {}",
                                file_hash
                            );
                            break None;
                        }
                    }
                }
            }
        }
    } else {
        None
    };

    let client = state.upstream_client.clone();
    let mut walk = upstream_candidates::Walk::new(
        state,
        filename,
        custom_origin,
        xs_servers,
        author_pubkey,
    );

    while let Some(candidate) = walk.next().await {
        let request = copy_headers_for_cold_fetch(headers, client.get(&candidate.url));
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                debug!("Found file on {}: {}", candidate.tier.label(), candidate.url);
                return handle_successful_upstream_response(
                    state,
                    &client,
                    response,
                    &candidate.url,
                    file_hash,
                    filename,
                    headers,
                    method,
                    negotiation,
                )
                .await;
            }
            Ok(response) => debug!(
                "{} {} returned status: {}",
                candidate.tier.label(),
                candidate.url,
                response.status()
            ),
            Err(error) => debug!(
                "Failed to fetch from {} {}: {error}",
                candidate.tier.label(),
                candidate.url
            ),
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Try to find file on upstream servers and return a 302 redirect response
/// Prioritization: `custom_origin` → `xs_servers` → `UPSTREAM_SERVERS` → user servers (lazy fetch)
/// Uses HEAD requests to verify file exists before redirecting
pub async fn try_upstream_redirect(
    state: &AppState,
    filename: &str,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_relay_pool::prelude::PublicKey>,
    cache_in_background: bool,
) -> Result<Response, StatusCode> {
    // Extract hash from filename for internal tracking
    let file_hash = get_sha256_hash_from_filename(filename).unwrap_or(filename);

    // Check if this file is already being downloaded
    if state.ongoing_downloads.read().await.contains_key(file_hash) {
        debug!(
            "File {} is already being downloaded in background, redirecting to upstream",
            file_hash
        );
    }

    let client = state.upstream_client.clone();
    let mut walk = upstream_candidates::Walk::new(
        state,
        filename,
        custom_origin,
        xs_servers,
        author_pubkey,
    );

    while let Some(candidate) = walk.next().await {
        if let Some(response) = try_head_and_redirect(
            state,
            &client,
            &candidate.url,
            file_hash,
            filename,
            cache_in_background,
        )
        .await
        {
            return Ok(response);
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Try HEAD request and return redirect response if successful
async fn try_head_and_redirect(
    state: &AppState,
    client: &Client,
    file_url: &str,
    file_hash: &str,
    filename: &str,
    cache_in_background: bool,
) -> Option<Response<Body>> {
    match client.head(file_url).send().await {
        Ok(response) if response.status().is_success() => {
            debug!("HEAD check succeeded for: {}", file_url);

            // Get content type and size from HEAD response for background download
            let content_type = extract_content_type_from_response(response.headers());
            let content_length = response.content_length();

            // Start background download if requested and not already downloading
            if cache_in_background {
                let already_downloading =
                    state.ongoing_downloads.read().await.contains_key(file_hash);

                if already_downloading {
                    debug!(
                        "File {} already being downloaded, skipping duplicate background download",
                        file_hash
                    );
                } else {
                    // Check size limit before starting background download
                    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
                    let should_cache = match content_length {
                        Some(len) if len > max_size_bytes => {
                            debug!(
                                "File {} too large for background cache: {} bytes (max: {} MB)",
                                file_hash, len, state.max_upstream_download_size_mb
                            );
                            false
                        }
                        _ => true,
                    };

                    if should_cache {
                        debug!("Starting background download for caching: {}", file_hash);
                        let state_clone = state.clone();
                        let file_url_clone = file_url.to_string();
                        let file_hash_clone = file_hash.to_string();
                        let content_type_clone = content_type.clone();

                        tokio::spawn(async move {
                            start_background_download_for_redirect(
                                &state_clone,
                                &file_url_clone,
                                &file_hash_clone,
                                &content_type_clone,
                            )
                            .await;
                        });
                    }
                }
            }

            // Build 302 redirect response
            match build_redirect_response(file_url, filename) {
                Ok(response) => Some(response),
                Err(e) => {
                    error!("Failed to build redirect response: {}", e);
                    None
                }
            }
        }
        Ok(response) => {
            debug!(
                "HEAD check failed for {} with status: {}",
                file_url,
                response.status()
            );
            None
        }
        Err(e) => {
            debug!("HEAD request failed for {}: {}", file_url, e);
            None
        }
    }
}

/// Build a 302 redirect response to the upstream URL
fn build_redirect_response(
    upstream_url: &str,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    debug!("Redirecting to upstream: {}", upstream_url);

    // Extract clean filename for logging
    let clean_filename = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    Response::builder()
        .status(StatusCode::FOUND) // 302
        .header(header::LOCATION, upstream_url)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", clean_filename),
        )
        .body(Body::empty())
        .map_err(|e| {
            error!("Failed to build redirect response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Start a background download for caching after redirect
async fn start_background_download_for_redirect(
    state: &AppState,
    file_url: &str,
    file_hash: &str,
    content_type: &str,
) {
    let client = state.upstream_client.clone();

    // Make GET request to download the file
    match client.get(file_url).send().await {
        Ok(response) if response.status().is_success() => {
            debug!("Starting background download from: {}", file_url);

            let total_len = response.content_length();
            match prepare_download_state(state, file_hash, content_type, total_len).await {
                Ok(prepared) => {
                    download_file_from_upstream_background(
                        state, file_url, response, file_hash, prepared,
                    )
                    .await;
                }
                Err(e) => {
                    error!(
                        "Failed to prepare download state for {}: {:?}",
                        file_hash, e
                    );
                }
            }
        }
        Ok(response) => {
            warn!(
                "Background download GET failed for {} with status: {}",
                file_url,
                response.status()
            );
        }
        Err(e) => {
            warn!(
                "Background download GET request failed for {}: {}",
                file_url, e
            );
        }
    }
}

/// Handle successful upstream response (consolidates range and non-range logic)
async fn handle_successful_upstream_response(
    state: &AppState,
    client: &Client,
    response: reqwest::Response,
    file_url: &str,
    file_hash: &str,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
    negotiation: Option<NegotiationGuard>,
) -> Result<Response, StatusCode> {
    let content_type = extract_content_type_from_response(response.headers());
    if *method == Method::HEAD {
        let total_len = upstream_response_total_len(&response);
        let prepared = prepare_download_state(state, file_hash, &content_type, total_len).await?;
        let handle = prepared.handle.clone();
        let head_response = serve_head_download(&handle, filename)?;
        if let Some(negotiation) = negotiation {
            negotiation.finish(NegotiationPhase::Ready).await;
        }

        let state_clone = state.clone();
        let file_url_clone = file_url.to_string();
        let file_hash_clone = file_hash.to_string();
        tokio::spawn(async move {
            download_file_from_upstream_background(
                &state_clone,
                &file_url_clone,
                response,
                &file_hash_clone,
                prepared,
            )
            .await;
        });
        return Ok(head_response);
    }
    if headers.get(header::RANGE).is_none() {
        let prepared =
            prepare_download_state(state, file_hash, &content_type, response.content_length())
                .await?;
        return stream_and_save_from_upstream(
            state,
            file_url,
            response,
            file_hash,
            prepared,
            negotiation,
        )
        .await;
    }

    // Far seeks and suffix/multi-ranges were sent upstream unchanged. They are
    // latency-sensitive and intentionally bypass the sequential cache fill.
    if coalescible_range_start(headers).is_none() {
        return proxy_upstream_response(response, &content_type, filename).await;
    }

    let total_len = upstream_response_total_len(&response);
    let Some(total_len) = total_len else {
        // A compliant 206 carries the complete length in Content-Range. If the
        // origin supplied neither it nor Content-Length, issue the original
        // range request so the client still receives correct range semantics.
        if let Some(negotiation) = negotiation {
            negotiation.finish(NegotiationPhase::Failed).await;
        }
        let request = copy_headers_to_reqwest(headers, client.get(file_url));
        let response = request.send().await.map_err(|error| {
            warn!("Failed range fallback for {}: {}", file_url, error);
            StatusCode::BAD_GATEWAY
        })?;
        return proxy_upstream_response(response, &content_type, filename).await;
    };

    let prepared = prepare_download_state(state, file_hash, &content_type, Some(total_len)).await?;
    let handle = prepared.handle.clone();
    let range_response = serve_range_download(&handle, filename, headers)
        .await
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)??;

    if let Some(negotiation) = negotiation {
        negotiation.finish(NegotiationPhase::Ready).await;
    }
    let state_clone = state.clone();
    let file_url_clone = file_url.to_string();
    let file_hash_clone = file_hash.to_string();
    tokio::spawn(async move {
        download_file_from_upstream_background(
            &state_clone,
            &file_url_clone,
            response,
            &file_hash_clone,
            prepared,
        )
        .await;
    });
    Ok(range_response)
}

fn upstream_response_total_len(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest_header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(_, total)| total.parse::<u64>().ok())
        .or_else(|| response.content_length())
}

async fn open_download_file(handle: &DownloadHandle) -> std::io::Result<File> {
    match File::open(&handle.temp_path).await {
        Ok(file) => Ok(file),
        Err(temp_error) => {
            if let Some(final_path) = handle.final_path.get() {
                File::open(final_path).await
            } else {
                Err(temp_error)
            }
        }
    }
}

fn serve_head_download(
    handle: &DownloadHandle,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    response = apply_streaming_headers(response, &handle.content_type, filename);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    if let Some(length) = handle.total_len {
        if let Ok(value) = length.to_string().parse() {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    Ok(response)
}

async fn serve_non_range_download(
    handle: &DownloadHandle,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    let reader = open_download_file(handle).await.map_err(|error| {
        error!(
            "Failed to open in-flight download {}: {}",
            handle.temp_path.display(),
            error
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let stream = create_tailing_stream(reader, handle.progress.subscribe(), 0, handle.total_len)
        .await
        .map_err(|error| {
            error!("Failed to attach to in-flight download: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let body = Body::from_stream(stream);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    response = apply_streaming_headers(response, &handle.content_type, filename);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    if let Some(length) = handle.total_len {
        if let Ok(value) = length.to_string().parse() {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    Ok(response)
}

async fn serve_range_download(
    handle: &DownloadHandle,
    filename: &str,
    headers: &HeaderMap,
) -> Option<Result<Response<Body>, StatusCode>> {
    let total_len = handle.total_len?;
    let range = headers.get(header::RANGE)?.to_str().ok()?;

    match parse_range_header(range, total_len) {
        RangeSpec::Satisfiable { start, end } => {
            let snapshot = *handle.progress.borrow();
            if snapshot.phase == DownloadPhase::Running
                && start > snapshot.written.saturating_add(SEEK_AHEAD_LIMIT)
            {
                return None;
            }

            let reader = match open_download_file(handle).await {
                Ok(reader) => reader,
                Err(error) => {
                    error!("Failed to open in-flight range source: {error}");
                    return Some(Err(StatusCode::INTERNAL_SERVER_ERROR));
                }
            };
            let stream = match create_tailing_stream(
                reader,
                handle.progress.subscribe(),
                start,
                Some(end + 1),
            )
            .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    error!("Failed to create range tailing stream: {error}");
                    return Some(Err(StatusCode::INTERNAL_SERVER_ERROR));
                }
            };
            let length = end - start + 1;
            let body = Body::from_stream(stream);
            let mut response = match Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_len),
                )
                .header(header::CONTENT_LENGTH, length)
                .body(body)
            {
                Ok(response) => response,
                Err(_) => return Some(Err(StatusCode::INTERNAL_SERVER_ERROR)),
            };
            response = apply_streaming_headers(response, &handle.content_type, filename);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("private, no-store"),
            );
            Some(Ok(response))
        }
        RangeSpec::Unsatisfiable => Some(
            Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", total_len))
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::empty())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        ),
        RangeSpec::Ignore => Some(serve_non_range_download(handle, filename).await),
    }
}

/// Proxy request to upstream server while download is in progress
/// Uses the same prioritization as `try_upstream_servers`
async fn proxy_request_to_upstream(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_relay_pool::prelude::PublicKey>,
) -> Result<Response<Body>, StatusCode> {
    debug!(
        "Proxying request to upstream for ongoing download: {}",
        filename
    );

    let client = state.upstream_client.clone();
    let mut walk = upstream_candidates::Walk::new(
        state,
        filename,
        custom_origin,
        xs_servers,
        author_pubkey,
    );

    while let Some(candidate) = walk.next().await {
        let request = copy_headers_to_reqwest(headers, client.get(&candidate.url));
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                debug!(
                    "Successfully proxied request to {}: {}",
                    candidate.tier.label(),
                    candidate.url
                );
                let content_type = extract_content_type_from_response(response.headers());
                return proxy_upstream_response(response, &content_type, filename).await;
            }
            Ok(response) => debug!(
                "{} {} returned status: {}",
                candidate.tier.label(),
                candidate.url,
                response.status()
            ),
            Err(error) => warn!(
                "Failed to proxy to {} {}: {error}",
                candidate.tier.label(),
                candidate.url
            ),
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Proxy upstream response directly to client
async fn proxy_upstream_response(
    response: reqwest::Response,
    content_type: &str,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    // Extract range info from upstream response for logging
    let content_range = response
        .headers()
        .get(reqwest_header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .map_or_else(|| "none".to_string(), ToString::to_string);

    debug!(
        "Proxying upstream response for: {} (content-type: {}, range: {})",
        filename, content_type, content_range
    );

    let status = if response.status().is_success() {
        if response
            .headers()
            .get(reqwest_header::CONTENT_RANGE)
            .is_some()
        {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        }
    } else {
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK)
    };

    // Get all relevant headers before consuming the response
    let content_range = response
        .headers()
        .get(reqwest_header::CONTENT_RANGE)
        .cloned();
    let content_length = response
        .headers()
        .get(reqwest_header::CONTENT_LENGTH)
        .cloned();
    let accept_ranges = response
        .headers()
        .get(reqwest_header::ACCEPT_RANGES)
        .cloned();
    let cache_control = response
        .headers()
        .get(reqwest_header::CACHE_CONTROL)
        .cloned();
    let etag = response.headers().get(reqwest_header::ETAG).cloned();
    let last_modified = response
        .headers()
        .get(reqwest_header::LAST_MODIFIED)
        .cloned();

    // Extract clean filename from the path (remove any query parameters or codecs)
    let clean_filename = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    // Extract MIME type essence (without parameters like codecs=avc1) to prevent browser from appending to filename
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    // Stream the response directly to client
    let body = Body::from_stream(response.bytes_stream());
    let mut response_builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime_type)
        .header(header::ACCEPT_RANGES, "bytes");

    // Copy all relevant headers from upstream (but NOT Content-Disposition - we set our own)
    if let Some(content_range) = content_range {
        response_builder = response_builder.header(header::CONTENT_RANGE, content_range);
    }
    if let Some(content_length) = content_length {
        response_builder = response_builder.header(header::CONTENT_LENGTH, content_length);
    }
    if let Some(accept_ranges) = accept_ranges {
        response_builder = response_builder.header(header::ACCEPT_RANGES, accept_ranges);
    }
    // IMPORTANT: When proxying upstream responses, do NOT use upstream's cache-control.
    // Use no-store to prevent CDNs from caching potentially incomplete streaming responses.
    // This fixes 416 errors when CDNs cache partial range responses that fail mid-transfer.
    // Once the file is fully downloaded and stored locally, Almond will serve it with
    // proper immutable cache headers from serve_file_with_range().
    let _ = cache_control; // Intentionally ignore upstream's cache-control
    response_builder = response_builder
        .header(header::CACHE_CONTROL, "private, no-store")
        // Vary: Range tells CDNs that responses differ based on Range header,
        // preventing cache collisions between different range requests
        .header(header::VARY, "Range");
    if let Some(etag) = etag {
        response_builder = response_builder.header(header::ETAG, etag);
    }
    if let Some(last_modified) = last_modified {
        response_builder = response_builder.header(header::LAST_MODIFIED, last_modified);
    }

    // Build response first, then insert Content-Disposition to ensure it overwrites any existing header
    let mut response = response_builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set Content-Disposition header with clean filename to prevent browser from appending codecs
    // Use insert() to ensure we overwrite any existing Content-Disposition from upstream
    let content_disposition = format!("inline; filename=\"{}\"", clean_filename);
    if let Ok(header_value) = content_disposition.parse() {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, header_value);
    }

    Ok(response)
}

/// Prepare download state and return metadata for either streaming or background download
async fn prepare_download_state(
    state: &AppState,
    filename: &str,
    content_type: &str,
    total_len: Option<u64>,
) -> Result<PreparedDownload, StatusCode> {
    crate::services::download::prepare_download_state(state, filename, content_type, total_len)
        .await
        .map_err(|error| {
            error!("Failed to prepare upstream download for {filename}: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Stream file from upstream server to client while saving to local storage
async fn stream_and_save_from_upstream(
    state: &AppState,
    file_url: &str,
    upstream_resp: reqwest::Response,
    filename: &str,
    prepared: PreparedDownload,
    negotiation: Option<NegotiationGuard>,
) -> Result<Response<Body>, StatusCode> {
    let content_length = upstream_resp.content_length();
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    if content_length.is_some_and(|length| length > max_size_bytes) {
        let temp_path = prepared.handle.temp_path.clone();
        drop(prepared); // guard drop → map removal
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let reader = match File::open(&prepared.handle.temp_path).await {
        Ok(file) => file,
        Err(error) => {
            error!("Failed to open temp file for streaming: {error}");
            let _ = tokio::fs::remove_file(&prepared.handle.temp_path).await;
            drop(prepared);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let stream = match create_tailing_stream(
        reader,
        prepared.handle.progress.subscribe(),
        0,
        content_length,
    )
    .await
    {
        Ok(stream) => stream,
        Err(error) => {
            error!("Failed to create tailing stream: {error}");
            drop(prepared);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let content_type = prepared.handle.content_type.clone();
    let body = Body::from_stream(stream);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    response = apply_streaming_headers(response, &content_type, filename);
    if let Some(length) = content_length {
        if let Ok(value) = length.to_string().parse() {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }

    if let Some(negotiation) = negotiation {
        negotiation.finish(NegotiationPhase::Ready).await;
    }
    let state = state.clone();
    let file_url = file_url.to_string();
    let filename = filename.to_string();
    tokio::spawn(async move {
        run_download(state, file_url, upstream_resp, filename, prepared, true).await;
    });

    Ok(response)
}

/// Download file from upstream in background (without streaming to client)
async fn download_file_from_upstream_background(
    state: &AppState,
    file_url: &str,
    upstream_resp: reqwest::Response,
    filename: &str,
    prepared: PreparedDownload,
) {
    run_download(
        state.clone(),
        file_url.to_string(),
        upstream_resp,
        filename.to_string(),
        prepared,
        false,
    )
    .await;
}

fn upstream_hash_matches_filename(computed_hash: &str, filename: &str) -> bool {
    get_sha256_hash_from_filename(filename)
        .unwrap_or(filename)
        .eq_ignore_ascii_case(computed_hash)
}

async fn run_download(
    state: AppState,
    file_url: String,
    upstream_resp: reqwest::Response,
    filename: String,

    mut prepared: PreparedDownload,
    count_as_served: bool,
) {
    let handle = prepared.handle.clone();
    let guard = prepared.take_guard();
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    let expected_len = handle.total_len.or_else(|| upstream_resp.content_length());
    let content_type = handle.content_type.clone();
    let extension = get_extension_from_mime(&content_type);
    let temp_path = handle.temp_path.clone();

    let result: Result<(String, u64), String> = async {
        if expected_len.is_some_and(|length| length > max_size_bytes) {
            return Err(format!(
                "upstream file exceeds the {} byte limit",
                max_size_bytes
            ));
        }

        let mut chunks = upstream_resp.bytes_stream();
        let mut hasher = sha2::Sha256::new();
        let mut body_size = 0u64;
        while let Some(next) = chunks.next().await {
            let chunk = next.map_err(|error| error.to_string())?;
            let new_size = body_size + chunk.len() as u64;
            if new_size > max_size_bytes {
                return Err(format!(
                    "upstream body exceeded the {} byte limit",
                    max_size_bytes
                ));
            }

            prepared
                .writer
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
            // `flush` waits for tokio's blocking-file operation. Publishing
            // progress afterwards guarantees followers can read these bytes.
            prepared
                .writer
                .flush()
                .await
                .map_err(|error| error.to_string())?;
            hasher.update(&chunk);
            body_size = new_size;
            handle.progress.send_modify(|progress| {
                progress.written = body_size;
            });
        }

        if expected_len.is_some_and(|length| length != body_size) {
            return Err(format!(
                "upstream body length mismatch: expected {:?}, received {}",
                expected_len, body_size
            ));
        }

        let sha256 = hex::encode(hasher.finalize());
        if !upstream_hash_matches_filename(&sha256, &filename) {
            let requested_hash = get_sha256_hash_from_filename(&filename).unwrap_or(&filename);
            return Err(format!(
                "upstream hash mismatch: requested {requested_hash}, received {sha256}"
            ));
        }
        // The coalescing machinery owned this file while followers tailed it;
        // publication is the point where ownership transfers to storage.
        let published = crate::services::file_storage::publish_blob(
            &state,
            crate::services::file_storage::TempBlob::adopt(temp_path.clone()),
            crate::services::file_storage::BlobPublication {
                sha256: sha256.clone(),
                origin: crate::models::BlobOrigin::UpstreamCache,
                extension,
                mime_type: Some(content_type),
                size: body_size,
                expiration: None,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        if let crate::models::FileLocation::Local(path) = &published.location {
            let _ = handle.final_path.set(path.clone());
        }
        Ok((sha256, body_size))
    }
    .await;

    match result {
        Ok((sha256, total)) => {
            if count_as_served {
                state.metrics.track_download(total);
            } else {
                state.metrics.files_downloaded.inc();
            }
            state.metrics.track_upstream_download(&file_url, total);
            handle.progress.send_modify(|progress| {
                progress.phase = DownloadPhase::Complete;
            });
            info!(
                "Upstream download completed: {} -> {} ({} bytes)",
                file_url, sha256, total
            );
            guard.finish(DownloadPhase::Complete).await;
        }
        Err(error) => {
            error!("Upstream download failed for {}: {}", file_url, error);
            handle.progress.send_modify(|progress| {
                progress.phase = DownloadPhase::Failed;
            });
            let _ = tokio::fs::remove_file(&temp_path).await;
            guard.finish(DownloadPhase::Failed).await;
        }
    }
}

fn apply_streaming_headers(
    mut response: Response<Body>,
    content_type: &str,
    filename: &str,
) -> Response<Body> {
    use axum::http::HeaderValue;

    let headers = response.headers_mut();

    // Extract MIME type essence (without parameters like codecs=avc1) to prevent browser from appending to filename
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    // Parse MIME type - fall back gracefully if parsing fails
    if let Ok(header_value) = mime_type.parse() {
        headers.insert(header::CONTENT_TYPE, header_value);
    }

    // Static header values - these are compile-time constants
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_IMMUTABLE),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    // Add Content-Disposition header to prevent save dialog
    let filename_display = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let content_disposition = format!("inline; filename=\"{}\"", filename_display);
    if let Ok(header_value) = content_disposition.parse() {
        headers.insert(header::CONTENT_DISPOSITION, header_value);
    }

    debug!(
        "Applied streaming headers: Content-Type={}, Content-Disposition=inline; filename=\"{}\"",
        mime_type, filename_display
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DownloadProgress;
    use tokio::{io::AsyncWriteExt, sync::watch};

    async fn temp_download_file() -> (std::path::PathBuf, File, File) {
        let path = std::env::temp_dir().join(format!("almond-tail-{}", uuid::Uuid::new_v4()));
        let writer = File::create(&path).await.unwrap();
        let reader = File::open(&path).await.unwrap();
        (path, writer, reader)
    }

    #[tokio::test]
    async fn non_range_follower_streams_from_existing_download() {
        let (path, mut writer, _) = temp_download_file().await;
        let (progress, _) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let handle = DownloadHandle {
            started: std::time::Instant::now(),
            temp_path: path.clone(),
            content_type: "application/octet-stream".to_string(),
            total_len: Some(11),
            progress: progress.clone(),
            final_path: std::sync::OnceLock::new(),
        };

        let response = serve_non_range_download(&handle, "blob.bin").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );

        let download = tokio::spawn(async move {
            writer.write_all(b"hello").await.unwrap();
            writer.flush().await.unwrap();
            progress.send_modify(|state| state.written = 5);
            writer.write_all(b" world").await.unwrap();
            writer.flush().await.unwrap();
            progress.send_modify(|state| {
                state.written = 11;
                state.phase = DownloadPhase::Complete;
            });
        });
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        download.await.unwrap();
        assert_eq!(body, bytes::Bytes::from_static(b"hello world"));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[test]
    fn cold_range_fetch_starts_at_zero() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=1024-2047".parse().unwrap());
        let request =
            copy_headers_for_cold_fetch(&headers, Client::new().get("http://127.0.0.1/blob"))
                .build()
                .unwrap();
        assert_eq!(request.headers()[reqwest_header::RANGE], "bytes=0-");

        headers.insert(
            header::RANGE,
            format!("bytes={}-", SEEK_AHEAD_LIMIT + 1).parse().unwrap(),
        );
        let request =
            copy_headers_for_cold_fetch(&headers, Client::new().get("http://127.0.0.1/blob"))
                .build()
                .unwrap();
        assert_eq!(
            request.headers()[reqwest_header::RANGE],
            format!("bytes={}-", SEEK_AHEAD_LIMIT + 1)
        );
    }

    #[tokio::test]
    async fn range_follower_streams_requested_bytes_from_full_fetch() {
        let (path, mut writer, _) = temp_download_file().await;
        let (progress, _) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let handle = DownloadHandle {
            started: std::time::Instant::now(),
            temp_path: path.clone(),
            content_type: "application/octet-stream".to_string(),
            total_len: Some(11),
            progress: progress.clone(),
            final_path: std::sync::OnceLock::new(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=6-10".parse().unwrap());

        let response = serve_range_download(&handle, "blob.bin", &headers)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 6-10/11");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");

        let download = tokio::spawn(async move {
            writer.write_all(b"hello world").await.unwrap();
            writer.flush().await.unwrap();
            progress.send_modify(|state| {
                state.written = 11;
                state.phase = DownloadPhase::Complete;
            });
        });
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        download.await.unwrap();
        assert_eq!(body, bytes::Bytes::from_static(b"world"));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn far_range_follower_declines_to_avoid_stalling() {
        let path = std::env::temp_dir().join(format!("almond-tail-{}", uuid::Uuid::new_v4()));
        let (progress, _) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let handle = DownloadHandle {
            started: std::time::Instant::now(),
            temp_path: path,
            content_type: "application/octet-stream".to_string(),
            total_len: Some(32 * 1024 * 1024),
            progress,
            final_path: std::sync::OnceLock::new(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            format!("bytes={}-", SEEK_AHEAD_LIMIT + 1).parse().unwrap(),
        );

        assert!(serve_range_download(&handle, "blob.bin", &headers)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn negotiation_wait_times_out_for_stalled_leader() {
        let (phase, _) = watch::channel(NegotiationPhase::Pending);
        let negotiation = UpstreamNegotiation {
            started: std::time::Instant::now(),
            phase,
        };

        assert_eq!(
            wait_for_negotiation(&negotiation, std::time::Duration::from_millis(1)).await,
            NegotiationPhase::Pending
        );
    }

    #[tokio::test]
    async fn negotiation_wait_observes_leader_completion() {
        let (phase, _) = watch::channel(NegotiationPhase::Pending);
        let negotiation = UpstreamNegotiation {
            started: std::time::Instant::now(),
            phase: phase.clone(),
        };
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            phase.send_replace(NegotiationPhase::Ready);
        });

        assert_eq!(
            wait_for_negotiation(&negotiation, std::time::Duration::from_secs(1)).await,
            NegotiationPhase::Ready
        );
    }

    #[test]
    fn upstream_hash_comparison_accepts_case_insensitive_filenames() {
        let hash = "a3f5c01f0f87b6e1d2c3a4b5c6d7e8f90123456789abcdef0123456789abcdef";
        assert!(upstream_hash_matches_filename(hash, &hash.to_uppercase()));
        assert!(upstream_hash_matches_filename(hash, &format!("{hash}.mp4")));
        assert!(!upstream_hash_matches_filename(
            hash,
            "b3f5c01f0f87b6e1d2c3a4b5c6d7e8f90123456789abcdef0123456789abcdef"
        ));
    }

    #[tokio::test]
    async fn head_follower_uses_download_metadata_without_reading_body() {
        let (progress, _) = watch::channel(DownloadProgress {
            written: 3,
            phase: DownloadPhase::Running,
        });
        let handle = DownloadHandle {
            started: std::time::Instant::now(),
            temp_path: std::path::PathBuf::from("/path/does/not/need/to/exist"),
            content_type: "video/mp4".to_string(),
            total_len: Some(42),
            progress,
            final_path: std::sync::OnceLock::new(),
        };

        let response = serve_head_download(&handle, "blob.mp4").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "42");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        assert!(axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());
    }
}
