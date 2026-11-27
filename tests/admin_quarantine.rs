use almond::{
    models::{BlobOrigin, FileLocation, FileMetadata},
    services::file_storage::{self, Removal},
};

#[tokio::test]
async fn quarantine_restore_recovers_after_interrupted_move_and_respects_block() {
    let root =
        std::env::temp_dir().join(format!("almond-quarantine-test-{}", uuid::Uuid::new_v4()));
    let mut cfg = almond::Config::defaults();
    cfg.storage_path = root.clone();
    let state = almond::build_state(&cfg).await.unwrap();
    let hash = "a".repeat(64);
    let path = state.storage.uploads.join(&hash);
    tokio::fs::create_dir_all(&state.storage.uploads)
        .await
        .unwrap();
    tokio::fs::write(&path, b"content").await.unwrap();
    state
        .file_index
        .insert(
            hash.clone(),
            FileMetadata {
                location: FileLocation::Local(path.clone()),
                extension: None,
                mime_type: None,
                size: 7,
                created_at: 1,
                pubkey: None,
                expiration: None,
                origin: BlobOrigin::Upload,
            },
        )
        .await;
    assert!(
        file_storage::remove_indexed_blob(&state, &hash, Removal::Quarantined, None)
            .await
            .unwrap()
    );
    assert!(file_storage::ensure_not_quarantined(&state, &hash)
        .await
        .is_err());
    let blocks = root.join("admin-blocks");
    tokio::fs::create_dir_all(&blocks).await.unwrap();
    tokio::fs::write(blocks.join(&hash), b"{}").await.unwrap();
    assert!(file_storage::restore_quarantined(&state, &hash)
        .await
        .is_err());
    tokio::fs::remove_file(blocks.join(&hash)).await.unwrap();
    // Simulate a crash after moving the bytes but before removing the journal.
    tokio::fs::rename(state.storage.quarantine.join(&hash), &path)
        .await
        .unwrap();
    file_storage::restore_quarantined(&state, &hash)
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"content");
    assert!(state.file_index.get(&hash).await.is_some());
    assert!(file_storage::ensure_not_quarantined(&state, &hash)
        .await
        .is_ok());
    tokio::fs::remove_dir_all(root).await.unwrap();
}
