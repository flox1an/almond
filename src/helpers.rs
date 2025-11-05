use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use reqwest::header as reqwest_header;
use tracing::info;

use crate::constants::*;
use crate::models::AppState;

/// Extract content type from headers with fallback
pub fn extract_content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string()
}

/// Extract content type from reqwest response headers
pub fn extract_content_type_from_response(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string()
}

/// Extract expiration timestamp from X-Expiration header
pub fn extract_expiration(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(X_EXPIRATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Track download statistics
pub async fn track_download_stats(state: &AppState, size: u64) {
    let mut files_downloaded = state.files_downloaded.write().await;
    *files_downloaded += 1;

    let mut download_throughput_data = state.download_throughput_data.write().await;
    download_throughput_data.push((std::time::Instant::now(), size));

    // Keep only last entries to prevent memory bloat
    if download_throughput_data.len() > MAX_THROUGHPUT_ENTRIES {
        download_throughput_data.drain(0..THROUGHPUT_CLEANUP_THRESHOLD);
    }
}

/// Track upload statistics
pub async fn track_upload_stats(state: &AppState, size: u64) {
    let mut files_uploaded = state.files_uploaded.write().await;
    *files_uploaded += 1;

    let mut upload_throughput_data = state.upload_throughput_data.write().await;
    upload_throughput_data.push((std::time::Instant::now(), size));

    // Keep only last entries to prevent memory bloat
    if upload_throughput_data.len() > MAX_THROUGHPUT_ENTRIES {
        upload_throughput_data.drain(0..THROUGHPUT_CLEANUP_THRESHOLD);
    }
}

/// Create a simple error response
pub fn create_error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(message))
        .unwrap()
}

/// Create a JSON response
pub fn create_json_response<T: serde::Serialize>(data: T) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&data).unwrap()))
        .unwrap()
}

/// Copy relevant headers from one HeaderMap to a reqwest request builder
pub fn copy_headers_to_reqwest(
    headers: &HeaderMap,
    mut request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        request = request.header(header::USER_AGENT, user_agent);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        request = request.header(header::ACCEPT, accept);
    }
    if let Some(range) = headers.get(header::RANGE) {
        request = request.header(header::RANGE, range);
        info!("Forwarding range request: {}", range.to_str().unwrap_or("invalid"));
    }
    if let Some(if_range) = headers.get(header::IF_RANGE) {
        request = request.header(header::IF_RANGE, if_range);
    }
    if let Some(if_match) = headers.get(header::IF_MATCH) {
        request = request.header(header::IF_MATCH, if_match);
    }
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        request = request.header(header::IF_NONE_MATCH, if_none_match);
    }
    if let Some(if_modified_since) = headers.get(header::IF_MODIFIED_SINCE) {
        request = request.header(header::IF_MODIFIED_SINCE, if_modified_since);
    }
    if let Some(if_unmodified_since) = headers.get(header::IF_UNMODIFIED_SINCE) {
        request = request.header(header::IF_UNMODIFIED_SINCE, if_unmodified_since);
    }
    request
}

/// Copy relevant headers from one HeaderMap to a reqwest request builder, excluding the Range header
pub fn copy_headers_without_range(
    headers: &HeaderMap,
    mut request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        request = request.header(header::USER_AGENT, user_agent);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        request = request.header(header::ACCEPT, accept);
    }
    // Explicitly skip RANGE header
    if let Some(if_range) = headers.get(header::IF_RANGE) {
        request = request.header(header::IF_RANGE, if_range);
    }
    if let Some(if_match) = headers.get(header::IF_MATCH) {
        request = request.header(header::IF_MATCH, if_match);
    }
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        request = request.header(header::IF_NONE_MATCH, if_none_match);
    }
    if let Some(if_modified_since) = headers.get(header::IF_MODIFIED_SINCE) {
        request = request.header(header::IF_MODIFIED_SINCE, if_modified_since);
    }
    if let Some(if_unmodified_since) = headers.get(header::IF_UNMODIFIED_SINCE) {
        request = request.header(header::IF_UNMODIFIED_SINCE, if_unmodified_since);
    }
    request
}

/// Copy relevant headers from reqwest response to axum response builder
pub fn copy_headers_from_reqwest(
    response: &reqwest::Response,
    mut response_builder: axum::http::response::Builder,
) -> axum::http::response::Builder {
    // Copy range-related headers from upstream
    if let Some(content_range) = response.headers().get(reqwest_header::CONTENT_RANGE) {
        response_builder = response_builder.header(header::CONTENT_RANGE, content_range);
    }
    if let Some(content_length) = response.headers().get(reqwest_header::CONTENT_LENGTH) {
        response_builder = response_builder.header(header::CONTENT_LENGTH, content_length);
    }
    if let Some(accept_ranges) = response.headers().get(reqwest_header::ACCEPT_RANGES) {
        response_builder = response_builder.header(header::ACCEPT_RANGES, accept_ranges);
    }
    if let Some(cache_control) = response.headers().get(reqwest_header::CACHE_CONTROL) {
        response_builder = response_builder.header(header::CACHE_CONTROL, cache_control);
    }
    if let Some(etag) = response.headers().get(reqwest_header::ETAG) {
        response_builder = response_builder.header(header::ETAG, etag);
    }
    if let Some(last_modified) = response.headers().get(reqwest_header::LAST_MODIFIED) {
        response_builder = response_builder.header(header::LAST_MODIFIED, last_modified);
    }
    response_builder
}
