use almond::Config;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

#[tokio::test]
async fn favicon_shares_homepage_visibility() {
    let root = std::env::temp_dir().join(format!("almond-branding-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::defaults();
    cfg.storage_path = root.clone();
    let mut state = almond::build_state(&cfg).await.unwrap();

    for enabled in [true, false] {
        state.feature_homepage_enabled = enabled;
        let app = almond::create_app(state.clone()).merge(almond::standalone_extras(state.clone()));
        for path in ["/", "/index.html", "/favicon.ico", "/config"] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if enabled { 200 } else { 404 },
                "{path}, homepage enabled: {enabled}"
            );
        }
    }

    tokio::fs::remove_dir_all(root).await.unwrap();
}
