use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BASE: &str = "http://127.0.0.1:3000";

fn authorization(
    keys: &Keys,
    url: &str,
    method: &str,
    body: &str,
    challenge: Option<&str>,
) -> String {
    let mut tags = vec![Tag::custom("u", [url]), Tag::custom("method", [method])];
    if !body.is_empty() {
        tags.push(Tag::custom(
            "payload",
            [hex::encode(Sha256::digest(body.as_bytes()))],
        ));
    }
    if let Some(challenge) = challenge {
        tags.push(Tag::custom("challenge", [challenge]));
    }
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .tags(tags)
        .finalize(keys)
        .unwrap();
    format!(
        "Nostr {}",
        STANDARD.encode(serde_json::to_vec(&event).unwrap())
    )
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    auth: Option<&str>,
) -> Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(auth) = auth {
        builder = builder.header("authorization", auth);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn signed_admin_requests_enforce_authentication_and_revocation() {
    let root = std::env::temp_dir().join(format!("almond-admin-auth-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let admin = Keys::generate();
    let collaborator = Keys::generate();
    let stranger = Keys::generate();
    let mut cfg = almond::Config::defaults();
    cfg.storage_path = root.join("storage");
    cfg.managed_config = Some(root.join("managed.json"));
    cfg.admin_npubs = vec![admin.public_key()];
    let app = almond::create_app(almond::build_state(&cfg).await.unwrap());

    let challenge = json_body(request(&app, "GET", "/admin/api/challenge", "", None).await).await
        ["challenge"]
        .as_str()
        .unwrap()
        .to_owned();
    let me = "/admin/api/me";
    let me_url = format!("{BASE}{me}");
    assert_eq!(
        request(&app, "GET", me, "", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let auth = authorization(&admin, &me_url, "GET", "", None);
    let response = request(&app, "GET", me, "", Some(&auth)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["pubkey"],
        admin.public_key().to_string()
    );
    let auth = authorization(&stranger, &me_url, "GET", "", None);
    assert_eq!(
        request(&app, "GET", me, "", Some(&auth)).await.status(),
        StatusCode::FORBIDDEN
    );
    let auth = authorization(&admin, &format!("{me_url}?wrong=1"), "GET", "", None);
    assert_eq!(
        request(&app, "GET", me, "", Some(&auth)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let auth = authorization(&admin, &me_url, "POST", "", None);
    assert_eq!(
        request(&app, "GET", me, "", Some(&auth)).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let path = "/admin/api/collaborators";
    let url = format!("{BASE}{path}");
    let auth = authorization(&admin, &url, "GET", "", None);
    let revision = json_body(request(&app, "GET", path, "", Some(&auth)).await).await["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = json!({"base_revision": revision, "collaborators": [collaborator.public_key().to_string()]}).to_string();
    let auth = authorization(&admin, &url, "PATCH", "different body", Some(&challenge));
    assert_eq!(
        request(&app, "PATCH", path, &body, Some(&auth))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let auth = authorization(&admin, &url, "PATCH", &body, None);
    assert_eq!(
        request(&app, "PATCH", path, &body, Some(&auth))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let auth = authorization(&admin, &url, "PATCH", &body, Some(&challenge));
    let response = request(&app, "PATCH", path, &body, Some(&auth)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let revision = json_body(response).await["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        request(&app, "PATCH", path, &body, Some(&auth))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let collab_auth = authorization(&collaborator, &me_url, "GET", "", None);
    assert_eq!(
        request(&app, "GET", me, "", Some(&collab_auth))
            .await
            .status(),
        StatusCode::OK
    );

    let body = json!({"base_revision": revision, "collaborators": []}).to_string();
    let revoke = authorization(&admin, &url, "PATCH", &body, Some(&challenge));
    assert_eq!(
        request(&app, "PATCH", path, &body, Some(&revoke))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", me, "", Some(&collab_auth))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let restarted = almond::create_app(almond::build_state(&cfg).await.unwrap());
    let new_challenge =
        json_body(request(&restarted, "GET", "/admin/api/challenge", "", None).await).await
            ["challenge"]
            .as_str()
            .unwrap()
            .to_owned();
    assert_ne!(challenge, new_challenge);
    assert_eq!(
        request(&restarted, "PATCH", path, &body, Some(&revoke))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&restarted, "GET", me, "", Some(&collab_auth))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    drop(restarted);
    drop(app);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn interrupted_activity_requires_explicit_reconciliation() {
    let root = std::env::temp_dir().join(format!("almond-admin-recovery-{}", uuid::Uuid::new_v4()));
    let admin = Keys::generate();
    let mut cfg = almond::Config::defaults();
    cfg.storage_path = root.join("storage");
    cfg.admin_npubs = vec![admin.public_key()];
    let app = almond::create_app(almond::build_state(&cfg).await.unwrap());
    let dir = cfg.storage_path.join("admin-activity");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    tokio::fs::write(
        dir.join(format!("{id}.intent.json")),
        serde_json::to_vec(
            &json!({"id":id,"status":"pending","hash":"a".repeat(64),"action":"delete","time":1}),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let path = "/admin/api/activity";
    let auth = authorization(&admin, &format!("{BASE}{path}"), "GET", "", None);
    let list = json_body(request(&app, "GET", path, "", Some(&auth)).await).await;
    assert_eq!(list["items"][0]["outcome"]["status"], "uncertain");
    let challenge = json_body(request(&app, "GET", "/admin/api/challenge", "", None).await).await
        ["challenge"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/admin/api/activity/{id}/reconcile");
    let body = r#"{"reason":"Inspected storage; outcome remains unknown"}"#;
    let auth = authorization(
        &admin,
        &format!("{BASE}{path}"),
        "POST",
        body,
        Some(&challenge),
    );
    let response = request(&app, "POST", &path, body, Some(&auth)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["status"], "reconciled");
    drop(app);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
