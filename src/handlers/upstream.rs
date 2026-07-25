use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use futures_util::StreamExt;
use reqwest::{header as reqwest_header, Client};
use serde_json::json;
use sha2::Digest;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::watch,
};
use tracing::{debug, error, info, warn};

use crate::services::upload::validate_upstream_url;

use crate::constants::*;
use crate::helpers::*;
use crate::models::{AppState, DownloadHandle, DownloadPhase, DownloadProgress};
use crate::services::download::{DownloadGuard, PreparedDownload};

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
/// Prioritization: custom_origin → xs_servers → UPSTREAM_SERVERS → user servers (lazy fetch)
pub async fn try_upstream_servers(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
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
    let file_hash = crate::utils::get_sha256_hash_from_filename(filename).unwrap_or(filename);

    if let Some(handle) = state.ongoing_downloads.read().await.get(file_hash).cloned() {
        if headers.get(header::RANGE).is_none() {
            debug!("Attaching request to in-flight download {}", file_hash);
            return serve_non_range_download(&handle, filename).await;
        }

        debug!(
            "Range request for in-flight download {}, proxying until phase 2",
            file_hash
        );
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

    // Track which servers we've already tried to avoid duplicate HEAD requests
    let mut tried_servers = std::collections::HashSet::<String>::new();

    let client = state.upstream_client.clone();

    // Try custom origin first if provided (single server)
    if let Some(origin_url) = custom_origin {
        // Validate URL against SSRF before making request
        let normalized_origin = match validate_upstream_url(origin_url).await {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    "Custom origin URL validation failed (SSRF protection): {} - {}",
                    origin_url, e
                );
                // Skip this server and continue to xs_servers or configured upstream servers
                String::new()
            }
        };

        if normalized_origin.is_empty() {
            debug!(
                "Custom origin failed validation, trying xs servers or configured upstream servers"
            );
        } else {
            debug!("Trying custom origin server first: {}", normalized_origin);
            let file_url = format!("{}/{}", normalized_origin.trim_end_matches('/'), filename);
            debug!("Trying upstream server: {}", file_url);
            tried_servers.insert(normalized_origin.clone());

            // Create request with all relevant headers for upstream servers
            let request = client.get(&file_url);
            let request = copy_headers_to_reqwest(headers, request);

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    debug!("Found file on custom origin server: {}", file_url);
                    // Get content type from upstream response
                    let content_type = extract_content_type_from_response(response.headers());
                    // Check if this is a range request
                    let has_range_header = headers.get(header::RANGE).is_some();

                    if has_range_header {
                        debug!("Range request detected for non-existent file {}, starting download from byte 0", file_hash);
                        // For range requests, we need to start a full download in the background
                        // while proxying the range request for immediate response
                        let full_request = client.get(&file_url);
                        let full_request = copy_headers_without_range(headers, full_request);

                        match full_request.send().await {
                            Ok(full_response) if full_response.status().is_success() => {
                                debug!(
                                    "Starting full download from byte 0 for range request: {}",
                                    file_hash
                                );
                                let prepared = prepare_download_state(
                                    state,
                                    file_hash,
                                    &content_type,
                                    full_response.content_length(),
                                )
                                .await?;
                                let state_clone = state.clone();
                                let file_url_clone = file_url.clone();
                                let file_hash_clone = file_hash.to_string();
                                tokio::spawn(async move {
                                    download_file_from_upstream_background(
                                        &state_clone,
                                        &file_url_clone,
                                        full_response,
                                        &file_hash_clone,
                                        prepared,
                                    )
                                    .await;
                                });
                                // Proxy the range request to upstream for immediate response
                                debug!("Proxying range request to upstream while download starts in background: {}", file_hash);
                                return proxy_upstream_response(response, &content_type, filename)
                                    .await;
                            }
                            Ok(_) | Err(_) => {
                                warn!("Failed to start full download for range request, proxying range request only: {}", file_hash);
                                return proxy_upstream_response(response, &content_type, filename)
                                    .await;
                            }
                        }
                    } else {
                        // For non-range requests, stream and save from upstream
                        debug!(
                            "Non-range request, starting download and streaming to client: {}",
                            file_hash
                        );
                        let prepared = prepare_download_state(
                            state,
                            file_hash,
                            &content_type,
                            response.content_length(),
                        )
                        .await?;
                        return stream_and_save_from_upstream(
                            state, &file_url, response, file_hash, prepared,
                        )
                        .await;
                    }
                }
                Ok(response) => {
                    debug!(
                        "Custom origin server {} returned status: {}",
                        file_url,
                        response.status()
                    );
                }
                Err(e) => {
                    warn!("Failed to fetch from custom origin {}: {}", file_url, e);
                }
            }
            // If custom origin failed, continue to xs_servers or configured upstream servers
            debug!("Custom origin failed, trying xs servers or configured upstream servers");
        }
    }

    // Priority 1: Try xs servers if provided
    if let Some(servers) = xs_servers {
        debug!("Priority 1: Trying xs servers ({} servers)", servers.len());
        for server in servers {
            for candidate in server_url_candidates(server) {
                let normalized_server = match validate_upstream_url(&candidate).await {
                    Ok(url) => url,
                    Err(e) => {
                        warn!(
                            "xs server URL validation failed (SSRF protection): {} - {}",
                            candidate, e
                        );
                        continue;
                    }
                };

                if !tried_servers.insert(normalized_server.clone()) {
                    debug!("Skipping already-tried server: {}", normalized_server);
                    continue;
                }

                let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                debug!("Trying xs server: {}", file_url);

                let request = client.get(&file_url);
                let request = copy_headers_to_reqwest(headers, request);

                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        debug!("Found file on xs server: {}", file_url);
                        return handle_successful_upstream_response(
                            state, &client, response, &file_url, file_hash, filename, headers,
                        )
                        .await;
                    }
                    Ok(response) => {
                        debug!(
                            "xs server {} returned status: {}",
                            file_url,
                            response.status()
                        );
                    }
                    Err(e) => {
                        debug!("Failed to fetch from xs server {}: {}", file_url, e);
                    }
                }
            }
        }
        debug!("All xs servers failed or returned non-success, trying local UPSTREAM_SERVERS");
    }

    // Priority 2: Try local UPSTREAM_SERVERS
    if !state.upstream_servers.is_empty() {
        debug!(
            "Priority 2: Trying local UPSTREAM_SERVERS ({} servers)",
            state.upstream_servers.len()
        );
        for server in &state.upstream_servers {
            // Validate URL against SSRF before making request
            let normalized_server = match validate_upstream_url(server).await {
                Ok(url) => url,
                Err(e) => {
                    warn!(
                        "UPSTREAM_SERVER URL validation failed (SSRF protection): {} - {}",
                        server, e
                    );
                    continue;
                }
            };

            // Skip if we've already tried this server
            if tried_servers.contains(&normalized_server) {
                debug!("Skipping already-tried server: {}", normalized_server);
                continue;
            }

            tried_servers.insert(normalized_server.clone());
            let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
            debug!("Trying local UPSTREAM_SERVER: {}", file_url);

            // Create request with all relevant headers for upstream servers
            let request = client.get(&file_url);
            let request = copy_headers_to_reqwest(headers, request);

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    debug!("Found file on local UPSTREAM_SERVER: {}", file_url);
                    return handle_successful_upstream_response(
                        state, &client, response, &file_url, file_hash, filename, headers,
                    )
                    .await;
                }
                Ok(response) => {
                    debug!(
                        "UPSTREAM_SERVER {} returned status: {}",
                        file_url,
                        response.status()
                    );
                }
                Err(e) => {
                    debug!("Failed to fetch from UPSTREAM_SERVER {}: {}", file_url, e);
                }
            }
        }
        debug!("All local UPSTREAM_SERVERS failed or returned non-success");
    }

    // Priority 3: Fetch and try user servers (lazy fetch) if author pubkey is provided
    if let Some(pubkey) = author_pubkey {
        debug!(
            "Priority 3: Fetching user server list for pubkey: {} (lazy fetch)",
            pubkey.to_hex()
        );
        match crate::services::blossom_servers::fetch_user_server_list(state, pubkey).await {
            Ok(user_servers) if !user_servers.is_empty() => {
                debug!(
                    "Fetched {} servers from user's server list (BUD-03)",
                    user_servers.len()
                );
                for server in &user_servers {
                    // Validate URL against SSRF before making request
                    let normalized_server = match validate_upstream_url(server).await {
                        Ok(url) => url,
                        Err(e) => {
                            warn!(
                                "User server URL validation failed (SSRF protection): {} - {}",
                                server, e
                            );
                            continue;
                        }
                    };

                    // Skip if we've already tried this server
                    if tried_servers.contains(&normalized_server) {
                        debug!("Skipping already-tried server: {}", normalized_server);
                        continue;
                    }

                    tried_servers.insert(normalized_server.clone());
                    let file_url =
                        format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                    debug!("Trying user server: {}", file_url);

                    // Create request with all relevant headers for upstream servers
                    let request = client.get(&file_url);
                    let request = copy_headers_to_reqwest(headers, request);

                    match request.send().await {
                        Ok(response) if response.status().is_success() => {
                            debug!("Found file on user server: {}", file_url);
                            return handle_successful_upstream_response(
                                state, &client, response, &file_url, file_hash, filename, headers,
                            )
                            .await;
                        }
                        Ok(response) => {
                            debug!(
                                "User server {} returned status: {}",
                                file_url,
                                response.status()
                            );
                        }
                        Err(e) => {
                            debug!("Failed to fetch from user server {}: {}", file_url, e);
                        }
                    }
                }
                debug!("All user servers failed or returned non-success");
            }
            Ok(_) => {
                debug!("User server list is empty for pubkey: {}", pubkey.to_hex());
            }
            Err(e) => {
                warn!(
                    "Failed to fetch user server list for pubkey {}: {}",
                    pubkey.to_hex(),
                    e
                );
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Try to find file on upstream servers and return a 302 redirect response
/// Prioritization: custom_origin → xs_servers → UPSTREAM_SERVERS → user servers (lazy fetch)
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
    let file_hash = crate::utils::get_sha256_hash_from_filename(filename).unwrap_or(filename);

    // Check if this file is already being downloaded
    if state.ongoing_downloads.read().await.contains_key(file_hash) {
        debug!(
            "File {} is already being downloaded in background, redirecting to upstream",
            file_hash
        );
    }

    // Track which servers we've already tried to avoid duplicate HEAD requests
    let mut tried_servers = std::collections::HashSet::<String>::new();
    let client = state.upstream_client.clone();

    // Try custom origin first if provided (single server)
    if let Some(origin_url) = custom_origin {
        let normalized_origin = match validate_upstream_url(origin_url).await {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    "Custom origin URL validation failed (SSRF protection): {} - {}",
                    origin_url, e
                );
                String::new()
            }
        };

        if !normalized_origin.is_empty() {
            debug!(
                "Trying custom origin server first (HEAD): {}",
                normalized_origin
            );
            tried_servers.insert(normalized_origin.clone());
            let file_url = format!("{}/{}", normalized_origin.trim_end_matches('/'), filename);

            if let Some(response) = try_head_and_redirect(
                state,
                &client,
                &file_url,
                file_hash,
                filename,
                cache_in_background,
            )
            .await
            {
                return Ok(response);
            }
        }
    }

    // Priority 1: Try xs servers if provided
    if let Some(servers) = xs_servers {
        debug!(
            "Priority 1: Trying xs servers (HEAD) ({} servers)",
            servers.len()
        );
        for server in servers {
            for candidate in server_url_candidates(server) {
                let normalized_server = match validate_upstream_url(&candidate).await {
                    Ok(url) => url,
                    Err(e) => {
                        warn!(
                            "xs server URL validation failed (SSRF protection): {} - {}",
                            candidate, e
                        );
                        continue;
                    }
                };

                if !tried_servers.insert(normalized_server.clone()) {
                    debug!("Skipping already-tried server: {}", normalized_server);
                    continue;
                }

                let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                debug!("Trying xs server (HEAD): {}", file_url);

                if let Some(response) = try_head_and_redirect(
                    state,
                    &client,
                    &file_url,
                    file_hash,
                    filename,
                    cache_in_background,
                )
                .await
                {
                    return Ok(response);
                }
            }
        }
        debug!("All xs servers failed HEAD check, trying local UPSTREAM_SERVERS");
    }

    // Priority 2: Try local UPSTREAM_SERVERS
    if !state.upstream_servers.is_empty() {
        debug!(
            "Priority 2: Trying local UPSTREAM_SERVERS (HEAD) ({} servers)",
            state.upstream_servers.len()
        );
        for server in &state.upstream_servers {
            let normalized_server = match validate_upstream_url(server).await {
                Ok(url) => url,
                Err(e) => {
                    warn!(
                        "UPSTREAM_SERVER URL validation failed (SSRF protection): {} - {}",
                        server, e
                    );
                    continue;
                }
            };

            if tried_servers.contains(&normalized_server) {
                debug!("Skipping already-tried server: {}", normalized_server);
                continue;
            }

            tried_servers.insert(normalized_server.clone());
            let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
            debug!("Trying local UPSTREAM_SERVER (HEAD): {}", file_url);

            if let Some(response) = try_head_and_redirect(
                state,
                &client,
                &file_url,
                file_hash,
                filename,
                cache_in_background,
            )
            .await
            {
                return Ok(response);
            }
        }
        debug!("All local UPSTREAM_SERVERS failed HEAD check");
    }

    // Priority 3: Fetch and try user servers (lazy fetch) if author pubkey is provided
    if let Some(pubkey) = author_pubkey {
        debug!(
            "Priority 3: Fetching user server list for pubkey: {} (lazy fetch)",
            pubkey.to_hex()
        );
        match crate::services::blossom_servers::fetch_user_server_list(state, pubkey).await {
            Ok(user_servers) if !user_servers.is_empty() => {
                debug!(
                    "Fetched {} servers from user's server list (BUD-03)",
                    user_servers.len()
                );
                for server in &user_servers {
                    let normalized_server = match validate_upstream_url(server).await {
                        Ok(url) => url,
                        Err(e) => {
                            warn!(
                                "User server URL validation failed (SSRF protection): {} - {}",
                                server, e
                            );
                            continue;
                        }
                    };

                    if tried_servers.contains(&normalized_server) {
                        debug!("Skipping already-tried server: {}", normalized_server);
                        continue;
                    }

                    tried_servers.insert(normalized_server.clone());
                    let file_url =
                        format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                    debug!("Trying user server (HEAD): {}", file_url);

                    if let Some(response) = try_head_and_redirect(
                        state,
                        &client,
                        &file_url,
                        file_hash,
                        filename,
                        cache_in_background,
                    )
                    .await
                    {
                        return Ok(response);
                    }
                }
                debug!("All user servers failed HEAD check");
            }
            Ok(_) => {
                debug!("User server list is empty for pubkey: {}", pubkey.to_hex());
            }
            Err(e) => {
                warn!(
                    "Failed to fetch user server list for pubkey {}: {}",
                    pubkey.to_hex(),
                    e
                );
            }
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

                if !already_downloading {
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
                } else {
                    debug!(
                        "File {} already being downloaded, skipping duplicate background download",
                        file_hash
                    );
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
) -> Result<Response, StatusCode> {
    // Get content type from upstream response
    let content_type = extract_content_type_from_response(response.headers());

    // Check if this is a range request
    let has_range_header = headers.get(header::RANGE).is_some();

    if has_range_header {
        debug!(
            "Range request detected for non-existent file {}, starting download from byte 0",
            file_hash
        );

        // For range requests, we need to start a full download in the background
        // while proxying the range request for immediate response
        let full_request = client.get(file_url);
        let full_request = copy_headers_without_range(headers, full_request);

        match full_request.send().await {
            Ok(full_response) if full_response.status().is_success() => {
                debug!(
                    "Starting full download from byte 0 for range request: {}",
                    file_hash
                );

                let prepared = prepare_download_state(
                    state,
                    file_hash,
                    &content_type,
                    full_response.content_length(),
                )
                .await?;

                let state_clone = state.clone();
                let file_url_clone = file_url.to_string();
                let file_hash_clone = file_hash.to_string();
                tokio::spawn(async move {
                    download_file_from_upstream_background(
                        &state_clone,
                        &file_url_clone,
                        full_response,
                        &file_hash_clone,
                        prepared,
                    )
                    .await;
                });

                // Proxy the range request to upstream for immediate response
                debug!(
                    "Proxying range request to upstream while download starts in background: {}",
                    file_hash
                );
                return proxy_upstream_response(response, &content_type, filename).await;
            }
            Ok(_) | Err(_) => {
                // If we can't get the full file, fall back to proxying the range request
                warn!("Failed to start full download for range request, proxying range request only: {}", file_hash);
                return proxy_upstream_response(response, &content_type, filename).await;
            }
        }
    } else {
        // For non-range requests, stream and save from upstream
        debug!(
            "Non-range request, starting download and streaming to client: {}",
            file_hash
        );

        let prepared =
            prepare_download_state(state, file_hash, &content_type, response.content_length())
                .await?;

        return stream_and_save_from_upstream(state, file_url, response, file_hash, prepared).await;
    }
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

/// Proxy request to upstream server while download is in progress
/// Uses the same prioritization as try_upstream_servers
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

    // Track which servers we've already tried to avoid duplicate requests
    let mut tried_servers = std::collections::HashSet::<String>::new();

    // Try custom origin first if provided
    if let Some(origin_url) = custom_origin {
        // Validate URL against SSRF before making request
        let normalized_origin = match validate_upstream_url(origin_url).await {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    "Custom origin URL validation failed (SSRF protection): {} - {}",
                    origin_url, e
                );
                String::new()
            }
        };

        if !normalized_origin.is_empty() {
            let file_url = format!("{}/{}", normalized_origin.trim_end_matches('/'), filename);
            debug!("Proxying to custom origin server: {}", file_url);
            tried_servers.insert(normalized_origin.clone());

            // Create request with all relevant headers
            let request = client.get(&file_url);
            let request = copy_headers_to_reqwest(headers, request);

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    debug!(
                        "Successfully proxied request to custom origin: {}",
                        file_url
                    );
                    let content_type = extract_content_type_from_response(response.headers());
                    return proxy_upstream_response(response, &content_type, filename).await;
                }
                Ok(response) => {
                    debug!(
                        "Custom origin server {} returned status: {}",
                        file_url,
                        response.status()
                    );
                }
                Err(e) => {
                    warn!("Failed to proxy to custom origin {}: {}", file_url, e);
                }
            }
        }
    }

    // Priority 1: Try xs servers if provided
    if let Some(servers) = xs_servers {
        for server in servers {
            for candidate in server_url_candidates(server) {
                let normalized_server = match validate_upstream_url(&candidate).await {
                    Ok(url) => url,
                    Err(e) => {
                        warn!(
                            "xs server URL validation failed (SSRF protection): {} - {}",
                            candidate, e
                        );
                        continue;
                    }
                };

                if !tried_servers.insert(normalized_server.clone()) {
                    continue;
                }

                let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                debug!("Proxying to xs server: {}", file_url);

                let request = client.get(&file_url);
                let request = copy_headers_to_reqwest(headers, request);

                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        debug!("Successfully proxied request to xs server: {}", file_url);
                        let content_type = extract_content_type_from_response(response.headers());
                        return proxy_upstream_response(response, &content_type, filename).await;
                    }
                    Ok(response) => {
                        debug!(
                            "xs server {} returned status: {}",
                            file_url,
                            response.status()
                        );
                    }
                    Err(e) => {
                        debug!("Failed to proxy to xs server {}: {}", file_url, e);
                    }
                }
            }
        }
    }

    // Priority 2: Try local UPSTREAM_SERVERS
    for server in &state.upstream_servers {
        // Validate URL against SSRF before making request
        let normalized_server = match validate_upstream_url(server).await {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    "UPSTREAM_SERVER URL validation failed (SSRF protection): {} - {}",
                    server, e
                );
                continue;
            }
        };

        if tried_servers.contains(&normalized_server) {
            continue;
        }
        tried_servers.insert(normalized_server.clone());

        let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
        debug!("Proxying to local UPSTREAM_SERVER: {}", file_url);

        let request = client.get(&file_url);
        let request = copy_headers_to_reqwest(headers, request);

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                debug!(
                    "Successfully proxied request to UPSTREAM_SERVER: {}",
                    file_url
                );
                let content_type = extract_content_type_from_response(response.headers());
                return proxy_upstream_response(response, &content_type, filename).await;
            }
            Ok(response) => {
                debug!(
                    "UPSTREAM_SERVER {} returned status: {}",
                    file_url,
                    response.status()
                );
            }
            Err(e) => {
                debug!("Failed to proxy to UPSTREAM_SERVER {}: {}", file_url, e);
            }
        }
    }

    // Priority 3: Fetch and try user servers (lazy fetch)
    if let Some(pubkey) = author_pubkey {
        debug!(
            "Fetching user server list for proxying: {}",
            pubkey.to_hex()
        );
        if let Ok(user_servers) =
            crate::services::blossom_servers::fetch_user_server_list(state, pubkey).await
        {
            for server in &user_servers {
                // Validate URL against SSRF before making request
                let normalized_server = match validate_upstream_url(server).await {
                    Ok(url) => url,
                    Err(e) => {
                        warn!(
                            "User server URL validation failed (SSRF protection): {} - {}",
                            server, e
                        );
                        continue;
                    }
                };

                if tried_servers.contains(&normalized_server) {
                    continue;
                }
                tried_servers.insert(normalized_server.clone());

                let file_url = format!("{}/{}", normalized_server.trim_end_matches('/'), filename);
                debug!("Proxying to user server: {}", file_url);

                let request = client.get(&file_url);
                let request = copy_headers_to_reqwest(headers, request);

                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        debug!("Successfully proxied request to user server: {}", file_url);
                        let content_type = extract_content_type_from_response(response.headers());
                        return proxy_upstream_response(response, &content_type, filename).await;
                    }
                    Ok(response) => {
                        debug!(
                            "User server {} returned status: {}",
                            file_url,
                            response.status()
                        );
                    }
                    Err(e) => {
                        debug!("Failed to proxy to user server {}: {}", file_url, e);
                    }
                }
            }
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
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());

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
) -> Result<Response<Body>, StatusCode> {
    let content_length = upstream_resp.content_length();
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    if content_length.is_some_and(|length| length > max_size_bytes) {
        let guard = DownloadGuard::new(state, filename, prepared.handle.clone());
        guard.finish(DownloadPhase::Failed).await;
        let _ = tokio::fs::remove_file(&prepared.handle.temp_path).await;
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let reader = File::open(&prepared.handle.temp_path)
        .await
        .map_err(|error| {
            error!("Failed to open temp file for streaming: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let stream = create_tailing_stream(
        reader,
        prepared.handle.progress.subscribe(),
        0,
        content_length,
    )
    .await
    .map_err(|error| {
        error!("Failed to create tailing stream: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

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

async fn run_download(
    state: AppState,
    file_url: String,
    upstream_resp: reqwest::Response,
    filename: String,
    mut prepared: PreparedDownload,
    count_as_served: bool,
) {
    let handle = prepared.handle.clone();
    let guard = DownloadGuard::new(&state, &filename, handle.clone());
    let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;
    let expected_len = upstream_resp.content_length();
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
        let final_path =
            crate::utils::get_nested_path(&state.upload_dir, &sha256, extension.as_deref(), None);
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        tokio::fs::rename(&temp_path, &final_path)
            .await
            .map_err(|error| error.to_string())?;
        let _ = handle.final_path.set(final_path.clone());

        let key = sha256[..sha256.len().min(64)].to_string();
        state
            .file_index
            .insert(
                key,
                crate::models::FileMetadata {
                    path: final_path,
                    extension,
                    mime_type: Some(content_type),
                    size: body_size,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    pubkey: None,
                    expiration: None,
                },
            )
            .await;
        *state.changes_pending.write().await = true;
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

/// Create a streaming response that reads from a growing file
async fn create_tailing_stream(
    mut reader: File,
    mut progress: watch::Receiver<DownloadProgress>,
    start: u64,
    end: Option<u64>,
) -> std::io::Result<impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>>> {
    reader.seek(SeekFrom::Start(start)).await?;

    Ok(async_stream::try_stream! {
        let mut position = start;
        loop {
            let snapshot = *progress.borrow_and_update();
            let available = end.unwrap_or(u64::MAX).min(snapshot.written);

            if position < available {
                let to_read = std::cmp::min(64 * 1024, (available - position) as usize);
                let mut buffer = vec![0u8; to_read];
                reader.read_exact(&mut buffer).await?;
                position += to_read as u64;
                yield bytes::Bytes::from(buffer);
                continue;
            }

            if end.is_some_and(|limit| position >= limit) {
                break;
            }

            match snapshot.phase {
                DownloadPhase::Running => {
                    progress.changed().await.map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "download progress channel closed",
                        )
                    })?;
                }
                DownloadPhase::Complete => {
                    if end.is_some_and(|limit| position < limit) {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "download completed before the requested range",
                        ))?;
                    }
                    break;
                }
                DownloadPhase::Failed => {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "upstream download failed",
                    ))?;
                }
            }
        }
    })
}

/// Apply streaming headers to an existing response
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
    use futures_util::{pin_mut, StreamExt};
    use tokio::io::AsyncWriteExt;

    async fn temp_download_file() -> (std::path::PathBuf, File, File) {
        let path = std::env::temp_dir().join(format!("almond-tail-{}", uuid::Uuid::new_v4()));
        let writer = File::create(&path).await.unwrap();
        let reader = File::open(&path).await.unwrap();
        (path, writer, reader)
    }

    #[tokio::test]
    async fn tailing_stream_joins_mid_download_and_terminates() {
        let (path, mut writer, reader) = temp_download_file().await;
        let (progress, receiver) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let stream = create_tailing_stream(reader, receiver, 0, None)
            .await
            .unwrap();
        pin_mut!(stream);

        writer.write_all(b"hello").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| state.written = 5);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"hello")
        );

        writer.write_all(b" world").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| {
            state.written = 11;
            state.phase = DownloadPhase::Complete;
        });
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b" world")
        );
        assert!(stream.next().await.is_none());
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_reports_download_failure() {
        let (path, mut writer, reader) = temp_download_file().await;
        let (progress, receiver) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let stream = create_tailing_stream(reader, receiver, 0, None)
            .await
            .unwrap();
        pin_mut!(stream);

        writer.write_all(b"abc").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| state.written = 3);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"abc")
        );
        progress.send_modify(|state| state.phase = DownloadPhase::Failed);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_respects_requested_bounds() {
        let (path, mut writer, reader) = temp_download_file().await;
        writer.write_all(b"abcdefgh").await.unwrap();
        writer.flush().await.unwrap();
        let (_, receiver) = watch::channel(DownloadProgress {
            written: 8,
            phase: DownloadPhase::Complete,
        });
        let stream = create_tailing_stream(reader, receiver, 2, Some(6))
            .await
            .unwrap();
        pin_mut!(stream);

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"cdef")
        );
        assert!(stream.next().await.is_none());
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_rejects_truncated_completed_range() {
        let (path, mut writer, reader) = temp_download_file().await;
        writer.write_all(b"abc").await.unwrap();
        writer.flush().await.unwrap();
        let (_, receiver) = watch::channel(DownloadProgress {
            written: 3,
            phase: DownloadPhase::Complete,
        });
        let stream = create_tailing_stream(reader, receiver, 0, Some(5))
            .await
            .unwrap();
        pin_mut!(stream);

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"abc")
        );
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        tokio::fs::remove_file(path).await.unwrap();
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
}
