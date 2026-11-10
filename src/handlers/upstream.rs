//! Upstream fetch: serve a blob this server does not hold yet.
//!
//! # Proxy mode (`try_upstream_servers`)
//!
//! Every request goes through the shared fill registry in
//! `services::download`, so one live fill makes one upstream transfer no
//! matter how many clients ask, how slowly they read, or how they slice it.
//!
//! - **Owner.** The first request for a session key spawns `run_fill` and then
//!   waits like everyone else. The owner is detached: a requester that
//!   disconnects during source negotiation or mid-body cancels nothing.
//! - **Full fetch.** The owner walks the upstream tiers (SSRF checks and tier
//!   order unchanged) and sends each source one `GET` with no `Range`, no
//!   conditional or client headers, and `Accept-Encoding: identity`. It accepts
//!   only a whole body: `200`, or a `206` covering `bytes 0-(N-1)/N`.
//! - **Local answers.** Every request, the creator included, is answered from
//!   the spool: `HEAD` from the declared metadata, ranges by tailing the spool
//!   from their own offset, preconditions evaluated here.
//!   - Known length: any satisfiable range — near, far, suffix — answers `206`
//!     at once and its body waits for the bytes. Out of range is `416`;
//!     malformed or multi-range is ignored for a full `200`.
//!   - Unknown length: a ranged request waits for the fill to complete and
//!     then resolves against the real size. Its headers are held meanwhile, so
//!     the router timeout can end it; it never opens a second transfer. A plain
//!     `GET` streams chunked as bytes arrive and ends only once verified.
//! - **Verification.** In-flight responses are `private, no-store` and carry
//!   no `ETag`: the bytes are unverified until the owner checks length and
//!   SHA-256, before anything reaches the index. For a known length the final
//!   byte is withheld until that check, so a full body (or any range touching
//!   the end) fails instead of completing on a bad hash. A range that ends
//!   earlier can complete before verification; those bytes cannot be
//!   authenticated retroactively. For a known length the held byte is
//!   released on verification (length + SHA-256), not on publication: if
//!   storing the verified blob then fails, readers already holding the spool
//!   still finish with correct bytes. An unknown-length body can only end on
//!   `Complete`, so a publication failure aborts it.
//! - **Failure statuses.** Only a walk where every source answered 404/410
//!   is `404`; transport errors, other statuses, partial or encoded bodies
//!   make it `502`, which callers must not remember as a miss. Disk admission
//!   failure is `507`; full fill or reader slots are `503`.
//! - **Over the cache limit** (`MAX_UPSTREAM_DOWNLOAD_SIZE_MB`, capped by
//!   `MAX_BLOB_SIZE_MB`): nothing is spooled and the one-transfer promise does
//!   not hold. The first `GET` waiter streams the owner's response as is — a
//!   full body even when it asked for a range, which RFC 9110 permits. `HEAD`
//!   answers from that response's headers without a body. Other waiters, and
//!   requests for the next `UNCACHEABLE_TTL`, proxy their own request with
//!   their own `Range`.
//!
//! # Redirect modes (`try_upstream_redirect`)
//!
//! The client is sent to upstream with a `307` and downloads from there, so
//! no one-transfer promise applies to it. Redirect-and-cache starts the same
//! shared fill proxy mode uses, under the same key, so concurrent redirects
//! never race each other's fills.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
    Json,
};
use futures_util::StreamExt;
use reqwest::header as reqwest_header;
use serde_json::json;
use sha2::Digest;
use tokio::{fs::File, io::AsyncWriteExt, sync::watch};
use tracing::{debug, error, info, warn};

use crate::constants::{CACHE_CONTROL_IMMUTABLE, DEFAULT_MIME_TYPE};
use crate::helpers::{
    copy_headers_to_reqwest, extract_content_type_from_response, get_extension_from_mime,
};
use crate::models::AppState;
use crate::services::download::{
    DownloadHandle, DownloadPhase, DownloadProgress, FillLease, FillState, Join, SessionKey,
    UncacheableResponse,
};
use crate::services::file_storage::{self, TempBlob};
use crate::services::intake;
use crate::services::tail::create_tailing_stream;
use crate::services::upstream_candidates;
use crate::utils::{find_file, get_sha256_hash_from_filename, parse_range_header, RangeSpec};

/// SHA-256 of the empty body: the only blob a zero-length response can be.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Cache policy for anything served before (or instead of) a verified copy.
const NO_STORE: &str = "private, no-store";

/// Request-scoped sources, owned so the fill can outlive the request.
struct Sources {
    custom_origin: Option<String>,
    xs_servers: Option<Vec<String>>,
    author_pubkey: Option<nostr_sdk::prelude::PublicKey>,
}

/// How a request relates to the shared fill for its key.
enum Attached {
    /// `created` is true only for the request that spawned the owner.
    Session {
        session: watch::Receiver<FillState>,
        created: bool,
    },
    /// The key recently produced an over-limit blob: proxy directly.
    Uncacheable,
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
        "max_download_size_mb": state.max_upstream_download_size_bytes / (1024 * 1024)
    });

    Json(response)
}

/// Lowercase hash of `filename`, the canonical form for keys and verification.
fn canonical_hash(filename: &str) -> String {
    get_sha256_hash_from_filename(filename)
        .unwrap_or(filename)
        .to_ascii_lowercase()
}

/// Attach to the live fill for this blob and source context, spawning its
/// owner if none is live. `503` when a new fill would exceed the fill ceiling.
fn attach_fill(
    state: &AppState,
    filename: &str,
    hash: &str,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
) -> Result<Attached, StatusCode> {
    let key = SessionKey::new(hash, custom_origin, xs_servers, author_pubkey);
    match state.upstream_fills.join(&key)? {
        Join::Waiter(session) => Ok(Attached::Session {
            session,
            created: false,
        }),
        Join::Uncacheable => Ok(Attached::Uncacheable),
        Join::Owner(lease) => {
            // Subscribe before spawning so the owner's decision cannot be missed.
            let session = lease.subscribe();
            let sources = Sources {
                custom_origin: custom_origin.map(ToString::to_string),
                xs_servers: xs_servers.map(<[String]>::to_vec),
                author_pubkey: author_pubkey.cloned(),
            };
            tokio::spawn(run_fill(
                state.clone(),
                lease,
                sources,
                filename.to_string(),
                hash.to_string(),
            ));
            Ok(Attached::Session {
                session,
                created: true,
            })
        }
    }
}

/// Fetch a blob from upstream through the shared fill and answer this request
/// from it. Prioritization: `custom_origin` → `xs_servers` → `UPSTREAM_SERVERS`
/// → user servers (lazy fetch). See the module docs for the full contract.
pub async fn try_upstream_servers(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
) -> Result<Response, StatusCode> {
    let hash = canonical_hash(filename);
    let (mut session, created) = match attach_fill(
        state,
        filename,
        &hash,
        custom_origin,
        xs_servers,
        author_pubkey,
    )? {
        Attached::Session { session, created } => (session, created),
        Attached::Uncacheable => {
            return proxy_request_to_upstream(
                state,
                filename,
                headers,
                method,
                custom_origin,
                xs_servers,
                author_pubkey,
            )
            .await
        }
    };

    // No timeout here on purpose: giving up on the owner could only mean
    // opening a second transfer. The owner's walk is bounded by the upstream
    // client's connect/read timeouts, this request by the router's.
    let decided = session
        .wait_for(|state| !matches!(state, FillState::Pending))
        .await
        .map_or(FillState::Failed(StatusCode::BAD_GATEWAY), |state| {
            state.clone()
        });

    match decided {
        FillState::Ready(handle) => {
            let response =
                serve_from_spool(state, &handle, &hash, filename, headers, method).await?;
            track_served(state, method, &response, !created);
            Ok(response)
        }
        FillState::Uncacheable(over_limit) => {
            serve_uncacheable(
                state,
                &over_limit,
                filename,
                headers,
                method,
                custom_origin,
                xs_servers,
                author_pubkey,
            )
            .await
        }
        FillState::Failed(status) => Err(status),
        // `wait_for` only returns a decided state.
        FillState::Pending => Err(StatusCode::BAD_GATEWAY),
    }
}

/// Count a response served from a spool. Requests that attached to someone
/// else's fill are also counted as coalesced.
fn track_served(state: &AppState, method: &Method, response: &Response<Body>, coalesced: bool) {
    if coalesced {
        state.metrics.track_coalesced_request();
    }
    if *method == Method::HEAD || !response.status().is_success() {
        return;
    }
    match response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(length) => state.metrics.track_download(length),
        None => state.metrics.files_downloaded.inc(),
    }
}

/// The fill owner, detached from every request.
///
/// Ends by publishing exactly one decision (`Ready`, `Uncacheable`, or
/// `Failed`); dropping the lease on return ends the session, frees the fill
/// slot, and fails anything left undecided (see `FillLease`).
async fn run_fill(
    state: AppState,
    lease: FillLease,
    sources: Sources,
    filename: String,
    hash: String,
) {
    if let Some(handle) = published_spool(&state, &hash).await {
        debug!("Blob {hash} was published while this fill was starting");
        lease.publish(FillState::Ready(handle));
        return;
    }

    let limit = intake::size_limit(&state, intake::Intake::UpstreamFetch);
    let mut walk = upstream_candidates::Walk::new(
        &state,
        &filename,
        sources.custom_origin.as_deref(),
        sources.xs_servers.as_deref(),
        sources.author_pubkey.as_ref(),
    );

    // Only a walk where every source said 404/410 (or there were none) proves
    // the blob absent. Anything else is an upstream problem: 502, which the
    // caller must not remember as a miss.
    let mut failure = StatusCode::NOT_FOUND;
    while let Some(candidate) = walk.next().await {
        let response = match state
            .upstream_client
            .get(&candidate.url)
            .header(reqwest_header::ACCEPT_ENCODING, "identity")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                debug!(
                    "Failed to fetch from {} {}: {error}",
                    candidate.tier.label(),
                    candidate.url
                );
                failure = StatusCode::BAD_GATEWAY;
                continue;
            }
        };
        let Some(total_len) = full_body_length(&response) else {
            debug!(
                "{} {} returned {} without a usable full body",
                candidate.tier.label(),
                candidate.url,
                response.status()
            );
            if !is_absent(response.status().as_u16()) {
                failure = StatusCode::BAD_GATEWAY;
            }
            continue;
        };
        if total_len == Some(0) && hash != EMPTY_SHA256 {
            debug!("{} returned an empty body for {hash}", candidate.url);
            failure = StatusCode::BAD_GATEWAY;
            continue;
        }

        let content_type = extract_content_type_from_response(response.headers());
        if total_len.is_some_and(|length| length > limit) {
            info!(
                "Blob {hash} is {total_len:?} bytes, above the {limit} byte upstream cache limit: proxying without caching"
            );
            lease.mark_uncacheable();
            lease.publish(FillState::Uncacheable(Arc::new(UncacheableResponse {
                content_type,
                total_len,
                response: Mutex::new(Some(response)),
            })));
            return;
        }

        debug!(
            "Filling {hash} from {} {}",
            candidate.tier.label(),
            candidate.url
        );
        spool(
            &state,
            &lease,
            response,
            &candidate.url,
            &hash,
            content_type,
            total_len,
            limit,
        )
        .await;
        return;
    }

    lease.publish(FillState::Failed(failure));
}

/// Statuses that say a source does not have the blob, as opposed to failing.
fn is_absent(status: u16) -> bool {
    status == 404 || status == 410
}

/// Whether upstream sent a content-coded body. The upstream client never
/// decodes, and every request asks for identity, so such bytes are not the
/// blob (and could not be relayed without the header).
fn is_encoded(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest_header::CONTENT_ENCODING)
        .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
}

/// The full body length when `response` carries the whole blob, unencoded,
/// from byte 0.
///
/// `Some(None)` is a `200` without a declared length. A `206` qualifies only
/// as `bytes 0-(N-1)/N`: some origins answer every `GET` that way. Anything
/// else (`204`, a partial `206`, a content-coded body, errors) is `None` and
/// the next source is tried.
fn full_body_length(response: &reqwest::Response) -> Option<Option<u64>> {
    if is_encoded(response) {
        return None;
    }
    match response.status().as_u16() {
        200 => Some(response.content_length()),
        206 => {
            let content_range = response
                .headers()
                .get(reqwest_header::CONTENT_RANGE)?
                .to_str()
                .ok()?;
            full_content_range(content_range).map(Some)
        }
        _ => None,
    }
}

/// `N` when a `Content-Range` value is exactly `bytes 0-(N-1)/N`.
fn full_content_range(value: &str) -> Option<u64> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let total = total.parse::<u64>().ok()?;
    let (start, end) = range.split_once('-')?;
    (start == "0" && end.parse::<u64>().ok()?.checked_add(1)? == total).then_some(total)
}

/// A blob already in the index, as a completed spool.
///
/// A request can miss the index, then reach the registry just after the
/// previous fill published and ended its session. That fill published before
/// removing its session, and this session exists before this check, so the
/// index is guaranteed to show it here: no second transfer, no missing file.
async fn published_spool(state: &AppState, hash: &str) -> Option<Arc<DownloadHandle>> {
    let metadata = find_file(&state.file_index, hash).await?;
    let path = file_storage::local_path(&metadata)?.to_path_buf();
    let (progress, _) = watch::channel(DownloadProgress {
        written: metadata.size,
        phase: DownloadPhase::Complete,
    });
    Some(Arc::new(DownloadHandle {
        temp_path: path.clone(),
        content_type: metadata
            .mime_type
            .clone()
            .unwrap_or_else(|| DEFAULT_MIME_TYPE.to_string()),
        total_len: Some(metadata.size),
        progress,
        final_path: OnceLock::from(path),
    }))
}

/// Admit, spool, verify and publish one response. Publishes `Ready` once the
/// spool exists, and drives the handle to `Complete` or `Failed`.
#[allow(clippy::too_many_arguments)]
async fn spool(
    state: &AppState,
    lease: &FillLease,
    response: reqwest::Response,
    url: &str,
    hash: &str,
    content_type: String,
    total_len: Option<u64>,
    limit: u64,
) {
    // An undeclared length may grow to the cache limit, so it reserves all of
    // it up front. Released when this function returns, after publication.
    let _reservation = match state
        .upstream_fills
        .reserve_disk(state, total_len.unwrap_or(limit))
        .await
    {
        Ok(reservation) => reservation,
        Err(status) => {
            lease.publish(FillState::Failed(status));
            return;
        }
    };

    let extension = get_extension_from_mime(&content_type);
    let temp = TempBlob::reserve(state, "upstream", extension.as_deref());
    let writer = match file_storage::ensure_temp_dir(state).await {
        Ok(_) => File::create(temp.path())
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let writer = match writer {
        Ok(writer) => writer,
        Err(error) => {
            error!("Failed to create upstream spool for {hash}: {error}");
            lease.publish(FillState::Failed(StatusCode::INTERNAL_SERVER_ERROR));
            return;
        }
    };

    // The file exists before `Ready` is published, so every reader that sees
    // the handle can open it.
    let (progress, _) = watch::channel(DownloadProgress {
        written: 0,
        phase: DownloadPhase::Running,
    });
    let handle = Arc::new(DownloadHandle {
        temp_path: temp.path().to_path_buf(),
        content_type,
        total_len,
        progress,
        final_path: OnceLock::new(),
    });
    lease.publish(FillState::Ready(handle.clone()));

    match fill_spool(state, lease, &handle, response, writer, temp, hash, limit).await {
        Ok(size) => {
            state.metrics.track_upstream_download(url, size);
            handle
                .progress
                .send_modify(|progress| progress.phase = DownloadPhase::Complete);
            info!("Upstream fill completed: {url} -> {hash} ({size} bytes)");
        }
        Err(error) => {
            warn!("Upstream fill failed for {url}: {error}");
            handle
                .progress
                .send_modify(|progress| progress.phase = DownloadPhase::Failed);
        }
    }
}

/// Stream the body into the spool, verify it, and publish it to the index.
///
/// On any error `temp` drops and unlinks the spool. Readers that already
/// opened it keep their handle and see `Failed` as a stream error.
#[allow(clippy::too_many_arguments)]
async fn fill_spool(
    state: &AppState,
    lease: &FillLease,
    handle: &DownloadHandle,
    response: reqwest::Response,
    mut writer: File,
    temp: TempBlob,
    hash: &str,
    limit: u64,
) -> Result<u64, String> {
    let mut chunks = response.bytes_stream();
    let mut hasher = sha2::Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        size += chunk.len() as u64;
        if handle.total_len.is_some_and(|total| size > total) {
            return Err(format!(
                "body is longer than the declared {:?} bytes",
                handle.total_len
            ));
        }
        if size > limit {
            // Only an undeclared length gets here; a declared over-limit body
            // never spools. Proxy this key directly for a while.
            lease.mark_uncacheable();
            return Err(format!(
                "body exceeded the {limit} byte upstream cache limit"
            ));
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        // `flush` waits for tokio's blocking write, so the bytes are readable
        // through other file handles before progress announces them.
        writer.flush().await.map_err(|error| error.to_string())?;
        hasher.update(&chunk);
        // Hold back a declared body's final byte until verification.
        let readable = handle
            .total_len
            .map_or(size, |total| size.min(total.saturating_sub(1)));
        handle
            .progress
            .send_modify(|progress| progress.written = readable);
    }
    drop(writer);

    if handle.total_len.is_some_and(|total| total != size) {
        return Err(format!(
            "body length mismatch: declared {:?}, received {size}",
            handle.total_len
        ));
    }
    let sha256 = hex::encode(hasher.finalize());
    if sha256 != hash {
        return Err(format!(
            "hash mismatch: requested {hash}, received {sha256}"
        ));
    }

    // Verified: release the held-back byte. Readers holding the spool finish
    // now, even if publication below fails.
    handle
        .progress
        .send_modify(|progress| progress.written = size);

    let published = intake::accept(
        state,
        temp,
        intake::Accepted {
            sha256,
            origin: crate::models::BlobOrigin::UpstreamCache,
            size,
            extension: get_extension_from_mime(&handle.content_type),
            mime_type: Some(handle.content_type.clone()),
            expiration: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    // Publication renamed the spool. Late readers that find it gone wait for
    // `Complete` and open this path instead.
    if let Some(path) = file_storage::local_path(&published.metadata) {
        let _ = handle.final_path.set(path.to_path_buf());
    }
    Ok(size)
}

/// Answer one request from a spool.
async fn serve_from_spool(
    state: &AppState,
    handle: &DownloadHandle,
    hash: &str,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
) -> Result<Response<Body>, StatusCode> {
    let etag = format!("\"{hash}\"");

    // A body-bearing request takes its reader slot before any wait below, so
    // the reader ceiling also bounds requests parked on validators or on an
    // unknown length. `HEAD` never streams and takes none.
    let slot = if *method == Method::HEAD {
        None
    } else {
        Some(state.upstream_fills.reader_slot()?)
    };

    // `If-Match` (strong) and a matching `If-None-Match` (weak) compare against
    // the hash, which only becomes this response's validator once the fill is
    // verified, so both wait for completion. Date validators are ignored: no
    // trustworthy modification time exists for a blob.
    let if_match = etag_list_matches(headers, &header::IF_MATCH, &etag, false);
    let not_modified =
        etag_list_matches(headers, &header::IF_NONE_MATCH, &etag, true) == Some(true);
    if if_match.is_some() || not_modified {
        wait_for_completion(handle).await?;
        if if_match == Some(false) {
            return empty_response(StatusCode::PRECONDITION_FAILED, NO_STORE);
        }
        if not_modified {
            let mut response = empty_response(StatusCode::NOT_MODIFIED, CACHE_CONTROL_IMMUTABLE)?;
            response.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&etag).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            );
            return Ok(response);
        }
    }

    if *method == Method::HEAD {
        return head_response(&handle.content_type, handle.total_len, filename);
    }

    // `If-Range` holds only for exactly our strong tag; a weak tag, a date, or
    // anything else means the client's partial copy may differ, so the range
    // is ignored and everything is sent.
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| if_range_holds(headers, &etag));
    let total_len = match (range, handle.total_len) {
        // A range cannot be resolved against an unknown size.
        (Some(_), None) => Some(wait_for_completion(handle).await?),
        (_, known) => known,
    };
    let (status, start, end) = match (range, total_len) {
        (Some(range), Some(total)) => match parse_range_header(range, total) {
            RangeSpec::Satisfiable { start, end } => {
                (StatusCode::PARTIAL_CONTENT, start, Some(end + 1))
            }
            RangeSpec::Unsatisfiable => {
                let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE, NO_STORE)?;
                let headers = response.headers_mut();
                headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                headers.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{total}"))
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                return Ok(response);
            }
            RangeSpec::Ignore => (StatusCode::OK, 0, Some(total)),
        },
        (_, total) => (StatusCode::OK, 0, total),
    };

    let reader = open_spool(handle).await.map_err(|error| {
        warn!(
            "Failed to open upstream spool {}: {error}",
            handle.temp_path.display()
        );
        match handle.progress.borrow().phase {
            DownloadPhase::Failed => StatusCode::BAD_GATEWAY,
            // The verified spool was dropped because an equal copy already
            // existed elsewhere (for example in S3). That copy is now indexed,
            // so a retry is served from it; never fetch upstream again here.
            DownloadPhase::Complete if handle.final_path.get().is_none() => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    })?;
    let stream = create_tailing_stream(reader, handle.progress.subscribe(), start, end)
        .await
        .map_err(|error| {
            error!("Failed to attach to upstream spool: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    // The reader slot rides inside the body: released when the body finishes
    // or the client goes away.
    let body = Body::from_stream(stream.map(move |chunk| {
        let _slot = &slot;
        chunk
    }));

    let mut builder = Response::builder().status(status);
    if let Some(end) = end {
        builder = builder.header(header::CONTENT_LENGTH, end - start);
        if status == StatusCode::PARTIAL_CONTENT {
            let total = total_len.unwrap_or(end);
            builder = builder.header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{total}", end - 1),
            );
        }
    }
    let response = builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(apply_streaming_headers(
        response,
        &handle.content_type,
        filename,
    ))
}

/// RFC 9110 §8.8.3.2 comparison of an entity-tag list header against `etag`:
/// weak for `If-None-Match`, strong for `If-Match` (a `W/` tag never matches
/// strongly). `None` when the header is absent; an unreadable header matches
/// nothing.
fn etag_list_matches(
    headers: &HeaderMap,
    name: &HeaderName,
    etag: &str,
    weak: bool,
) -> Option<bool> {
    let Ok(list) = headers.get(name)?.to_str() else {
        return Some(false);
    };
    Some(list.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*"
            || candidate == etag
            || (weak && candidate.strip_prefix("W/") == Some(etag))
    }))
}

/// RFC 9110 §13.1.5: a range is honoured when `If-Range` is absent or is
/// exactly our strong tag. Weak tags and dates never hold.
fn if_range_holds(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_RANGE)
        .is_none_or(|value| value.to_str().is_ok_and(|value| value.trim() == etag))
}

/// Wait for a fill to end; its verified length when it completed.
async fn wait_for_completion(handle: &DownloadHandle) -> Result<u64, StatusCode> {
    let mut progress = handle.progress.subscribe();
    let done = progress
        .wait_for(|progress| progress.phase != DownloadPhase::Running)
        .await
        .map(|progress| *progress);
    match done {
        Ok(DownloadProgress {
            written,
            phase: DownloadPhase::Complete,
        }) => Ok(written),
        _ => Err(StatusCode::BAD_GATEWAY),
    }
}

/// Open a spool for reading, following it to its published path.
///
/// The spool is created before `Ready` and only disappears at the end: renamed
/// by publication or unlinked on failure. Either way the terminal phase says
/// which, and `final_path` is set before `Complete`.
async fn open_spool(handle: &DownloadHandle) -> std::io::Result<File> {
    match File::open(&handle.temp_path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut progress = handle.progress.subscribe();
            let _ = progress
                .wait_for(|progress| progress.phase != DownloadPhase::Running)
                .await;
            match handle.final_path.get() {
                Some(path) => File::open(path).await,
                None => Err(error),
            }
        }
        result => result,
    }
}

fn empty_response(
    status: StatusCode,
    cache_control: &'static str,
) -> Result<Response<Body>, StatusCode> {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// `HEAD` from declared metadata; never a body.
fn head_response(
    content_type: &str,
    total_len: Option<u64>,
    filename: &str,
) -> Result<Response<Body>, StatusCode> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(length) = total_len {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    Ok(apply_streaming_headers(response, content_type, filename))
}

/// Answer a request whose fill found the blob above the cache limit.
#[allow(clippy::too_many_arguments)]
async fn serve_uncacheable(
    state: &AppState,
    over_limit: &UncacheableResponse,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
) -> Result<Response, StatusCode> {
    if *method == Method::HEAD {
        return head_response(&over_limit.content_type, over_limit.total_len, filename);
    }
    // The owner's transfer is already running: the first GET streams it as is
    // instead of starting another. Everyone after asks upstream themselves.
    let taken = over_limit
        .response
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    match taken {
        Some(response) => proxy_upstream_response(response, &over_limit.content_type, filename),
        None => {
            proxy_request_to_upstream(
                state,
                filename,
                headers,
                method,
                custom_origin,
                xs_servers,
                author_pubkey,
            )
            .await
        }
    }
}

/// Find the blob upstream and `307` the client there.
///
/// Uses `HEAD` to verify a source has it. With `cache_in_background`, also
/// starts (or joins) the shared fill for the same key proxy mode would use;
/// a declared length above the cache limit skips it. The fill walks the tiers
/// itself and verifies like any other fill; the client's own download from
/// upstream is outside this server's control.
pub async fn try_upstream_redirect(
    state: &AppState,
    filename: &str,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
    cache_in_background: bool,
) -> Result<Response, StatusCode> {
    let mut walk =
        upstream_candidates::Walk::new(state, filename, custom_origin, xs_servers, author_pubkey);

    // As in `run_fill`: only all-404/410 is absence; anything else is 502.
    let mut failure = StatusCode::NOT_FOUND;
    while let Some(candidate) = walk.next().await {
        let response = match state.upstream_client.head(&candidate.url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                debug!(
                    "HEAD check failed for {} with status: {}",
                    candidate.url,
                    response.status()
                );
                if !is_absent(response.status().as_u16()) {
                    failure = StatusCode::BAD_GATEWAY;
                }
                continue;
            }
            Err(error) => {
                debug!("HEAD request failed for {}: {error}", candidate.url);
                failure = StatusCode::BAD_GATEWAY;
                continue;
            }
        };

        if cache_in_background {
            let declared = response
                .headers()
                .get(reqwest_header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let limit = intake::size_limit(state, intake::Intake::UpstreamFetch);
            if declared.is_some_and(|length| length > limit) {
                debug!("{filename} is {declared:?} bytes, too large for the background cache");
            } else if let Err(status) = attach_fill(
                state,
                filename,
                &canonical_hash(filename),
                custom_origin,
                xs_servers,
                author_pubkey,
            ) {
                debug!("Skipping background cache fill for {filename}: {status}");
            }
        }

        return build_redirect_response(&candidate.url, filename);
    }

    Err(failure)
}

/// Build a 307 redirect response to the upstream URL.
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
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, upstream_url)
        .header(header::CACHE_CONTROL, NO_STORE)
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

/// Proxy this request to upstream unchanged, `Range` and validators included.
///
/// Only for blobs over the cache limit, where no spool exists to share. `HEAD`
/// stays `HEAD` upstream, so it never pulls a body. `304`, `412` and `416`
/// answer the client's own validators or range and are forwarded as is; only
/// all-404/410 is absence, and other failures are `502`.
async fn proxy_request_to_upstream(
    state: &AppState,
    filename: &str,
    headers: &HeaderMap,
    method: &Method,
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
) -> Result<Response<Body>, StatusCode> {
    debug!("Proxying over-limit request to upstream: {}", filename);

    let client = &state.upstream_client;
    let mut walk =
        upstream_candidates::Walk::new(state, filename, custom_origin, xs_servers, author_pubkey);

    let mut failure = StatusCode::NOT_FOUND;
    while let Some(candidate) = walk.next().await {
        let request = if *method == Method::HEAD {
            client.head(&candidate.url)
        } else {
            client.get(&candidate.url)
        };
        // Identity only: `proxy_upstream_response` does not relay
        // `Content-Encoding`, so a coded body would reach the client as garbage.
        // One that arrives anyway falls through to `502`.
        let request = copy_headers_to_reqwest(headers, request)
            .header(reqwest_header::ACCEPT_ENCODING, "identity");
        match request.send().await {
            Ok(response)
                if (response.status().is_success()
                    || matches!(response.status().as_u16(), 304 | 412 | 416))
                    && !is_encoded(&response) =>
            {
                debug!(
                    "Proxied request to {} {}: {}",
                    candidate.tier.label(),
                    candidate.url,
                    response.status()
                );
                let content_type = extract_content_type_from_response(response.headers());
                return proxy_upstream_response(response, &content_type, filename);
            }
            Ok(response) => {
                debug!(
                    "{} {} returned status: {}",
                    candidate.tier.label(),
                    candidate.url,
                    response.status()
                );
                if !is_absent(response.status().as_u16()) {
                    failure = StatusCode::BAD_GATEWAY;
                }
            }
            Err(error) => {
                warn!(
                    "Failed to proxy to {} {}: {error}",
                    candidate.tier.label(),
                    candidate.url
                );
                failure = StatusCode::BAD_GATEWAY;
            }
        }
    }

    Err(failure)
}

/// Proxy upstream response directly to client
fn proxy_upstream_response(
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
    // Never forward upstream's cache-control: these bytes are unverified, and a
    // CDN caching a partial or failed stream would serve it to everyone.
    response_builder = response_builder
        .header(header::CACHE_CONTROL, NO_STORE)
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

/// Content headers shared by every spool response; always `no-store`.
fn apply_streaming_headers(
    mut response: Response<Body>,
    content_type: &str,
    filename: &str,
) -> Response<Body> {
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

    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
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

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_whole_body_206_counts_as_a_full_fetch() {
        assert_eq!(full_content_range("bytes 0-9/10"), Some(10));
        assert_eq!(full_content_range("bytes 1-9/10"), None);
        assert_eq!(full_content_range("bytes 0-8/10"), None);
        assert_eq!(full_content_range("bytes 0-9/*"), None);
        assert_eq!(full_content_range("garbage"), None);
    }

    #[test]
    fn validators_compare_weakly_only_for_if_none_match() {
        let etag = "\"abc\"";
        let mut headers = HeaderMap::new();
        assert_eq!(
            etag_list_matches(&headers, &header::IF_MATCH, etag, false),
            None
        );
        assert!(if_range_holds(&headers, etag));

        headers.insert(header::IF_NONE_MATCH, "W/\"abc\"".parse().unwrap());
        headers.insert(header::IF_MATCH, "W/\"abc\"".parse().unwrap());
        headers.insert(header::IF_RANGE, "W/\"abc\"".parse().unwrap());
        assert_eq!(
            etag_list_matches(&headers, &header::IF_NONE_MATCH, etag, true),
            Some(true)
        );
        assert_eq!(
            etag_list_matches(&headers, &header::IF_MATCH, etag, false),
            Some(false)
        );
        assert!(!if_range_holds(&headers, etag));

        headers.insert(header::IF_MATCH, "\"x\", \"abc\"".parse().unwrap());
        headers.insert(header::IF_RANGE, "\"abc\"".parse().unwrap());
        assert_eq!(
            etag_list_matches(&headers, &header::IF_MATCH, etag, false),
            Some(true)
        );
        assert!(if_range_holds(&headers, etag));
    }
}
