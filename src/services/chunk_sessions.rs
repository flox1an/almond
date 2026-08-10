//! Chunked-upload session registry.
//!
//! Owns the resumable-upload state machine that previously lived inline in
//! `handlers/upload.rs::patch_upload`. Every lock acquisition is inside this
//! type and no `.await` happens while a guard is held, so callers cannot
//! introduce a lock-across-await or a TOCTOU on the completion transition.
//!
//! The payment race the old code had: "will this chunk complete the upload?"
//! was decided under a *read* guard, the guard was dropped, and then the chunk
//! was committed under a *write* guard. Two concurrent final chunks both saw
//! "not complete" and neither paid. Here the decision and the commit are one
//! atomic `commit()` transition: exactly one caller observes
//! [`Commit::Complete`], and payment is charged on that transition.

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

/// Result of [`ChunkSessions::reserve`].
pub enum Reservation {
    /// A session exists (or was created). The caller may stream the body.
    Granted,
    /// Global session capacity exhausted.
    GlobalLimit,
    /// This owner's per-pubkey session capacity exhausted.
    PerPubkeyLimit,
}

/// Result of [`ChunkSessions::commit`].
#[must_use]
pub enum Commit {
    /// Chunk appended; the upload is still incomplete.
    Incomplete,
    /// Chunk appended and the upload is now complete; the session was removed
    /// from the registry and returned so the caller owns the finish (payment,
    /// reconstruction). Exactly one caller sees this variant per upload.
    Complete(ChunkUpload),
    /// Chunk overlaps an already-received chunk at this offset range.
    Overlap,
    /// The session's stored parameters disagree with this chunk's headers.
    ParamMismatch,
}

/// The resumable-upload session registry.
pub struct ChunkSessions {
    map: RwLock<HashMap<ChunkUploadKey, ChunkUpload>>,
    limits: SessionLimits,
}

impl ChunkSessions {
    pub fn new(limits: SessionLimits) -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            limits,
        }
    }

    /// Ensure a session exists for `key`, enforcing global and per-pubkey
    /// capacity. Call *before* streaming the body so capacity is checked
    /// before any bytes hit disk.
    pub async fn reserve(&self, key: &ChunkUploadKey, params: &SessionParams) -> Reservation {
        let mut map = self.map.write().await;
        if map.contains_key(key) {
            return Reservation::Granted;
        }
        let owner_sessions = map
            .keys()
            .filter(|existing| existing.pubkey == key.pubkey)
            .count();
        if map.len() >= self.limits.max_sessions {
            return Reservation::GlobalLimit;
        }
        if owner_sessions >= self.limits.max_sessions_per_pubkey {
            return Reservation::PerPubkeyLimit;
        }
        map.insert(
            key.clone(),
            ChunkUpload {
                sha256: params.sha256.clone(),
                owner: params.owner,
                upload_type: params.upload_type.clone(),
                upload_length: params.upload_length,
                chunks: Vec::new(),
                created_at: Instant::now(),
                expiration: params.expiration,
            },
        );
        Reservation::Granted
    }

    /// Append a chunk to the session for `key`.
    ///
    /// The completion decision and the chunk insertion are one atomic
    /// transition under the write guard. When the accumulated length reaches
    /// `upload_length` the session is removed and returned as
    /// [`Commit::Complete`]; the caller is then responsible for payment and
    /// reconstruction. No `.await` occurs while the guard is held.
    pub async fn commit(
        &self,
        key: &ChunkUploadKey,
        chunk: ChunkInfo,
        upload_type: &str,
        upload_length: u64,
    ) -> Commit {
        let mut map = self.map.write().await;
        let Some(upload) = map.get_mut(key) else {
            // Session was evicted or never reserved; caller must restart.
            return Commit::ParamMismatch;
        };
        if upload.upload_type != upload_type || upload.upload_length != upload_length {
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
            // Atomic remove: a second finisher cannot observe a complete
            // session, so only this caller proceeds to payment+reconstruction.
            let complete = map.remove(key).expect("session present above");
            return Commit::Complete(complete);
        }
        Commit::Incomplete
    }

    /// Re-insert a session previously returned by [`Commit::Complete`].
    ///
    /// Used by the payment flow: when the completing chunk's payment is
    /// rejected, the session is restored *without* the completing chunk so the
    /// client can retry the final chunk with payment attached (the reactive
    /// BUD-07 flow). Only call with a session you received from `commit`.
    pub async fn reinsert(&self, key: &ChunkUploadKey, upload: ChunkUpload) {
        self.map.write().await.insert(key.clone(), upload);
    }

    /// Remove sessions idle longer than `age` and return them so the caller
    /// can clean up their chunk files. Used by the periodic cleanup job.
    pub async fn evict_older_than(&self, age: Duration) -> Vec<ChunkUpload> {
        let mut map = self.map.write().await;
        let mut evicted = Vec::new();
        map.retain(|_, upload| {
            if upload.created_at.elapsed() >= age {
                evicted.push(upload.clone());
                false
            } else {
                true
            }
        });
        evicted
    }
}
