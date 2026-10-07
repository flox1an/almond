use almond::{models::FeatureMode, Config, ConfigError};

// This integration-test binary owns its process-level crypto provider.
#[tokio::test]
async fn startup_rejects_invalid_settings_before_touching_storage() {
    let root =
        std::env::temp_dir().join(format!("almond-library-startup-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::defaults();
    cfg.storage_path = root.clone();
    cfg.s3_endpoint = Some("https://s3.example.invalid".to_owned());
    let error = almond::build_state(&cfg)
        .await
        .err()
        .expect("incomplete S3 must fail");
    assert!(error.is::<ConfigError>());
    assert!(error.to_string().contains("Incomplete S3"));
    assert!(!root.exists(), "invalid settings must not create storage");

    cfg.s3_endpoint = None;
    cfg.cleanup_interval = std::time::Duration::ZERO;
    let error = almond::build_state(&cfg)
        .await
        .err()
        .expect("zero cleanup interval must fail");
    assert!(error.is::<ConfigError>());
    assert!(!root.exists());
    cfg.cleanup_interval = std::time::Duration::from_secs(30);

    #[cfg(not(feature = "cashu"))]
    {
        cfg.cashu_paid = vec![almond::services::cashu::PaidOperation::Upload];
        cfg.cashu_mint = Some("https://mint.example.invalid".to_owned());
        let error = almond::build_state(&cfg)
            .await
            .err()
            .expect("paid operations need Cashu");
        assert!(error.to_string().contains("without the cashu feature"));
        assert!(!root.exists());
        cfg.cashu_paid.clear();
        cfg.cashu_mint = None;
    }

    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    cfg.upload_access = FeatureMode::Wot;
    let error = almond::build_state(&cfg)
        .await
        .err()
        .expect("relay jobs need a crypto provider");
    assert!(error.to_string().contains("CryptoProvider"));
    assert!(!root.exists());
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "Almond must not select the host's provider"
    );

    cfg.upload_access = FeatureMode::Dvm;
    cfg.dvm_kinds = vec![5207];
    let error = almond::build_state(&cfg)
        .await
        .err()
        .expect("DVM jobs need a crypto provider");
    assert!(error.to_string().contains("CryptoProvider"));
    assert!(!root.exists());

    cfg.upload_access = FeatureMode::Public;
    cfg.dvm_kinds.clear();
    cfg.custom_origin_access = FeatureMode::Public;
    let error = almond::build_state(&cfg)
        .await
        .err()
        .expect("author-based upstream discovery needs a crypto provider");
    assert!(error.to_string().contains("CryptoProvider"));
    assert!(!root.exists());
    cfg.custom_origin_access = FeatureMode::Off;
    cfg.metrics_token = Some("  ".to_owned());
    let state = almond::build_state(&cfg)
        .await
        .expect("local mode needs no global provider");
    assert!(
        state.metrics_bearer_token.is_none(),
        "a blank token must not enable metrics"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    drop(state);

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("test owns its provider");
    let provider = rustls::crypto::CryptoProvider::get_default()
        .unwrap()
        .clone();
    cfg.upload_access = FeatureMode::Wot;
    let state = almond::build_state(&cfg)
        .await
        .expect("host-selected ring provider must work");
    assert!(std::sync::Arc::ptr_eq(
        rustls::crypto::CryptoProvider::get_default().unwrap(),
        &provider,
    ));
    drop(state);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
