//! `native_storage` against an in-process, path-style fake S3 on 127.0.0.1.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use almond::services::native_storage::{probe, NativeS3Storage, ProbeStep, S3Settings};
use axum::{
    body::{Body, Bytes},
    extract::{Query, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::Response,
    Router,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

const BUCKET: &str = "almond-test";
const ACCESS_KEY: &str = "AKIATEST";

#[derive(Default)]
struct FakeS3 {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    requests: Mutex<Vec<String>>,
}

async fn spawn_fake(fake: Arc<FakeS3>) -> String {
    let app = Router::new().fallback(handle).with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn xml_error(status: StatusCode, code: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{code}</Message></Error>"
        )))
        .unwrap()
}

fn parse_range(value: &str, len: usize) -> Option<(usize, usize)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end): (usize, usize) = (start.parse().ok()?, end.parse().ok()?);
    (start <= end && end < len).then_some((start, end))
}

/// Strip `aws-chunked` framing (`<hex>;chunk-signature=…\r\n<data>\r\n`, ended
/// by a zero-size chunk and optional trailers) the way S3 does.
fn decode_body(headers: &HeaderMap, body: &[u8]) -> Vec<u8> {
    let chunked = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("aws-chunked"));
    if !chunked {
        return body.to_vec();
    }
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_field = std::str::from_utf8(&rest[..line_end]).unwrap_or("");
        let size = usize::from_str_radix(size_field.split(';').next().unwrap_or(""), 16)
            .expect("chunk size is hex");
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    out
}

async fn handle(
    State(fake): State<Arc<FakeS3>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Response {
    fake.requests.lock().push(format!("{method} {uri}"));

    let signed_with_expected_key = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains(&format!("Credential={ACCESS_KEY}/")));
    if !signed_with_expected_key {
        return xml_error(StatusCode::FORBIDDEN, "InvalidAccessKeyId");
    }

    let path = uri.path().trim_start_matches('/');
    let (bucket, key) = path.split_once('/').unwrap_or((path, ""));
    if bucket != BUCKET {
        return xml_error(StatusCode::NOT_FOUND, "NoSuchBucket");
    }

    let mut objects = fake.objects.lock();
    if method == Method::GET && key.is_empty() && query.contains_key("list-type") {
        let prefix = query.get("prefix").map_or("", String::as_str);
        let matching: Vec<_> = objects
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .collect();
        let contents: String = matching
            .iter()
            .map(|(k, v)| {
                format!(
                    "<Contents><Key>{k}</Key><LastModified>2026-10-16T00:00:00.000Z</LastModified>\
                     <ETag>\"etag\"</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                    v.len()
                )
            })
            .collect();
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>{BUCKET}</Name>\
             <Prefix>{prefix}</Prefix><KeyCount>{}</KeyCount><MaxKeys>1000</MaxKeys>\
             <IsTruncated>false</IsTruncated>{contents}</ListBucketResult>",
            matching.len()
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    if method == Method::PUT && !key.is_empty() {
        objects.insert(key.to_owned(), decode_body(&headers, &body));
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::ETAG, "\"etag\"")
            .body(Body::empty())
            .unwrap();
    }

    if method == Method::GET && !key.is_empty() {
        let Some(data) = objects.get(key) else {
            return xml_error(StatusCode::NOT_FOUND, "NoSuchKey");
        };
        let range = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_range(value, data.len()));
        return match range {
            None => Response::new(Body::from(data.clone())),
            Some((start, end)) => Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", data.len()),
                )
                .body(Body::from(data[start..=end].to_vec()))
                .unwrap(),
        };
    }

    if method == Method::DELETE && !key.is_empty() {
        objects.remove(key);
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap();
    }

    xml_error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed")
}

fn settings(endpoint: &str, bucket: &str, access_key: &str) -> S3Settings {
    S3Settings {
        endpoint: endpoint.to_owned(),
        bucket: bucket.to_owned(),
        access_key_id: access_key.to_owned(),
        secret_access_key: "test-secret".to_owned(),
        region: "us-east-1".to_owned(),
        force_path_style: true,
    }
}

#[tokio::test]
async fn probe_passes_and_never_touches_blob_keys() {
    let fake = Arc::new(FakeS3::default());
    let blob_key = format!("a/a/{}.txt", "aa".repeat(32));
    fake.objects
        .lock()
        .insert(blob_key.clone(), b"blob".to_vec());
    let endpoint = spawn_fake(fake.clone()).await;

    probe(&settings(&endpoint, BUCKET, ACCESS_KEY))
        .await
        .expect("probe must pass against a working bucket");

    assert_eq!(
        fake.objects.lock().keys().collect::<Vec<_>>(),
        vec![&blob_key],
        "probe must leave only the pre-existing blob behind"
    );
    let requests = fake.requests.lock();
    assert!(
        requests
            .iter()
            .any(|r| r.starts_with("PUT") && r.contains("almond-probe")),
        "probe must write its own object: {requests:?}"
    );
    assert!(
        requests.iter().all(|r| !r.contains(&blob_key)),
        "probe touched a blob key: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .filter(|r| r.starts_with("GET") && r.contains("list-type"))
            .all(|r| r.contains("almond-probe")),
        "probe listed outside its own prefix: {requests:?}"
    );
}

#[tokio::test]
async fn probe_reports_wrong_bucket() {
    let endpoint = spawn_fake(Arc::new(FakeS3::default())).await;

    let failure = probe(&settings(&endpoint, "missing-bucket", ACCESS_KEY))
        .await
        .expect_err("probe must fail for an unknown bucket");

    assert_eq!(failure.step, ProbeStep::Put);
    assert_eq!(failure.code.as_deref(), Some("NoSuchBucket"));
}

#[tokio::test]
async fn probe_reports_wrong_credentials() {
    let endpoint = spawn_fake(Arc::new(FakeS3::default())).await;

    let failure = probe(&settings(&endpoint, BUCKET, "AKIAWRONG"))
        .await
        .expect_err("probe must fail for an unknown access key");

    assert_eq!(failure.step, ProbeStep::Put);
    assert_eq!(failure.code.as_deref(), Some("InvalidAccessKeyId"));
}

#[tokio::test]
async fn upload_range_and_delete_use_path_style() {
    let fake = Arc::new(FakeS3::default());
    let endpoint = spawn_fake(fake.clone()).await;
    let storage = NativeS3Storage::connect(settings(&endpoint, BUCKET, ACCESS_KEY)).await;

    let content = b"hello world";
    let hash = hex::encode(Sha256::digest(content));
    let temp = std::env::temp_dir().join(format!("almond-s3-test-{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&temp, content).await.unwrap();

    let key = storage.put(&temp, &hash, Some("txt"), None).await.unwrap();
    assert!(!temp.exists(), "put consumes the temporary file");

    let metadata = storage
        .find(&hash)
        .await
        .unwrap()
        .expect("uploaded blob must be found");
    assert_eq!(metadata.size, content.len() as u64);
    assert_eq!(metadata.mime_type.as_deref(), Some("text/plain"));

    let ranged = storage.get(&key, Some("bytes=0-4")).await.unwrap();
    assert_eq!(ranged.content_range(), Some("bytes 0-4/11"));
    let bytes = ranged.body.collect().await.unwrap().into_bytes();
    assert_eq!(&bytes[..], &b"hello"[..]);

    assert_eq!(storage.read_text(&key).await.unwrap(), "hello world");

    storage.delete_matching(&hash).await.unwrap();
    assert!(storage.find(&hash).await.unwrap().is_none());
    assert!(fake.objects.lock().is_empty());
}
