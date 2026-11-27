//! Shared helpers: a real app over a temp storage root, and signed requests.

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::Response,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr_sdk::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use almond::Config;

pub(crate) struct Harness {
    pub(crate) app: Router,
    root: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.root));
    }
}

/// Build a real app on a fresh temp directory; `configure` adjusts the settings.
pub(crate) async fn harness(configure: impl FnOnce(&mut Config)) -> Harness {
    let root = std::env::temp_dir().join(format!("almond-it-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::defaults();
    cfg.storage_path = root.clone();
    configure(&mut cfg);
    let state = almond::build_state(&cfg).await.unwrap();
    Harness {
        app: almond::create_app(state),
        root,
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn header_value(event: &Event) -> String {
    format!(
        "Nostr {}",
        STANDARD.encode(serde_json::to_string(event).unwrap())
    )
}

/// A BUD-11 authorization event (kind 24242) for `verb`, valid for ten minutes.
pub(crate) fn blossom_auth(keys: &Keys, verb: &str, hash: Option<&str>) -> String {
    let mut tags = vec![
        Tag::custom("t", [verb]),
        Tag::expiration(Timestamp::from_secs(Timestamp::now().as_secs() + 600)),
    ];
    if let Some(hash) = hash {
        tags.push(Tag::custom("x", [hash]));
    }
    let event = EventBuilder::new(Kind::Custom(24242), verb)
        .tags(tags)
        .finalize(keys)
        .unwrap();
    header_value(&event)
}

/// A NIP-56 report (kind 1984) naming `hashes` with `reason`.
pub(crate) fn report_event(keys: &Keys, hashes: &[&str], reason: &str) -> Value {
    let tags: Vec<Tag> = hashes
        .iter()
        .map(|hash| Tag::custom("x", [*hash, reason]))
        .collect();
    let event = EventBuilder::new(Kind::Custom(1984), "report")
        .tags(tags)
        .finalize(keys)
        .unwrap();
    serde_json::to_value(event).unwrap()
}

pub(crate) async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

pub(crate) async fn status_of(app: &Router, request: Request<Body>) -> StatusCode {
    send(app, request).await.status()
}

pub(crate) async fn json_of(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

pub(crate) fn put_json(uri: &str, body: &Value) -> Request<Body> {
    Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(crate) fn delete(hash: &str, authorization: Option<&str>) -> Request<Body> {
    let mut request = Request::delete(format!("/{hash}"));
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    request.body(Body::empty()).unwrap()
}

pub(crate) fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}
