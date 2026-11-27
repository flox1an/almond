mod common;

use almond::models::FeatureMode;
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use nostr_sdk::prelude::Keys;

#[tokio::test]
async fn public_report_does_not_delete_and_authorized_delete_cannot_be_replayed() {
    let owner = Keys::generate();
    let reporter = Keys::generate();
    let h = common::harness(|cfg| {
        cfg.allowed_npubs = vec![owner.public_key()];
        cfg.report_access = FeatureMode::Public;
    })
    .await;
    let bytes = b"hello";
    let hash = common::sha256_hex(bytes);
    let upload = Request::put("/upload")
        .header(
            header::AUTHORIZATION,
            common::blossom_auth(&owner, "upload", Some(&hash)),
        )
        .header("X-SHA-256", &hash)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(bytes.as_slice()))
        .unwrap();
    assert_eq!(common::status_of(&h.app, upload).await, StatusCode::CREATED);

    let report = common::report_event(&reporter, &[&hash], "spam");
    let response = common::send(&h.app, common::put_json("/report", &report)).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(common::json_of(response).await["action"], "none");
    assert_eq!(
        common::status_of(&h.app, common::get(&format!("/{hash}"))).await,
        StatusCode::OK
    );
    assert_eq!(
        common::status_of(&h.app, common::put_json("/report", &report)).await,
        StatusCode::CONFLICT
    );

    let wrong_key = common::blossom_auth(&reporter, "delete", Some(&hash));
    assert_eq!(
        common::status_of(&h.app, common::delete(&hash, Some(&wrong_key))).await,
        StatusCode::UNAUTHORIZED
    );
    let auth = common::blossom_auth(&owner, "delete", Some(&hash));
    assert_eq!(
        common::status_of(&h.app, common::delete(&hash, Some(&auth))).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        common::status_of(&h.app, common::delete(&hash, Some(&auth))).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        common::status_of(&h.app, common::get(&format!("/{hash}"))).await,
        StatusCode::NOT_FOUND
    );
}
