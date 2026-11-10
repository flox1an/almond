//! End-to-end checks for the shared progressive upstream cache.
//!
//! Every test drives the public router against a local HTTPS origin that
//! counts the real HTTP GETs it receives and decides exactly when its bytes
//! leave. Almond's production URL validation still runs unchanged and needs no
//! DNS: upstreams are configured as public IP literals (`https://1.1.1.1:<port>`),
//! which pass the public-address check, and only the test's upstream client
//! sends its traffic through a tiny local CONNECT proxy that maps exactly those
//! literal `ip:port` targets to the loopback origins. Nothing leaves the host.

// Fixture servers deliberately ignore teardown and best-effort socket errors,
// as `main.rs` does for its own background work.
#![allow(clippy::let_underscore_must_use)]

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use almond::{models::FeatureMode, AppState, Config};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, Method, Request, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tower::ServiceExt;

/// Upper bound for anything a test waits on; generous so slow CI is not flaky.
const DEADLINE: Duration = Duration::from_secs(20);
/// How long a request must stay pending to count as "waiting for bytes".
const STILL_WAITING: Duration = Duration::from_millis(300);
/// Public addresses used as upstream names. They are never contacted: the
/// test proxy routes them to loopback origins.
const ORIGIN_IP: &str = "1.1.1.1";
const OTHER_ORIGIN_IP: &str = "1.0.0.1";

/// How an origin announces the length of the blobs it serves.
#[derive(Clone, Copy)]
enum Length {
    /// `Content-Length` equals the bytes sent; single ranges are honoured.
    Actual,
    /// No `Content-Length`: a chunked body of unknown length.
    Chunked,
    /// Announce this `Content-Length`, whatever is actually sent.
    Claim(usize),
}

/// A loopback HTTPS Blossom origin whose release of bytes the test controls.
struct Origin {
    addr: SocketAddr,
    /// Real HTTP GETs received, whatever their path or headers.
    gets: AtomicUsize,
    /// The `Range` header of each GET, in arrival order.
    ranges: Mutex<Vec<Option<String>>>,
    /// Closed: the origin accepts the GET but sends no response headers.
    headers_open: watch::Sender<bool>,
    /// Closed: the origin stops after `prefix` body bytes.
    body_open: watch::Sender<bool>,
    /// While set, every request is answered with this status and no body.
    failure: Mutex<Option<StatusCode>>,
    /// While set, responses carry this `Content-Encoding` over the raw bytes.
    encoding: Mutex<Option<&'static str>>,
    /// Bytes served per hash (the file name without extension); others 404.
    blobs: HashMap<String, Bytes>,
    length: Length,
    /// Body bytes sent before waiting on `body_open`.
    prefix: usize,
}

impl Origin {
    async fn start(
        blobs: Vec<(String, Bytes)>,
        length: Length,
        prefix: usize,
        headers_open: bool,
    ) -> Arc<Self> {
        install_crypto_provider();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind origin");
        listener.set_nonblocking(true).expect("nonblocking origin");
        let origin = Arc::new(Self {
            addr: listener.local_addr().expect("origin address"),
            gets: AtomicUsize::new(0),
            ranges: Mutex::new(Vec::new()),
            headers_open: watch::Sender::new(headers_open),
            body_open: watch::Sender::new(false),
            failure: Mutex::new(None),
            encoding: Mutex::new(None),
            blobs: blobs.into_iter().collect(),
            length,
            prefix,
        });
        let app = Router::new()
            .route("/{file}", get(serve_origin))
            .with_state(Arc::clone(&origin));
        let server = axum_server::from_tcp_rustls(listener, origin_tls().await).expect("origin");
        tokio::spawn(server.serve(app.into_make_service()));
        origin
    }

    /// An origin holding one blob, stored under its true hash.
    async fn single(data: &Bytes, length: Length, prefix: usize, headers_open: bool) -> Arc<Self> {
        Self::start(
            vec![(sha256_hex(data), data.clone())],
            length,
            prefix,
            headers_open,
        )
        .await
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }

    fn release_headers(&self) {
        self.headers_open.send_replace(true);
    }

    fn release_body(&self) {
        self.body_open.send_replace(true);
    }

    fn fail_with(&self, status: Option<StatusCode>) {
        *self.failure.lock().unwrap() = status;
    }

    fn encode_as(&self, encoding: Option<&'static str>) {
        *self.encoding.lock().unwrap() = encoding;
    }
}

/// Behave like a plain Blossom server: honour one byte range, otherwise send
/// the whole blob. Honouring ranges matters: an Almond that answered a client
/// range with its own ranged upstream request would get bytes immediately and
/// would show up as an extra GET, which is exactly what these tests forbid.
async fn serve_origin(
    State(origin): State<Arc<Origin>>,
    Path(file): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if method == Method::GET {
        origin.gets.fetch_add(1, Ordering::SeqCst);
        origin.ranges.lock().unwrap().push(range.clone());
    }
    let _ = origin.headers_open.subscribe().wait_for(|open| *open).await;

    let failure = *origin.failure.lock().unwrap();
    let hash = file.split('.').next().unwrap_or_default();
    let blob = match (failure, origin.blobs.get(hash)) {
        (Some(status), _) => return status_only(status),
        (None, None) => return status_only(StatusCode::NOT_FOUND),
        (None, Some(blob)) => blob.clone(),
    };

    let response = Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes");
    let encoding = *origin.encoding.lock().unwrap();
    let response = match encoding {
        Some(encoding) => response.header(header::CONTENT_ENCODING, encoding),
        None => response,
    };
    let ranged = match origin.length {
        Length::Actual => range
            .as_deref()
            .and_then(|range| parse_single_range(range, blob.len())),
        Length::Chunked | Length::Claim(_) => None,
    };
    let (response, bytes) = match (ranged, origin.length) {
        (Some((start, end)), _) => (
            response
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", blob.len()),
                )
                .header(header::CONTENT_LENGTH, end - start + 1),
            blob.slice(start..=end),
        ),
        (None, Length::Actual) => (response.header(header::CONTENT_LENGTH, blob.len()), blob),
        (None, Length::Claim(len)) => (response.header(header::CONTENT_LENGTH, len), blob),
        (None, Length::Chunked) => (response, blob),
    };
    if method == Method::HEAD {
        return response.body(Body::empty()).unwrap();
    }

    // The prefix leaves at once; the rest waits for the test to open the gate.
    let prefix = origin.prefix.min(bytes.len());
    let mut body_open = origin.body_open.subscribe();
    let stream = async_stream::stream! {
        if prefix > 0 {
            yield Ok::<_, std::io::Error>(bytes.slice(..prefix));
        }
        if prefix < bytes.len() {
            let _ = body_open.wait_for(|open| *open).await;
            let mut offset = prefix;
            while offset < bytes.len() {
                let end = (offset + 64 * 1024).min(bytes.len());
                yield Ok(bytes.slice(offset..end));
                offset = end;
            }
        }
    };
    response.body(Body::from_stream(stream)).unwrap()
}

fn status_only(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

/// `bytes=a-b`, `bytes=a-` and `bytes=-n` against a known length.
fn parse_single_range(value: &str, len: usize) -> Option<(usize, usize)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let last = len.checked_sub(1)?;
    let (start, end) = if start.is_empty() {
        let suffix: usize = end.parse().ok()?;
        (len.checked_sub(suffix.min(len))?, last)
    } else {
        let start: usize = start.parse().ok()?;
        let end = if end.is_empty() {
            last
        } else {
            end.parse::<usize>().ok()?.min(last)
        };
        (start, end)
    };
    (start <= end).then_some((start, end))
}

/// A minimal HTTP CONNECT proxy for the test's upstream client.
///
/// Only `CONNECT ip:port` targets listed in `routes` are tunnelled, each to
/// its loopback origin; anything else is refused, so a test can never reach
/// the real address behind a public literal.
async fn start_connect_proxy(routes: HashMap<String, SocketAddr>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("proxy address");
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let routes = Arc::clone(&routes);
            tokio::spawn(async move {
                // Read the request head one byte at a time so no byte of the
                // TLS handshake that follows it is consumed here.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if head.len() > 8 * 1024 || client.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let target = head
                    .strip_prefix("CONNECT ")
                    .and_then(|rest| rest.split_whitespace().next());
                let Some(&local) = target.and_then(|target| routes.get(target)) else {
                    let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                    return;
                };
                let Ok(mut origin) = TcpStream::connect(local).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                if client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .is_ok()
                {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut origin).await;
                }
            });
        }
    });
    addr
}

fn install_crypto_provider() {
    // Both rustls backends are compiled in, so one must be chosen explicitly,
    // as `main.rs` does. Losing the race to another test is fine.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn origin_tls() -> axum_server::tls_rustls::RustlsConfig {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![ORIGIN_IP.to_owned(), OTHER_ORIGIN_IP.to_owned()])
            .expect("self-signed origin certificate");
    axum_server::tls_rustls::RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        signing_key.serialize_pem().into_bytes(),
    )
    .await
    .expect("origin TLS config")
}

/// `https://<ip>:<port>` for an origin published under a public literal.
fn upstream_url(ip: &str, origin: &Origin) -> String {
    format!("https://{ip}:{}", origin.addr.port())
}

/// An Almond instance whose upstream client reaches the given local origins.
struct Almond {
    state: AppState,
    app: Router,
    root: PathBuf,
}

impl Almond {
    /// `origins` pairs a public IP literal with the origin it stands for; the
    /// URL Almond sees is [`upstream_url`].
    async fn start(origins: &[(&str, &Origin)], configure: impl FnOnce(&mut Config)) -> Self {
        install_crypto_provider();
        let root = std::env::temp_dir().join(format!(
            "almond-upstream-streaming-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config::defaults();
        cfg.storage_path = root.clone();
        configure(&mut cfg);
        let mut state = almond::build_state(&cfg).await.expect("almond state");

        let routes = origins
            .iter()
            .map(|(ip, origin)| (format!("{ip}:{}", origin.addr.port()), origin.addr))
            .collect();
        let proxy = start_connect_proxy(routes).await;
        // Same settings as production (no redirects, no transparent
        // decompression). Test-only differences: all traffic goes through the
        // routing proxy (which also disables any system proxy), and the
        // self-signed origin certificate is accepted.
        state.upstream_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .proxy(reqwest::Proxy::all(format!("http://{proxy}")).expect("proxy URL"))
            .danger_accept_invalid_certs(true)
            .build()
            .expect("test upstream client");
        let app = almond::create_app(state.clone());
        Self { state, app, root }
    }

    /// Almond configured with one trusted upstream, `https://1.1.1.1:<port>`.
    async fn with_upstream(origin: &Origin, configure: impl FnOnce(&mut Config)) -> Self {
        let upstream = upstream_url(ORIGIN_IP, origin);
        Self::start(&[(ORIGIN_IP, origin)], |cfg| {
            cfg.upstream_servers = vec![upstream];
            configure(cfg);
        })
        .await
    }

    async fn send(&self, uri: &str, headers: &[(&'static str, String)]) -> Response {
        send(&self.app, uri, headers).await
    }

    /// Start a request in the background and collect its whole body there.
    fn spawn_fetch(
        &self,
        uri: &str,
        headers: &[(&'static str, String)],
    ) -> tokio::task::JoinHandle<Reply> {
        let app = self.app.clone();
        let uri = uri.to_owned();
        let headers = headers.to_vec();
        tokio::spawn(async move { collect(send(&app, &uri, &headers).await).await })
    }

    async fn wait_cached(&self, hash: &str) {
        let index = Arc::clone(&self.state.file_index);
        eventually("blob published to the cache index", async || {
            index.contains(hash).await
        })
        .await;
    }
}

impl Drop for Almond {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Status, headers and the full body, or the error that cut the body short.
type Reply = (StatusCode, HeaderMap, Result<Bytes, String>);

async fn send(app: &Router, uri: &str, headers: &[(&'static str, String)]) -> Response {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn collect(response: Response) -> Reply {
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|error| error.to_string());
    (parts.status, parts.headers, body)
}

async fn eventually(what: &str, check: impl AsyncFn() -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_gets(origin: &Origin, count: usize) {
    eventually(&format!("{count} upstream GET(s)"), async || {
        origin.gets() >= count
    })
    .await;
}

/// Deterministic, non-periodic bytes, so a range served from the wrong offset
/// cannot accidentally compare equal.
fn blob(len: usize) -> Bytes {
    let mut state: u32 = 0x9E37_79B9;
    let bytes: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect();
    bytes.into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn range(value: &str) -> Vec<(&'static str, String)> {
    vec![("range", value.to_owned())]
}

fn cache_control(headers: &HeaderMap) -> &str {
    headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn assert_partial(reply: &Reply, data: &Bytes, start: usize, end: usize) {
    let (status, headers, body) = reply;
    assert_eq!(*status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        headers[header::CONTENT_RANGE],
        format!("bytes {start}-{end}/{}", data.len()).as_str()
    );
    assert_eq!(
        body.as_ref().expect("complete range body"),
        &data.slice(start..=end)
    );
}

/// The tracer: a 10 MiB "video" requested by a player (full GET) while other
/// players seek far ahead and read the MP4 tail, all before the origin has
/// sent those bytes. One upstream GET must serve everyone, the first viewer
/// leaving must not abort the fill, and the finished blob must then be served
/// locally without touching the origin again.
#[tokio::test]
async fn one_upstream_get_serves_full_far_and_suffix_readers_then_caches() {
    let data = blob(10 * 1024 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, 64 * 1024, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    // First viewer: response headers and the prefix arrive while the origin
    // still holds the rest of the blob.
    let first = almond.send(&uri, &[]).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert!(
        cache_control(first.headers()).contains("no-store"),
        "bytes not yet verified against the hash must not be cacheable downstream"
    );
    let mut first_body = first.into_body().into_data_stream();
    let first_chunk = first_body
        .next()
        .await
        .expect("first chunk")
        .expect("first chunk bytes");
    assert_eq!(first_chunk, data.slice(..first_chunk.len()));

    // A second full reader that will hang up early.
    let quitter = almond.send(&uri, &[]).await;
    assert_eq!(quitter.status(), StatusCode::OK);
    assert!(cache_control(quitter.headers()).contains("no-store"));

    // Seek far ahead (beyond any small read-ahead window) and read the MP4
    // tail, where `moov` often lives.
    let far = almond.spawn_fetch(&uri, &range("bytes=9437184-9437283"));
    let suffix = almond.spawn_fetch(&uri, &range("bytes=-100"));
    tokio::time::sleep(STILL_WAITING).await;
    assert!(!far.is_finished(), "far range must wait for its bytes");
    assert!(!suffix.is_finished(), "suffix range must wait for the tail");
    assert_eq!(
        origin.gets(),
        1,
        "readers must share the single upstream GET"
    );

    // Every reader that started the fill or joined it disconnects.
    drop(first_body);
    drop(quitter);
    origin.release_body();

    assert_partial(&far.await.unwrap(), &data, 9_437_184, 9_437_283);
    assert_partial(
        &suffix.await.unwrap(),
        &data,
        data.len() - 100,
        data.len() - 1,
    );
    almond.wait_cached(&hash).await;

    let (status, headers, body) = collect(almond.send(&uri, &[]).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!cache_control(&headers).contains("no-store"));
    assert_eq!(body.unwrap(), data);
    assert_eq!(origin.gets(), 1, "a cached blob is served without upstream");
    let ranges = origin.ranges.lock().unwrap().clone();
    assert!(
        matches!(ranges[0].as_deref(), None | Some("bytes=0-")),
        "the shared fill fetches the whole blob, got {ranges:?}"
    );
}

/// An origin slow to send response headers (longer than any follower wait a
/// previous design used) must still see exactly one GET, and the fill must
/// survive the client that triggered it hanging up during that negotiation.
#[tokio::test]
async fn slow_origin_headers_and_a_cancelled_first_client_never_duplicate_the_get() {
    let data = blob(512 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, 0, false).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    let leader = almond.spawn_fetch(&uri, &[]);
    wait_gets(&origin, 1).await;
    let follower = almond.spawn_fetch(&uri, &[]);
    let tail = almond.spawn_fetch(&uri, &range("bytes=-1000"));
    tokio::time::sleep(STILL_WAITING).await;
    leader.abort();

    // Real time on purpose: the origin is a real socket, and the point is to
    // outlast a fixed 5 s follower timeout that used to open a second GET.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        origin.gets(),
        1,
        "waiting followers must not open a second GET"
    );
    assert!(!follower.is_finished());

    origin.release_headers();
    origin.release_body();
    let (status, _, body) = follower.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    assert_partial(
        &tail.await.unwrap(),
        &data,
        data.len() - 1000,
        data.len() - 1,
    );
    almond.wait_cached(&hash).await;
    assert_eq!(origin.gets(), 1);
}

/// Without `Content-Length` nobody knows where the end is, so a suffix range
/// can only be answered once the fill completes. Ranges are then resolved
/// against the verified size, still from the single upstream GET.
#[tokio::test]
async fn ranges_of_a_blob_without_content_length_resolve_after_the_fill() {
    let data = blob(256 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Chunked, 64 * 1024, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    let full = almond.spawn_fetch(&uri, &[]);
    wait_gets(&origin, 1).await;
    let suffix = almond.spawn_fetch(&uri, &range("bytes=-100"));
    let middle = almond.spawn_fetch(&uri, &range("bytes=1000-1999"));
    tokio::time::sleep(STILL_WAITING).await;
    assert!(!suffix.is_finished(), "an unknown end cannot be served yet");
    assert_eq!(origin.gets(), 1);

    origin.release_body();
    let (status, _, body) = full.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    assert_partial(
        &suffix.await.unwrap(),
        &data,
        data.len() - 100,
        data.len() - 1,
    );
    assert_partial(&middle.await.unwrap(), &data, 1000, 1999);
    almond.wait_cached(&hash).await;
    assert_eq!(origin.gets(), 1);
}

/// HTTP conditionals are evaluated by Almond against the blob's own ETag (its
/// quoted hash), never by forwarding them, and a seek that arrives first still
/// starts one whole-blob fill rather than a ranged one.
#[tokio::test]
async fn conditional_and_unsatisfiable_requests_are_answered_from_the_shared_fill() {
    let data = blob(1024 * 1024);
    let hash = sha256_hex(&data);
    let etag = format!("\"{hash}\"");
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, 64 * 1024, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    let seek = almond.spawn_fetch(&uri, &range("bytes=900000-900099"));
    wait_gets(&origin, 1).await;
    let stale = almond.spawn_fetch(
        &uri,
        &[
            ("range", "bytes=0-99".to_owned()),
            ("if-range", format!("\"{}\"", "0".repeat(64))),
        ],
    );
    let fresh = almond.spawn_fetch(
        &uri,
        &[
            ("range", "bytes=100-199".to_owned()),
            ("if-range", etag.clone()),
        ],
    );

    // A matching `If-None-Match` claims the blob exists with this hash, which
    // is only true once the fill has been verified, so it must wait too.
    let not_modified = almond.spawn_fetch(&uri, &[("if-none-match", etag.clone())]);

    // The declared length is already known, so this needs no blob bytes.
    let (status, _, _) = collect(
        almond
            .send(&uri, &range(&format!("bytes={}-", data.len())))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);

    tokio::time::sleep(STILL_WAITING).await;
    assert!(!seek.is_finished(), "the seek must wait for its bytes");
    assert!(
        !not_modified.is_finished(),
        "304 must wait until the hash is verified"
    );
    origin.release_body();

    assert_partial(&seek.await.unwrap(), &data, 900_000, 900_099);
    let (status, _, body) = stale.await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "a stale If-Range gets the whole blob"
    );
    assert_eq!(body.unwrap(), data);
    assert_partial(&fresh.await.unwrap(), &data, 100, 199);
    let (status, _, body) = not_modified.await.unwrap();
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.unwrap().is_empty());
    almond.wait_cached(&hash).await;
    assert_eq!(origin.gets(), 1);
    let ranges = origin.ranges.lock().unwrap().clone();
    assert!(
        matches!(ranges[0].as_deref(), None | Some("bytes=0-")),
        "a seek must not narrow the shared fill, got {ranges:?}"
    );
}

/// `If-Range` and `If-Match` need the strong comparison, `If-None-Match` the
/// weak one (RFC 9110 §13.1). Answers that assert the blob's identity wait for
/// verification, and the local copy answers exactly like the fill did.
#[tokio::test]
async fn validators_compare_strongly_or_weakly_as_http_requires() {
    let data = blob(256 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let strong = format!("\"{hash}\"");
    let weak = format!("W/\"{hash}\"");
    let origin = Origin::single(&data, Length::Actual, 64 * 1024, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;
    let if_range = |validator: &str| {
        vec![
            ("range", "bytes=0-99".to_owned()),
            ("if-range", validator.to_owned()),
        ]
    };

    let leader = almond.spawn_fetch(&uri, &[]);
    wait_gets(&origin, 1).await;
    let weak_if_range = almond.spawn_fetch(&uri, &if_range(&weak));
    let date_if_range = almond.spawn_fetch(&uri, &if_range("Wed, 21 Oct 2015 07:28:00 GMT"));
    let strong_if_range = almond.spawn_fetch(&uri, &if_range(&strong));
    let weak_if_match = almond.spawn_fetch(&uri, &[("if-match", weak.clone())]);
    let other_if_match =
        almond.spawn_fetch(&uri, &[("if-match", format!("\"{}\"", "0".repeat(64)))]);
    let strong_if_match = almond.spawn_fetch(&uri, &[("if-match", strong.clone())]);
    let weak_if_none_match = almond.spawn_fetch(&uri, &[("if-none-match", weak.clone())]);
    tokio::time::sleep(STILL_WAITING).await;
    assert!(
        !strong_if_match.is_finished(),
        "If-Match waits for verification"
    );
    assert!(
        !weak_if_none_match.is_finished(),
        "304 waits for verification"
    );

    origin.release_body();
    assert_eq!(leader.await.unwrap().2.unwrap(), data);
    for (name, reply) in [("weak", weak_if_range), ("date", date_if_range)] {
        let (status, _, body) = reply.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{name} If-Range ignores the range");
        assert_eq!(body.unwrap(), data);
    }
    assert_partial(&strong_if_range.await.unwrap(), &data, 0, 99);
    for reply in [weak_if_match, other_if_match] {
        assert_eq!(reply.await.unwrap().0, StatusCode::PRECONDITION_FAILED);
    }
    let (status, _, body) = strong_if_match.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    assert_eq!(
        weak_if_none_match.await.unwrap().0,
        StatusCode::NOT_MODIFIED
    );
    almond.wait_cached(&hash).await;

    // The cached copy must not drift from what the fill answered.
    let (status, _, body) = collect(almond.send(&uri, &if_range(&weak)).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    let local_strong = collect(almond.send(&uri, &if_range(&strong)).await).await;
    assert_partial(&local_strong, &data, 0, 99);
    let (status, _, _) = collect(almond.send(&uri, &[("if-none-match", weak.clone())]).await).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(origin.gets(), 1);
}

/// Bytes that do not hash to the requested name, or that stop short of the
/// declared length, must never complete as a successful full response and
/// must never enter the cache. Bytes already delivered cannot be taken back;
/// what matters is that the client sees the transfer fail rather than end.
#[tokio::test]
async fn corrupt_or_truncated_origin_bytes_fail_the_transfer_and_are_never_cached() {
    let data = blob(256 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let mut corrupt = data.to_vec();
    *corrupt.last_mut().unwrap() ^= 0xff;

    for (case, served) in [
        ("corrupt", Bytes::from(corrupt)),
        ("truncated", data.slice(..data.len() - 1000)),
    ] {
        let origin = Origin::start(
            vec![(hash.clone(), served)],
            Length::Claim(data.len()),
            usize::MAX,
            true,
        )
        .await;
        let almond = Almond::with_upstream(&origin, |_| {}).await;

        let (status, _, body) = collect(almond.send(&uri, &[]).await).await;
        assert!(
            body.is_err() || !status.is_success(),
            "{case}: an unverifiable transfer must not end cleanly ({status})"
        );

        // The failed fill leaves nothing behind, so the next request goes back
        // to the origin instead of being served from a bad cache entry.
        eventually(&format!("{case}: retry reaches the origin"), async || {
            let _ = collect(almond.send(&uri, &[]).await).await;
            origin.gets() >= 2
        })
        .await;
        assert!(
            !almond.state.file_index.contains(&hash).await,
            "{case}: a blob that failed verification must not be published"
        );
    }
}

/// An upstream error is a `502`, not a miss: it must not be remembered as
/// "absent", so the next request asks the origin again and succeeds once the
/// origin has recovered. A content-encoded body is such an error too: its
/// bytes are not the blob, and Almond asks for and accepts identity only.
#[tokio::test]
async fn upstream_errors_and_encoded_bodies_are_502s_not_remembered_as_misses() {
    let data = blob(64 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, usize::MAX, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    origin.fail_with(Some(StatusCode::INTERNAL_SERVER_ERROR));
    let (status, headers, _) = collect(almond.send(&uri, &[]).await).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(cache_control(&headers).contains("no-store"));
    assert_eq!(origin.gets(), 1);
    origin.fail_with(None);

    origin.encode_as(Some("gzip"));
    let (status, _, _) = collect(almond.send(&uri, &[]).await).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(origin.gets(), 2);
    assert!(!almond.state.file_index.contains(&hash).await);
    origin.encode_as(None);

    let (status, _, body) = collect(almond.send(&uri, &[]).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    assert_eq!(origin.gets(), 3, "the failures were not cached as misses");
    almond.wait_cached(&hash).await;
}

/// A fill is shared only between requests that asked for the same sources:
/// two different `?origin=` hosts must each be fetched, while a repeat of the
/// first origin joins the existing transfer.
#[tokio::test]
async fn different_custom_origins_do_not_share_a_fill() {
    let data = blob(512 * 1024);
    let hash = sha256_hex(&data);
    let first = Origin::single(&data, Length::Actual, 0, true).await;
    let second = Origin::single(&data, Length::Actual, 0, true).await;
    let almond = Almond::start(
        &[(ORIGIN_IP, &*first), (OTHER_ORIGIN_IP, &*second)],
        |cfg| cfg.custom_origin_access = FeatureMode::Public,
    )
    .await;
    let via_first = format!("/{hash}.mp4?origin={}", upstream_url(ORIGIN_IP, &first));
    let via_second = format!(
        "/{hash}.mp4?origin={}",
        upstream_url(OTHER_ORIGIN_IP, &second)
    );

    let a = almond.spawn_fetch(&via_first, &[]);
    wait_gets(&first, 1).await;
    let b = almond.spawn_fetch(&via_second, &[]);
    wait_gets(&second, 1).await;
    let a_again = almond.spawn_fetch(&via_first, &[]);
    tokio::time::sleep(STILL_WAITING).await;
    assert_eq!(first.gets(), 1, "the same origin is fetched once");
    assert_eq!(second.gets(), 1);

    first.release_body();
    second.release_body();
    for reply in [a, b, a_again] {
        let (status, _, body) = reply.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.unwrap(), data);
    }
    almond.wait_cached(&hash).await;
}

/// Above the cache limit the blob is only streamed: one upstream GET, never a
/// second ranged one, and nothing cached. Ignoring the client's `Range` and
/// answering 200 from that same response is allowed.
#[tokio::test]
async fn blobs_above_the_cache_limit_stream_from_one_get_and_are_not_cached() {
    let data = blob(256 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, usize::MAX, true).await;
    let almond = Almond::with_upstream(&origin, |cfg| {
        cfg.upstream_max_download_size = 64 * 1024;
    })
    .await;

    let reply = collect(almond.send(&uri, &range("bytes=1000-1999")).await).await;
    match reply.0 {
        StatusCode::PARTIAL_CONTENT => assert_partial(&reply, &data, 1000, 1999),
        StatusCode::OK => assert_eq!(reply.2.unwrap(), data),
        status => panic!("oversize blob must still stream, got {status}"),
    }
    assert_eq!(
        origin.gets(),
        1,
        "the client's range must not be re-requested"
    );
    assert!(!almond.state.file_index.contains(&hash).await);
}

/// The documented fill ceiling (`MAX_CONCURRENT_FILLS` in `services::download`).
const MAX_FILLS: usize = 8;
/// The documented reader ceiling (`MAX_ATTACHED_READERS` in `services::download`).
const MAX_READERS: usize = 256;

/// Distinct blobs beyond the fill ceiling are refused outright: the overloaded
/// request must not quietly open its own upstream transfer, and slots come
/// back once fills end.
#[tokio::test]
async fn a_fill_beyond_the_ceiling_is_refused_with_503_and_slots_return() {
    let blobs: Vec<Bytes> = (0..=MAX_FILLS)
        .map(|i| Bytes::from([&blob(64 * 1024)[..], &i.to_le_bytes()[..]].concat()))
        .collect();
    let origin = Origin::start(
        blobs
            .iter()
            .map(|data| (sha256_hex(data), data.clone()))
            .collect(),
        Length::Actual,
        0,
        true,
    )
    .await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;
    let uris: Vec<String> = blobs
        .iter()
        .map(|data| format!("/{}.bin", sha256_hex(data)))
        .collect();

    let mut held = Vec::new();
    for uri in &uris[..MAX_FILLS] {
        let response = almond.send(uri, &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    assert_eq!(origin.gets(), MAX_FILLS);

    let (status, _, _) = collect(almond.send(&uris[MAX_FILLS], &[]).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        origin.gets(),
        MAX_FILLS,
        "an overloaded request must not open its own transfer"
    );

    origin.release_body();
    for (response, data) in held.into_iter().zip(&blobs) {
        let (_, _, body) = collect(response).await;
        assert_eq!(&body.unwrap(), data);
    }
    let last = &blobs[MAX_FILLS];
    eventually("a free fill slot", async || {
        let (status, _, body) = collect(almond.send(&uris[MAX_FILLS], &[]).await).await;
        status == StatusCode::OK && body.is_ok_and(|body| body == *last)
    })
    .await;
}

/// Response bodies attached to fills are bounded too. Clients that hang up
/// return their slots, and their leaving does not stop the fill.
#[tokio::test]
async fn reader_slots_are_bounded_and_returned_when_clients_leave() {
    let data = blob(64 * 1024);
    let hash = sha256_hex(&data);
    let uri = format!("/{hash}.mp4");
    let origin = Origin::single(&data, Length::Actual, 0, true).await;
    let almond = Almond::with_upstream(&origin, |_| {}).await;

    let mut held = Vec::with_capacity(MAX_READERS);
    for _ in 0..MAX_READERS {
        let response = almond.send(&uri, &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    let (status, _, _) = collect(almond.send(&uri, &[]).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        origin.gets(),
        1,
        "readers over the ceiling must not go upstream"
    );

    drop(held);
    let late = almond.spawn_fetch(&uri, &[]);
    origin.release_body();
    let (status, _, body) = late.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), data);
    almond.wait_cached(&hash).await;
    assert_eq!(origin.gets(), 1);
}

/// Disk is reserved for the declared size of every in-flight fill together:
/// two blobs that each fit the quota alone cannot be filled at the same time,
/// and the refused one gets 507 rather than being written past the quota.
#[tokio::test]
async fn concurrent_fills_share_one_disk_reservation_and_overflow_gets_507() {
    let first = blob(256 * 1024);
    let second = Bytes::from([&first[..], &b"second"[..]].concat());
    let origin = Origin::start(
        vec![
            (sha256_hex(&first), first.clone()),
            (sha256_hex(&second), second.clone()),
        ],
        Length::Actual,
        0,
        true,
    )
    .await;
    let almond = Almond::with_upstream(&origin, |cfg| {
        cfg.storage_max_size = 384 * 1024;
    })
    .await;

    let filling = almond.spawn_fetch(&format!("/{}.bin", sha256_hex(&first)), &[]);
    wait_gets(&origin, 1).await;
    let (status, _, _) = collect(
        almond
            .send(&format!("/{}.bin", sha256_hex(&second)), &[])
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(origin.gets() <= 2, "a refused fill must not retry upstream");

    origin.release_body();
    let (status, _, body) = filling.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), first);
    almond.wait_cached(&sha256_hex(&first)).await;
    assert!(!almond.state.file_index.contains(&sha256_hex(&second)).await);
}
