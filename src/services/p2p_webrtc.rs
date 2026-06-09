use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use hashtree_network::{PeerLink, PeerLinkFactory, TransportError};
use tokio::sync::{mpsc, Mutex, RwLock};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    data_channel::{
        data_channel_init::RTCDataChannelInit, data_channel_message::DataChannelMessage,
        RTCDataChannel,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};

const DATA_CHANNEL_LABEL: &str = "hashtree";

pub struct RealDataChannel {
    dc: Arc<RTCDataChannel>,
    msg_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl RealDataChannel {
    fn new(dc: Arc<RTCDataChannel>) -> Arc<Self> {
        let (msg_tx, msg_rx) = mpsc::channel(100);

        let tx = msg_tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            let data = msg.data.to_vec();
            Box::pin(async move {
                let _ = tx.send(data).await;
            })
        }));

        Arc::new(Self {
            dc,
            msg_rx: Mutex::new(msg_rx),
        })
    }
}

#[async_trait]
impl PeerLink for RealDataChannel {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        self.dc
            .send(&bytes::Bytes::from(data))
            .await
            .map(|_| ())
            .map_err(|err| TransportError::SendFailed(err.to_string()))
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        self.msg_rx.lock().await.recv().await
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        let Ok(mut rx) = self.msg_rx.try_lock() else {
            return None;
        };
        rx.try_recv().ok()
    }

    fn is_open(&self) -> bool {
        self.dc.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
    }

    async fn close(&self) {
        let _ = self.dc.close().await;
    }
}

struct PendingConnection {
    connection: Arc<RTCPeerConnection>,
    data_channel: Option<Arc<RTCDataChannel>>,
}

pub struct RealPeerConnectionFactory {
    pending: RwLock<HashMap<String, PendingConnection>>,
    inbound: RwLock<HashMap<String, PendingConnection>>,
    stun_servers: Vec<String>,
}

impl RealPeerConnectionFactory {
    pub fn with_stun_servers(stun_servers: Vec<String>) -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            inbound: RwLock::new(HashMap::new()),
            stun_servers,
        }
    }

    async fn create_connection(&self) -> Result<Arc<RTCPeerConnection>, TransportError> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: self.stun_servers.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };

        api.new_peer_connection(config)
            .await
            .map(Arc::new)
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))
    }

    async fn wait_for_ice_gathering(
        connection: &Arc<RTCPeerConnection>,
    ) -> Result<String, TransportError> {
        let mut gathering_complete = connection.gathering_complete_promise().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), gathering_complete.recv()).await;

        let local_desc = connection.local_description().await.ok_or_else(|| {
            TransportError::ConnectionFailed("No local description after ICE gathering".to_string())
        })?;

        Ok(local_desc.sdp)
    }
}

#[async_trait]
impl PeerLinkFactory for RealPeerConnectionFactory {
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        let connection = self.create_connection().await?;

        let dc = connection
            .create_data_channel(
                DATA_CHANNEL_LABEL,
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let offer = connection
            .create_offer(None)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;
        connection
            .set_local_description(offer)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let sdp = Self::wait_for_ice_gathering(&connection).await?;

        self.pending.write().await.insert(
            target_peer_id.to_string(),
            PendingConnection {
                connection,
                data_channel: Some(dc.clone()),
            },
        );

        Ok((RealDataChannel::new(dc), sdp))
    }

    async fn accept_offer(
        &self,
        from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        let connection = self.create_connection().await?;

        let (dc_tx, dc_rx) = tokio::sync::oneshot::channel::<Arc<RTCDataChannel>>();
        let dc_tx = Arc::new(Mutex::new(Some(dc_tx)));

        connection.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let dc_tx = dc_tx.clone();
            Box::pin(async move {
                if let Some(tx) = dc_tx.lock().await.take() {
                    let _ = tx.send(dc);
                }
            })
        }));

        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;
        connection
            .set_remote_description(offer)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let answer = connection
            .create_answer(None)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;
        connection
            .set_local_description(answer)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let sdp = Self::wait_for_ice_gathering(&connection).await?;
        let dc = tokio::time::timeout(Duration::from_secs(30), dc_rx)
            .await
            .map_err(|_| {
                TransportError::ConnectionFailed("Timeout waiting for data channel".to_string())
            })?
            .map_err(|_| {
                TransportError::ConnectionFailed("Data channel sender dropped".to_string())
            })?;

        self.inbound.write().await.insert(
            from_peer_id.to_string(),
            PendingConnection {
                connection,
                data_channel: Some(dc.clone()),
            },
        );

        Ok((RealDataChannel::new(dc), sdp))
    }

    async fn handle_answer(
        &self,
        target_peer_id: &str,
        answer_sdp: &str,
    ) -> Result<Arc<dyn PeerLink>, TransportError> {
        let pending = self
            .pending
            .write()
            .await
            .remove(target_peer_id)
            .ok_or_else(|| TransportError::ConnectionFailed("No pending connection".to_string()))?;

        let answer = RTCSessionDescription::answer(answer_sdp.to_string())
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;
        pending
            .connection
            .set_remote_description(answer)
            .await
            .map_err(|err| TransportError::ConnectionFailed(err.to_string()))?;

        let dc = pending
            .data_channel
            .ok_or_else(|| TransportError::ConnectionFailed("No data channel".to_string()))?;

        Ok(RealDataChannel::new(dc))
    }

    async fn remove_peer(&self, peer_id: &str) -> Result<(), TransportError> {
        self.pending.write().await.remove(peer_id);
        self.inbound.write().await.remove(peer_id);
        Ok(())
    }
}
