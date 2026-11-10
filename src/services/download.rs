//! Shared upstream cache fills: one upstream transfer per blob.
//!
//! A blob that is not held locally is fetched from upstream exactly once per
//! process while the fill is live, however many clients ask for it and however
//! they slice it. The first request for a key creates a *session* and spawns an
//! owner task; every later request for the same key attaches to that session.
//! The owner walks the upstream tiers, issues one full `GET`, spools the body
//! to a temporary file, verifies SHA-256 and length, and publishes the blob to
//! the index. Readers tail the spool file (see `services::tail`), each at its
//! own pace and offset, so a slow client never slows the transfer or another
//! client, and nothing is buffered in memory beyond one read chunk per reader.
//!
//! Guarantees are deliberately narrow: one attempt, per process, per live
//! session. A failed fill is not retried for its waiters; nothing coordinates
//! separate processes, restarts, or different source contexts (see
//! [`SessionKey`]).
//!
//! Everything here is bounded by fixed constants rather than configuration:
//! distinct concurrent fills, concurrently streaming readers, and the disk
//! bytes fills may claim. Exhausting a bound fails the request explicitly
//! (`503`/`507`); it never opens a second upstream transfer to get around it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info};

use crate::models::AppState;

/// Distinct upstream fills that may run at once.
// ponytail: fixed ceiling; make it configurable if operators need more parallel cold fetches.
pub const MAX_CONCURRENT_FILLS: usize = 8;

/// Response bodies that may stream from in-flight spools at once.
// ponytail: fixed ceiling; make it configurable if one process serves more concurrent viewers.
pub const MAX_ATTACHED_READERS: usize = 256;

/// How long a source key that produced an over-limit blob skips the fill and
/// proxies straight to upstream.
pub const UNCACHEABLE_TTL: Duration = Duration::from_secs(600);

/// Cap on remembered over-limit keys, so the marker map stays bounded.
const MAX_UNCACHEABLE_MARKERS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Running,
    /// Verified and published; every byte is readable.
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Bytes readers may consume. For a known length the owner holds back the
    /// final byte until the hash is verified, so no reader can finish a body
    /// that turns out to be the wrong blob.
    pub written: u64,
    pub phase: DownloadPhase,
}

/// A spool file that readers can tail while the owner fills it.
pub struct DownloadHandle {
    pub temp_path: PathBuf,
    pub content_type: String,
    /// The full body length, when upstream declared one.
    pub total_len: Option<u64>,
    pub progress: watch::Sender<DownloadProgress>,
    /// Where the bytes live after publication renamed the spool. Set before
    /// the phase becomes `Complete`, so a reader that finds the spool gone can
    /// wait for the terminal phase and open this instead.
    pub final_path: OnceLock<PathBuf>,
}

/// The owner's decision, as seen by every request attached to a session.
#[derive(Clone)]
pub enum FillState {
    /// Walking sources or waiting for response headers.
    Pending,
    /// The spool exists; attach and tail it.
    Ready(Arc<DownloadHandle>),
    /// Upstream declared a body above the cache limit, so nothing is spooled.
    Uncacheable(Arc<UncacheableResponse>),
    /// Terminal failure; waiters answer with this status.
    Failed(StatusCode),
}

/// An over-limit response the owner already holds.
pub struct UncacheableResponse {
    pub content_type: String,
    pub total_len: Option<u64>,
    /// The owner's full-body response. The first `GET` waiter takes it and
    /// streams it as is; later waiters issue their own request.
    pub response: Mutex<Option<reqwest::Response>>,
}

type Session = Arc<watch::Sender<FillState>>;

#[derive(Default)]
struct Reserved {
    bytes: u64,
    fills: usize,
}

/// Registry of live fills plus the ceilings that bound them.
pub struct UpstreamFills {
    sessions: Mutex<HashMap<SessionKey, Session>>,
    uncacheable: Mutex<HashMap<SessionKey, Instant>>,
    fill_slots: Arc<Semaphore>,
    reader_slots: Arc<Semaphore>,
    /// Serialises disk admission so check-then-reserve is atomic.
    admission: tokio::sync::Mutex<()>,
    reserved: Mutex<Reserved>,
}

impl Default for UpstreamFills {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            uncacheable: Mutex::default(),
            fill_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_FILLS)),
            reader_slots: Arc::new(Semaphore::new(MAX_ATTACHED_READERS)),
            admission: tokio::sync::Mutex::default(),
            reserved: Mutex::default(),
        }
    }
}

/// The registry's locks guard plain maps and counters that are valid after
/// any panic, so a poisoned lock is simply taken over.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a request relates to the fill for its key.
pub enum Join {
    /// No fill was live: the caller must spawn the owner with this lease.
    Owner(FillLease),
    /// A fill is live: wait on its state.
    Waiter(watch::Receiver<FillState>),
    /// This key recently produced an over-limit blob: proxy directly.
    Uncacheable,
}

/// Identity of a fill: the blob plus where it may be fetched from.
///
/// Configured-only traffic carries the lowercase hash and no sources. A
/// request that names its own sources (`?origin=`, `?xs=`/`?servers=`,
/// `?as=`) keeps them as typed fields, compared field by field, so no
/// delimiter can make two different source lists collide and an unverified
/// origin can never supply the transfer, failure, or over-limit marker that
/// configured traffic attaches to. (It still shares the fill and disk limits.)
/// Verified bytes land in the shared index either way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    hash: String,
    custom_origin: Option<String>,
    xs_servers: Option<Vec<String>>,
    author_pubkey: Option<String>,
}

impl SessionKey {
    #[must_use]
    pub fn new(
        hash: &str,
        custom_origin: Option<&str>,
        xs_servers: Option<&[String]>,
        author_pubkey: Option<&nostr_sdk::prelude::PublicKey>,
    ) -> Self {
        Self {
            hash: hash.to_ascii_lowercase(),
            custom_origin: custom_origin.map(ToString::to_string),
            xs_servers: xs_servers.map(<[String]>::to_vec),
            author_pubkey: author_pubkey.map(|key| key.to_hex()),
        }
    }
}

impl UpstreamFills {
    /// Attach to the live fill for `key`, or become its owner.
    ///
    /// Lookup and insertion happen under one lock, so two requests can never
    /// both become owners. A new fill needs a free fill slot; without one this
    /// fails with `503` rather than letting the request fetch on its own.
    pub fn join(self: &Arc<Self>, key: &SessionKey) -> Result<Join, StatusCode> {
        if self.is_uncacheable(key) {
            return Ok(Join::Uncacheable);
        }
        let mut sessions = lock(&self.sessions);
        if let Some(session) = sessions.get(key) {
            return Ok(Join::Waiter(session.subscribe()));
        }
        let slot = self.fill_slots.clone().try_acquire_owned().map_err(|_| {
            debug!("All {MAX_CONCURRENT_FILLS} upstream fill slots are busy");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
        let (session, _) = watch::channel(FillState::Pending);
        let session = Arc::new(session);
        sessions.insert(key.clone(), session.clone());
        Ok(Join::Owner(FillLease {
            fills: self.clone(),
            key: key.clone(),
            session,
            _slot: slot,
        }))
    }

    /// One slot per response body streaming from a spool, held until the
    /// body is dropped. `503` when every slot is taken.
    pub fn reader_slot(&self) -> Result<OwnedSemaphorePermit, StatusCode> {
        self.reader_slots.clone().try_acquire_owned().map_err(|_| {
            debug!("All {MAX_ATTACHED_READERS} upstream reader slots are busy");
            StatusCode::SERVICE_UNAVAILABLE
        })
    }

    fn is_uncacheable(&self, key: &SessionKey) -> bool {
        lock(&self.uncacheable)
            .get(key)
            .is_some_and(|marked| marked.elapsed() < UNCACHEABLE_TTL)
    }

    /// Claim disk for one fill of up to `bytes`, or fail with `507`.
    ///
    /// Applies the same limits as `file_storage::ensure_storage_capacity`
    /// (aggregate size, file count, free-disk reserve), but against the
    /// index *plus every other fill's reservation*, so concurrent fills cannot
    /// each pass the check and jointly overrun it. Free space already reflects
    /// bytes other fills have written, which double-counts them: conservative
    /// by design. The reservation is released when the guard drops, after
    /// publication has moved the bytes into the index.
    ///
    /// The guarantee covers fills against each other only. Client uploads and
    /// mirrors still check capacity on their own (`ensure_storage_capacity`)
    /// without seeing these reservations, so this is not a global hard quota.
    pub async fn reserve_disk(
        self: &Arc<Self>,
        state: &AppState,
        bytes: u64,
    ) -> Result<DiskReservation, StatusCode> {
        let _admission = self.admission.lock().await;
        let (pending_bytes, pending_fills) = {
            let reserved = lock(&self.reserved);
            (reserved.bytes, reserved.fills)
        };
        let needed = pending_bytes.saturating_add(bytes);

        let stats = state.file_index.stats().await;
        let over_size = state.max_total_size != 0
            && stats.total_bytes.saturating_add(needed) > state.max_total_size;
        let over_files =
            state.max_total_files != 0 && stats.count + pending_fills >= state.max_total_files;
        if over_size || over_files {
            debug!("Upstream fill of {bytes} bytes would exceed the storage quota");
            return Err(StatusCode::INSUFFICIENT_STORAGE);
        }
        let available = fs4::available_space(&state.storage.root).map_err(|error| {
            tracing::error!("Failed to inspect available disk space: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        if available < state.min_free_disk_bytes.saturating_add(needed) {
            debug!("Upstream fill of {bytes} bytes would violate the free-disk reserve");
            return Err(StatusCode::INSUFFICIENT_STORAGE);
        }

        let mut reserved = lock(&self.reserved);
        reserved.bytes += bytes;
        reserved.fills += 1;
        Ok(DiskReservation {
            fills: self.clone(),
            bytes,
        })
    }
}

/// Disk claimed by one fill; released on drop, whatever the outcome.
pub struct DiskReservation {
    fills: Arc<UpstreamFills>,
    bytes: u64,
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        let mut reserved = lock(&self.fills.reserved);
        reserved.bytes -= self.bytes;
        reserved.fills -= 1;
    }
}

/// The owner's hold on a session and its fill slot.
///
/// Dropping the lease ends the session on every path, including panic:
/// a still-`Pending` session fails with `502`, a still-running spool is marked
/// `Failed`, the map entry is removed, and the fill slot is released. Readers
/// that already hold a `Ready` handle keep it, so a completed spool stays
/// readable while the index takes over.
pub struct FillLease {
    fills: Arc<UpstreamFills>,
    key: SessionKey,
    session: Session,
    _slot: OwnedSemaphorePermit,
}

impl FillLease {
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<FillState> {
        self.session.subscribe()
    }

    pub fn publish(&self, state: FillState) {
        self.session.send_replace(state);
    }

    /// Remember that this key yields an over-limit blob, so requests for the
    /// next [`UNCACHEABLE_TTL`] proxy directly instead of starting a full
    /// fetch that will only be thrown away.
    pub fn mark_uncacheable(&self) {
        let mut markers = lock(&self.fills.uncacheable);
        markers.retain(|_, marked| marked.elapsed() < UNCACHEABLE_TTL);
        if markers.len() < MAX_UNCACHEABLE_MARKERS {
            markers.insert(self.key.clone(), Instant::now());
        }
    }
}

impl Drop for FillLease {
    fn drop(&mut self) {
        self.session.send_if_modified(|state| {
            if matches!(state, FillState::Pending) {
                *state = FillState::Failed(StatusCode::BAD_GATEWAY);
                true
            } else {
                false
            }
        });
        if let FillState::Ready(handle) = &*self.session.borrow() {
            handle.progress.send_if_modified(|progress| {
                if progress.phase == DownloadPhase::Running {
                    progress.phase = DownloadPhase::Failed;
                    true
                } else {
                    false
                }
            });
        }

        let mut sessions = lock(&self.fills.sessions);
        if sessions
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.session))
        {
            sessions.remove(&self.key);
            info!("Upstream fill session for {} ended", self.key.hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_traffic_and_named_sources_never_share_a_key() {
        let hash = "A3F5C01F0F87B6E1D2C3A4B5C6D7E8F90123456789ABCDEF0123456789ABCDEF";
        let bare = SessionKey::new(hash, None, None, None);
        assert_eq!(
            bare,
            SessionKey::new(&hash.to_ascii_lowercase(), None, None, None)
        );
        assert_ne!(
            bare,
            SessionKey::new(hash, Some("https://a.example"), None, None)
        );
        assert_ne!(
            SessionKey::new(hash, Some("https://a.example"), None, None),
            SessionKey::new(hash, Some("https://b.example"), None, None)
        );
        // A comma inside one URL is not two servers.
        assert_ne!(
            SessionKey::new(hash, None, Some(&["https://a,https://b".to_string()]), None),
            SessionKey::new(
                hash,
                None,
                Some(&["https://a".to_string(), "https://b".to_string()]),
                None
            )
        );
    }

    #[tokio::test]
    async fn one_owner_per_key_and_lease_drop_fails_waiters() {
        let fills = Arc::new(UpstreamFills::default());
        let key = SessionKey::new("k", None, None, None);
        let Ok(Join::Owner(lease)) = fills.join(&key) else {
            panic!("first join must own");
        };
        let Ok(Join::Waiter(mut waiter)) = fills.join(&key) else {
            panic!("second join must wait");
        };
        drop(lease);
        let state = waiter
            .wait_for(|state| !matches!(state, FillState::Pending))
            .await
            .unwrap()
            .clone();
        assert!(matches!(state, FillState::Failed(StatusCode::BAD_GATEWAY)));
        assert!(matches!(fills.join(&key), Ok(Join::Owner(_))));
        assert_eq!(fills.fill_slots.available_permits(), MAX_CONCURRENT_FILLS);
    }
}
