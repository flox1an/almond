use std::time::Duration;

use almond::{services::native_storage::S3Status, Config};
use axum::{
    body::Body,
    extract::Query,
    http::{header, Request, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use tower::ServiceExt;

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const OTHER_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY: &str = "2/c/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824.txt";

/// Minimal S3 stand-in: ListObjectsV2 over one object, and GET of that object.
async fn fake_s3(Query(query): Query<HashMap<String, String>>) -> impl IntoResponse {
    if !query.contains_key("list-type") {
        return (StatusCode::OK, "hello".to_owned());
    }
    let listed = query
        .get("prefix")
        .is_none_or(|prefix| KEY.starts_with(prefix));
    let contents = if listed {
        format!(
            "<Contents><Key>{KEY}</Key><Size>5</Size>\
             <LastModified>2024-01-01T00:00:00.000Z</LastModified></Contents>"
        )
    } else {
        String::new()
    };
    (
        StatusCode::OK,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>\
             <Name>b</Name><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>"
        ),
    )
}

fn upload_request(keys: &Keys) -> Request<Body> {
    let event: Event = EventBuilder::new(Kind::Custom(24242), "upload")
        .tags([
            Tag::custom("t", ["upload"]),
            Tag::custom("x", [HELLO_SHA256]),
            Tag::expiration(Timestamp::from_secs(Timestamp::now().as_secs() + 600)),
        ])
        .finalize(keys)
        .unwrap();
    Request::put("/upload")
        .header(
            header::AUTHORIZATION,
            format!(
                "Nostr {}",
                STANDARD.encode(serde_json::to_string(&event).unwrap())
            ),
        )
        .header("X-SHA-256", HELLO_SHA256)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("hello"))
        .unwrap()
}

async fn status(app: &Router, request: Request<Body>) -> StatusCode {
    app.clone().oneshot(request).await.unwrap().status()
}

fn get(hash: &str) -> Request<Body> {
    Request::get(format!("/{hash}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn unreachable_bucket_degrades_startup_then_recovers() {
    // Reserve a port, then close it: the endpoint is unreachable until we rebind.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let root = std::env::temp_dir().join(format!("almond-s3-degraded-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::defaults();
    cfg.storage_path = root.clone();
    cfg.s3_endpoint = Some(format!("http://{addr}"));
    cfg.storage_min_free = 0; // keep the check independent of the host disk
    cfg.s3_bucket = Some("b".to_owned());
    cfg.s3_access_key_id = Some("key".to_owned());
    cfg.s3_secret_access_key = Some("secret".to_owned());

    let state = almond::build_state(&cfg)
        .await
        .expect("unreachable S3 must not fail startup");
    let Some(S3Status::Unavailable {
        error,
        last_attempt,
    }) = state.s3_status()
    else {
        panic!("S3 should be unavailable");
    };
    assert!(!error.is_empty() && last_attempt > 0);
    let _tasks = almond::spawn_background_tasks(&state, &cfg);
    let app = almond::create_app(state.clone());

    // Unknown hash: 503, not 404. Uploads: 503, nothing lands on local disk.
    assert_eq!(
        status(&app, get(OTHER_SHA256)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let keys = Keys::generate();
    assert_eq!(
        status(&app, upload_request(&keys)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(state.file_index.get(HELLO_SHA256).await.is_none());
    let mut uploads = std::fs::read_dir(root.join("uploads")).unwrap();
    assert!(uploads.next().is_none(), "no blob may land on local disk");

    // The endpoint comes back: the background job indexes the bucket.
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(fake_s3)))
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while state.s3_status() != Some(S3Status::Available) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("S3 should recover");

    assert_eq!(status(&app, get(HELLO_SHA256)).await, StatusCode::OK);
    assert_eq!(status(&app, get(OTHER_SHA256)).await, StatusCode::NOT_FOUND);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
