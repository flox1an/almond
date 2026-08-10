//! Chunked-upload session registry.
//!
//! A reservation is acquired before its request body is streamed, so session
//! capacity is enforced before bytes reach disk. Reservations are kept separate
//! from persisted sessions: failed bodies release their reservation without
//! leaving an empty upload behind.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nostr_relay_pool::prelude::PublicKey;
use tokio::sync::RwLock;

use crate::models::{ChunkInfo, ChunkUpload, ChunkUploadKey};

/// Parameters a new session is created with, from the request headers.
pub struct SessionParams {
    pub sha256: String,
    pub owner: PublicKey,
    pub upload_type: String,
    pub upload_length: u64,
    pub expiration: Option<u64>,
}

/// Capacity limits for concurrent sessions.
#[derive(Clone, Copy)]
pub struct SessionLimits {
    pub max_sessions: usize,
    pub max_sessions_per_pubkey: usize,
}

/// A capacity reservation for one in-flight chunk request.
#[must_use]
pub struct ReservationTicket {
    key: ChunkUploadKey,
}

/// Result of [`ChunkSessions::reserve`].
pub enum Reservation {
    /// Capacity was reserved; the caller may stream the body.
    Granted(ReservationTicket),
    /// Global session capacity is exhausted.
    GlobalLimit,
    /// This owner's per-pubkey session capacity is exhausted.
    PerPubkeyLimit,
}

/// Result of [`ChunkSessions::commit`].
#[must_use]
pub enum Commit {
    /// Chunk appended; the upload is still incomplete.
    Incomplete,
    /// Chunk appended and the upload is now complete. The session was removed
    /// from the registry; the caller owns payment and reconstruction.
    Complete(ChunkUpload),
    /// Chunk overlaps an already-received chunk at this offset range.
    Overlap,
    /// The reservation or stored parameters disagree with this request.
    ParamMismatch,
}

struct SessionState {
    uploads: HashMap<ChunkUploadKey, ChunkUpload>,
    pending: HashMap<ChunkUploadKey, usize>,
}

/// The resumable-upload session registry.
pub struct ChunkSessions {
    state: RwLock<SessionState>,
    limits: SessionLimits,
}

impl ChunkSessions {
    pub fn new(limits: SessionLimits) -> Self {
        Self {
            state: RwLock::new(SessionState {
                uploads: HashMap::new(),
                pending: HashMap::new(),
            }),
            limits,
        }
    }

    /// Reserve capacity for one request before streaming its body.
    ///
    /// A reservation is not yet a persisted upload: callers must pass the
    /// ticket to [`Self::commit`] after a successful body write, or release it
    /// on every error path with [`Self::release`]. Pending reservations count
    /// toward both capacity limits, preventing disk-before-cap races without
    /// retaining failed requests as empty sessions.
    pub async fn reserve(&self, key: &ChunkUploadKey) -> Reservation {
        let mut state = self.state.write().await;

        if state.uploads.contains_key(key) || state.pending.contains_key(key) {
            *state.pending.entry(key.clone()).or_default() += 1;
            return Reservation::Granted(ReservationTicket { key: key.clone() });
        }

        let pending_without_upload = state
            .pending
            .keys()
            .filter(|pending_key| !state.uploads.contains_key(*pending_key));
        if state.uploads.len() + pending_without_upload.clone().count() >= self.limits.max_sessions
        {
            return Reservation::GlobalLimit;
        }
        let owner_sessions = state
            .uploads
            .keys()
            .chain(pending_without_upload)
            .filter(|existing| existing.pubkey == key.pubkey)
            .count();
        if owner_sessions >= self.limits.max_sessions_per_pubkey {
            return Reservation::PerPubkeyLimit;
        }

        state.pending.insert(key.clone(), 1);
        Reservation::Granted(ReservationTicket { key: key.clone() })
    }

    /// Discard a reservation after a failed pre-commit request.
    pub async fn release(&self, ticket: ReservationTicket) {
        let mut state = self.state.write().await;
        Self::consume_reservation(&mut state, &ticket.key);
    }

    /// Append a successfully written chunk to its session.
    ///
    /// The completion decision and insertion are one atomic transition. A
    /// completed session is removed before payment and reconstruction, so only
    /// one caller can observe [`Commit::Complete`].
    pub async fn commit(
        &self,
        ticket: ReservationTicket,
        params: &SessionParams,
        chunk: ChunkInfo,
    ) -> Commit {
        let mut state = self.state.write().await;
        if !Self::consume_reservation(&mut state, &ticket.key) {
            return Commit::ParamMismatch;
        }

        let upload = state
            .uploads
            .entry(ticket.key.clone())
            .or_insert_with(|| ChunkUpload {
                sha256: params.sha256.clone(),
                owner: params.owner,
                upload_type: params.upload_type.clone(),
                upload_length: params.upload_length,
                chunks: Vec::new(),
                created_at: Instant::now(),
                expiration: params.expiration,
            });
        if upload.upload_type != params.upload_type || upload.upload_length != params.upload_length
        {
            return Commit::ParamMismatch;
        }
        if upload.chunks.iter().any(|existing| {
            chunk.offset < existing.offset.saturating_add(existing.length)
                && existing.offset < chunk.offset.saturating_add(chunk.length)
        }) {
            return Commit::Overlap;
        }
        let new_total = upload
            .chunks
            .iter()
            .try_fold(0u64, |total, existing| total.checked_add(existing.length))
            .and_then(|total| total.checked_add(chunk.length));
        let Some(new_total) = new_total else {
            return Commit::ParamMismatch;
        };

        upload.chunks.push(chunk);
        if new_total == upload.upload_length {
            let complete = state
                .uploads
                .remove(&ticket.key)
                .expect("session inserted or found above");
            return Commit::Complete(complete);
        }
        Commit::Incomplete
    }

    /// Restore a payment-rejected completed upload if no other request for its
    /// key began while payment was pending.
    ///
    /// A concurrent request has priority over the failed completion: replacing
    /// its in-flight session would lose chunks and leak their temporary files.
    pub async fn restore(&self, key: &ChunkUploadKey, upload: ChunkUpload) -> bool {
        let mut state = self.state.write().await;
        if state.uploads.contains_key(key) || state.pending.contains_key(key) {
            return false;
        }
        state.uploads.insert(key.clone(), upload);
        true
    }

    /// Remove sessions idle longer than `age` and return them for file cleanup.
    /// Sessions with an in-flight reserved body are retained until that body
    /// reaches [`Self::commit`] or [`Self::release`].
    pub async fn evict_older_than(&self, age: Duration) -> Vec<ChunkUpload> {
        let mut state = self.state.write().await;
        let SessionState { uploads, pending } = &mut *state;
        let mut evicted = Vec::new();
        uploads.retain(|key, upload| {
            if pending.contains_key(key) || upload.created_at.elapsed() < age {
                true
            } else {
                evicted.push(upload.clone());
                false
            }
        });
        evicted
    }

    fn consume_reservation(state: &mut SessionState, key: &ChunkUploadKey) -> bool {
        let Some(count) = state.pending.get_mut(key) else {
            return false;
        };
        if *count == 1 {
            state.pending.remove(key);
        } else {
            *count -= 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const OWNER_HEX: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn key(hash: &str) -> ChunkUploadKey {
        ChunkUploadKey {
            pubkey: PublicKey::from_hex(OWNER_HEX).unwrap(),
            sha256: hash.to_string(),
        }
    }

    fn params(hash: &str, upload_length: u64) -> SessionParams {
        SessionParams {
            sha256: hash.to_string(),
            owner: PublicKey::from_hex(OWNER_HEX).unwrap(),
            upload_type: "media".to_string(),
            upload_length,
            expiration: None,
        }
    }

    fn chunk(offset: u64, length: u64) -> ChunkInfo {
        ChunkInfo {
            offset,
            length,
            chunk_path: PathBuf::from("test-chunk"),
        }
    }

    fn granted(reservation: Reservation) -> ReservationTicket {
        let Reservation::Granted(ticket) = reservation else {
            panic!("expected a granted reservation");
        };
        ticket
    }

    #[tokio::test]
    async fn released_failed_reservation_frees_global_capacity() {
        let sessions = ChunkSessions::new(SessionLimits {
            max_sessions: 1,
            max_sessions_per_pubkey: 1,
        });
        let first = key("first");
        let second = key("second");

        let ticket = granted(sessions.reserve(&first).await);
        assert!(matches!(
            sessions.reserve(&second).await,
            Reservation::GlobalLimit
        ));

        sessions.release(ticket).await;
        sessions
            .release(granted(sessions.reserve(&second).await))
            .await;
    }

    #[tokio::test]
    async fn restore_never_overwrites_a_racing_reservation() {
        let sessions = ChunkSessions::new(SessionLimits {
            max_sessions: 2,
            max_sessions_per_pubkey: 2,
        });
        let key = key("shared");
        let params = params("shared", 4);

        let completed = sessions
            .commit(granted(sessions.reserve(&key).await), &params, chunk(0, 4))
            .await;
        let Commit::Complete(upload) = completed else {
            panic!("expected the first chunk to complete the upload");
        };

        let racing = granted(sessions.reserve(&key).await);
        assert!(!sessions.restore(&key, upload).await);
        assert!(matches!(
            sessions.commit(racing, &params, chunk(0, 2)).await,
            Commit::Incomplete
        ));
    }

    #[tokio::test]
    async fn cleanup_skips_sessions_with_an_in_flight_body() {
        let sessions = ChunkSessions::new(SessionLimits {
            max_sessions: 1,
            max_sessions_per_pubkey: 1,
        });
        let key = key("active");
        let params = params("active", 4);

        assert!(matches!(
            sessions
                .commit(granted(sessions.reserve(&key).await), &params, chunk(0, 2))
                .await,
            Commit::Incomplete
        ));
        let pending = granted(sessions.reserve(&key).await);

        assert!(sessions.evict_older_than(Duration::ZERO).await.is_empty());

        sessions.release(pending).await;
        assert_eq!(sessions.evict_older_than(Duration::ZERO).await.len(), 1);
    }
}
