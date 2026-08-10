use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use hashtree_core::{store::StoreStats, Hash, Store, StoreError};
use hashtree_network::{
    MeshRouter, MeshStoreCore, NostrRelayTransport, PoolSettings, SignalingTransport,
};
use nostr_sdk::prelude::Keys;
use tokio::fs;
use tracing::{debug, error, info, warn};

use crate::{
    models::AppState,
    services::{blob_index::BlobIndex, p2p_webrtc::RealPeerConnectionFactory},
};

#[derive(Clone)]
struct AlmondLocalBlobStore {
    file_index: Arc<BlobIndex>,
}

impl AlmondLocalBlobStore {
    fn new(state: &AppState) -> Self {
        Self {
            file_index: state.file_index.clone(),
        }
    }

    async fn local_path_for_hash(&self, hash: &Hash) -> Option<PathBuf> {
        let hash_hex = hex::encode(hash);
        self.file_index
            .get(&hash_hex)
            .await
            .and_then(|metadata| {
                // Only the filesystem adapter can hand out a path to export;
                // a natively stored blob is simply not P2P-servable.
                crate::services::file_storage::local_path(&metadata)
                    .map(std::path::Path::to_path_buf)
            })
    }
}

#[async_trait]
impl Store for AlmondLocalBlobStore {
    async fn put(&self, _hash: Hash, _data: Vec<u8>) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(path) = self.local_path_for_hash(hash).await else {
            return Ok(None);
        };

        let data = fs::read(path).await?;
        Ok(Some(data))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.local_path_for_hash(hash).await.is_some())
    }

    async fn delete(&self, _hash: &Hash) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn stats(&self) -> StoreStats {
        let index = self.file_index.stats().await;
        StoreStats {
            count: index.count as u64,
            bytes: index.total_bytes,
            pinned_count: 0,
            pinned_bytes: 0,
        }
    }
}

pub fn start_p2p_serve_job(state: AppState) {
    if !state.feature_p2p_serve_enabled {
        info!("⚠️ Hashtree P2P serving disabled");
        return;
    }

    tokio::spawn(async move {
        if let Err(err) = run_p2p_serve(state).await {
            error!("Hashtree P2P serving stopped: {err}");
        }
    });
}

async fn run_p2p_serve(state: AppState) -> Result<(), String> {
    let keys = if let Some(secret) = &state.p2p_nsec {
        Keys::parse(secret).map_err(|err| format!("invalid P2P_NSEC: {err}"))?
    } else {
        warn!("FEATURE_P2P_SERVE_ENABLED is on but P2P_NSEC is not set; P2P serving disabled");
        return Ok(());
    };

    let relays = if state.p2p_relays.is_empty() {
        vec![
            "wss://relay.primal.net".to_string(),
            "wss://relay.nostr.band".to_string(),
            "wss://temp.iris.to".to_string(),
            "wss://relay.snort.social".to_string(),
        ]
    } else {
        state.p2p_relays.clone()
    };

    let stun_servers = if state.p2p_stun_servers.is_empty() {
        vec![
            "stun:stun.iris.to:3478".to_string(),
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun.cloudflare.com:3478".to_string(),
        ]
    } else {
        state.p2p_stun_servers.clone()
    };

    let pubkey = keys.public_key().to_hex();
    let peer_id = pubkey.clone();

    let transport = Arc::new(NostrRelayTransport::new(keys, state.p2p_debug));
    transport
        .connect(&relays)
        .await
        .map_err(|err| format!("failed to connect P2P signaling transport: {err}"))?;

    let factory = Arc::new(RealPeerConnectionFactory::with_stun_servers(stun_servers));
    let router = Arc::new(MeshRouter::new(
        peer_id.clone(),
        transport.clone(),
        factory,
        PoolSettings::default(),
        state.p2p_debug,
    ));
    let local_store = Arc::new(AlmondLocalBlobStore::new(&state));
    let store = Arc::new(MeshStoreCore::new(
        local_store,
        router,
        Duration::from_millis(state.p2p_request_timeout_ms),
        state.p2p_debug,
    ));

    store
        .start()
        .await
        .map_err(|err| format!("failed to start P2P store: {err}"))?;

    info!(
        "✅ Hashtree P2P serving enabled as {} on relays {:?}",
        peer_id, relays
    );

    let signaling_store = store.clone();
    let signaling_transport = transport.clone();
    tokio::spawn(async move {
        while let Some(msg) = signaling_transport.recv().await {
            if let Err(err) = signaling_store.process_signaling(msg).await {
                warn!("Failed to process Hashtree P2P signaling message: {err}");
            }
        }
    });

    let pump_store = store.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(25));
        loop {
            interval.tick().await;
            let mut processed = 0usize;
            for peer_id in pump_store.signaling().peer_ids().await {
                let Some(channel) = pump_store.signaling().get_channel(&peer_id).await else {
                    continue;
                };

                while let Some(data) = channel.try_recv() {
                    processed += 1;
                    pump_store.handle_data_message(&peer_id, &data).await;
                }
            }

            if processed > 0 {
                debug!("Processed {processed} P2P data messages");
            }
        }
    });

    let hello_store = store.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_millis(state.p2p_hello_interval_ms));
        loop {
            interval.tick().await;
            if let Err(err) = hello_store.signaling().send_hello(vec![]).await {
                warn!("Failed to publish Hashtree P2P hello: {err}");
            }
        }
    });

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}
