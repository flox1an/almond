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
use tracing::{debug, warn};

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

/// How long a proven-absent hash stays absent before upstream is retried.
const UPSTREAM_MISS_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Where a requested blob's bytes come from.
///
/// The decision used to exist only as control flow inside a 300-line handler,
/// which is why the ETag, HEAD, payment and statistics preamble was written
/// out once per source and drifted between them. Naming the outcome lets the
/// serving path be written once.
enum BlobSource {
    /// Held by this server, on disk or in the native backend.
    Indexed(std::sync::Arc<crate::models::FileMetadata>),
    /// A read-only file from the serve-files directory.
    ServeFile(std::sync::Arc<crate::models::ServeFileMetadata>),
    /// Not held here. The upstream tiers must be walked.
    Upstream(Box<UpstreamPlan>),
    /// Proven absent upstream recently enough to answer without asking again.
    RecentlyMissing,
}

/// The request-scoped upstream inputs, after feature gating and trust checks.
pub struct UpstreamPlan {
    custom_origin: Option<String>,
    xs_servers: Option<Vec<String>>,
    author_pubkey: Option<nostr_relay_pool::prelude::PublicKey>,
}

impl UpstreamPlan {
    /// A plan naming specific servers is request-scoped, so its failures say
    /// nothing about whether the blob exists anywhere else and must not be
    /// cached as a miss.
    const fn names_specific_servers(&self) -> bool {
        self.custom_origin.is_some() || self.xs_servers.is_some()
    }
}

/// The bytes behind a resolved blob, and who can read them.
enum BlobBytes<'a> {
    LocalFile(&'a std::path::Path),
    Native {
        storage: &'a crate::services::native_storage::SharedNativeS3Storage,
        key: &'a str,
    },
}

/// Handle file requests (GET/HEAD)
pub async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<FileRequestQuery>,
    req: Request,
) -> Result<Response, AppError> {
    let Some(file_hash) = crate::utils::get_sha256_hash_from_filename(&filename) else {
        return Err(AppError::NotFound("Invalid filename format".to_string()));
    };

    // Split the request up front: a borrowed `Request` is not `Send`, because
    // its body is not `Sync`, and every path below awaits.
    let (parts, _body) = req.into_parts();
    let headers = &parts.headers;
    let method = &parts.method;

    match resolve_blob(&state, file_hash, &query).await? {
        BlobSource::Indexed(metadata) => {
            let bytes = match &metadata.location {
                FileLocation::Local(path) => BlobBytes::LocalFile(path),
                FileLocation::S3 { key } => BlobBytes::Native {
                    storage: state.native_s3.as_ref().ok_or_else(|| {
                        AppError::ServiceUnavailable("S3 storage is not configured".to_string())
                    })?,
                    key,
                },
            };
            serve_blob(
                &state,
                headers,
                method,
                file_hash,
                metadata.size,
                metadata.mime_type.as_deref(),
                bytes,
            )
            .await
        }
        BlobSource::ServeFile(metadata) => {
            serve_blob(
                &state,
                headers,
                method,
                file_hash,
                metadata.size,
                metadata.mime_type.as_deref(),
                BlobBytes::LocalFile(&metadata.path),
            )
            .await
        }
        BlobSource::RecentlyMissing => Err(AppError::NotFound(
            "File not found (cached upstream failure)".to_string(),
        )),
        BlobSource::Upstream(plan) => {
            fetch_from_upstream(&state, &filename, file_hash, headers, method, *plan).await
        }
    }
}

/// Decide where this blob comes from, without building any response.
async fn resolve_blob(
    state: &AppState,
    file_hash: &str,
    query: &FileRequestQuery,
) -> Result<BlobSource, AppError> {
    if let Some(metadata) = find_file(&state.file_index, file_hash).await {
        return Ok(BlobSource::Indexed(metadata));
    }

    // An object can exist in the native backend without being indexed yet, for
    // instance after a restart against a pre-populated bucket.
    if let Some(s3) = &state.native_s3 {
        if let Some(metadata) = s3.find(file_hash).await? {
            let published =
                file_storage::publish_existing_metadata(state, file_hash.to_owned(), metadata)
                    .await?;
            return Ok(BlobSource::Indexed(published));
        }
    }

    if let Some(metadata) =
        crate::services::serve_files::get_serve_file(&state.serve_file_index, file_hash).await
    {
        return Ok(BlobSource::ServeFile(metadata));
    }

    let plan = upstream_plan(state, query).await?;
    if !plan.names_specific_servers() && upstream_recently_missed(state, file_hash).await {
        return Ok(BlobSource::RecentlyMissing);
    }
    Ok(BlobSource::Upstream(Box::new(plan)))
}

/// Apply the custom-upstream-origin feature gate to the request's query.
///
/// Ignored parameters are warned about rather than silently dropped, and an
/// `?as=` author is rejected outright when the feature demands web of trust.
async fn upstream_plan(
    state: &AppState,
    query: &FileRequestQuery,
) -> Result<UpstreamPlan, AppError> {
    let enabled = state.feature_custom_upstream_origin_enabled.is_enabled();
    let requires_wot = state.feature_custom_upstream_origin_enabled.requires_wot();

    if !enabled {
        if query.origin.is_some() || !query.xs.is_empty() || !query.servers.is_empty() {
            warn!("Upstream origin parameters provided but FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED is disabled, ignoring");
        }
        return Ok(UpstreamPlan {
            custom_origin: None,
            xs_servers: None,
            author_pubkey: None,
        });
    }

    // `xs` takes priority; `servers` is the legacy spelling of the same list.
    let xs_servers = if query.xs.is_empty() {
        (!query.servers.is_empty()).then(|| query.servers.clone())
    } else {
        Some(query.xs.clone())
    };

    let mut author_pubkey = None;
    if let Some(author) = &query.author_pubkey {
        match blossom_servers::parse_pubkey(author) {
            Ok(pubkey) => {
                if requires_wot
                    && !crate::services::auth::is_pubkey_authorized(&pubkey, state).await
                {
                    warn!(
                        "Author pubkey {} not in Web of Trust, rejecting upstream lookup",
                        pubkey.to_hex()
                    );
                    return Err(AppError::Forbidden(
                        "Author pubkey not in Web of Trust".to_string(),
                    ));
                }
                author_pubkey = Some(pubkey);
            }
            Err(error) => warn!("Invalid pubkey in 'as' parameter: {author} ({error})"),
        }
    }

    Ok(UpstreamPlan {
        custom_origin: query.origin.clone(),
        xs_servers,
        author_pubkey,
    })
}

/// Whether upstream already proved this hash absent recently.
async fn upstream_recently_missed(state: &AppState, file_hash: &str) -> bool {
    let failed = state.failed_upstream_lookups.read().await;
    failed
        .get(file_hash)
        .is_some_and(|missed_at| missed_at.elapsed() < UPSTREAM_MISS_TTL)
}

/// Walk the upstream tiers, in whichever mode is configured.
async fn fetch_from_upstream(
    state: &AppState,
    filename: &str,
    file_hash: &str,
    headers: &axum::http::HeaderMap,
    method: &Method,
    plan: UpstreamPlan,
) -> Result<Response, AppError> {
    let custom_origin = plan.custom_origin.as_deref();
    let xs_servers = plan.xs_servers.as_deref();

    let result = if state.upstream_mode.is_redirect() {
        crate::handlers::upstream::try_upstream_redirect(
            state,
            filename,
            custom_origin,
            xs_servers,
            plan.author_pubkey.as_ref(),
            state.upstream_mode.caches_in_background(),
        )
        .await
    } else {
        crate::handlers::upstream::try_upstream_servers(
            state,
            filename,
            headers,
            method,
            custom_origin,
            xs_servers,
            plan.author_pubkey.as_ref(),
        )
        .await
    };

    if let Ok(response) = result {
        return Ok(response);
    }

    if !plan.names_specific_servers() {
        state
            .failed_upstream_lookups
            .write()
            .await
            .insert(file_hash.to_string(), std::time::Instant::now());
        debug!("Added {file_hash} to failed upstream lookups cache");
    }
    Err(AppError::NotFound("File not found".to_string()))
}

/// Turn a resolved blob into a response.
///
/// Written once, whatever holds the bytes: revalidation, HEAD, payment and
/// statistics used to be repeated per source, and the payment step had already
/// drifted between the copies.
async fn serve_blob(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    method: &Method,
    file_hash: &str,
    size: u64,
    mime_type: Option<&str>,
    bytes: BlobBytes<'_>,
) -> Result<Response, AppError> {
    let etag = blob_etag(file_hash);
    if let Some(response) = check_not_modified(headers, &etag)? {
        return Ok(response);
    }

    if method == Method::HEAD {
        return build_blob_head_response(mime_type, size, &etag);
    }

    cashu::charge(state, headers, cashu::PaidOperation::Download, size).await?;
    track_download_stats(state, size);

    match bytes {
        BlobBytes::LocalFile(path) => serve_file_with_range(path, mime_type, headers, &etag).await,
        BlobBytes::Native { storage, key } => {
            serve_s3_with_range(storage, key, mime_type, size, headers, &etag).await
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(custom_origin: Option<&str>, xs_servers: Option<&[&str]>) -> UpstreamPlan {
        UpstreamPlan {
            custom_origin: custom_origin.map(ToString::to_string),
            xs_servers: xs_servers
                .map(|servers| servers.iter().map(|s| (*s).to_string()).collect()),
            author_pubkey: None,
        }
    }

    #[test]
    fn a_plain_lookup_may_be_remembered_as_a_miss() {
        // Nothing about this request is caller-specific, so upstream saying no
        // is a fact about the blob and worth caching.
        assert!(!plan(None, None).names_specific_servers());
    }

    #[test]
    fn a_request_scoped_server_list_must_not_poison_the_miss_cache() {
        // These failures say only that the servers *this caller named* did not
        // have the blob. Caching that would hide it from everyone else for an
        // hour.
        assert!(plan(Some("https://origin.example"), None).names_specific_servers());
        assert!(plan(None, Some(&["https://xs.example"])).names_specific_servers());
        assert!(plan(
            Some("https://origin.example"),
            Some(&["https://xs.example"])
        )
        .names_specific_servers());
    }
}
