//! One authorization decision: may this signed event perform this operation?
//!
//! Almond has a single authorization concept, but it used to be reassembled by
//! hand at every handler: the same `FeatureMode` to auth-mode match was written
//! out three times, the `Authorization` header was extracted four times, and
//! whether a destructive verb consumed its single-use nonce was decided per
//! handler — which is how `DELETE` ended up with no replay guard at all.
//!
//! This module owns that decision. `services::auth` keeps what it is good at:
//! the BUD-11 event grammar (parsing, signature and expiry verification, tag
//! validators). This module owns the policy on top of it.

use axum::http::{header, HeaderMap};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use nostr_relay_pool::prelude::*;

use crate::error::{AppError, AppResult};
use crate::models::{AppState, FeatureMode};
use crate::services::auth::{self, AuthMode};

/// A verb a client can ask this server to perform.
///
/// The operation, not the handler, decides which feature flag governs it,
/// which BUD-11 verb tag the event must carry, and whether its authorization
/// event is single-use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Upload,
    Mirror,
    ChunkUpload,
    Delete,
}

impl Operation {
    /// The name used in the "feature is disabled" refusal.
    const fn name(self) -> &'static str {
        match self {
            Self::Upload | Self::ChunkUpload => "Upload",
            Self::Mirror => "Mirror",
            Self::Delete => "Delete",
        }
    }

    /// Destructive verbs permanently remove data, so their authorization event
    /// may be presented exactly once.
    const fn is_destructive(self) -> bool {
        matches!(self, Self::Delete)
    }

    /// The configured mode governing this operation, or `None` when the
    /// operation is not feature-gated.
    fn feature(self, state: &AppState) -> Option<FeatureMode> {
        match self {
            Self::Upload | Self::ChunkUpload => Some(state.feature_upload_enabled),
            Self::Mirror => Some(state.feature_mirror_enabled),
            // Deletion is never public: it is whitelist-only whatever the
            // upload feature is set to.
            Self::Delete => None,
        }
    }
}

/// Map a configured feature mode to the authorization mode it implies.
///
/// `has_whitelist` is whether `ALLOWED_NPUBS` is configured at all: a public
/// feature on a server with no whitelist authenticates the caller but does not
/// restrict which caller it may be.
///
/// `None` means the feature is off and the operation must be refused.
fn mode_for(feature: FeatureMode, has_whitelist: bool) -> Option<AuthMode> {
    match feature {
        FeatureMode::Off => None,
        FeatureMode::Wot => Some(AuthMode::WotOnly),
        FeatureMode::Dvm => Some(AuthMode::DvmOnly),
        FeatureMode::Public if has_whitelist => Some(AuthMode::Strict),
        FeatureMode::Public => Some(AuthMode::Unrestricted),
    }
}

/// A client that has been authenticated and cleared to attempt an operation.
///
/// Authorization is genuinely two-phase for uploads: the client is cleared
/// before its blob has been streamed, and the authorization can only be bound
/// to a hash once that hash is known. Holding the event in this value keeps
/// both phases in one module instead of leaving handlers to remember the
/// second one.
pub struct Authorized {
    event: Event,
    operation: Operation,
    expiration: u64,
}

impl Authorized {
    /// The authenticated client.
    #[must_use]
    pub const fn pubkey(&self) -> &PublicKey {
        &self.event.pubkey
    }

    /// The verified authorization event.
    ///
    /// Mirror reads the blob hash out of the event's `x` tag rather than
    /// receiving it from the client, so it needs the event itself.
    #[must_use]
    pub const fn event(&self) -> &Event {
        &self.event
    }

    /// Bind the authorization to the blob it actually applies to.
    ///
    /// Checks the verb tag and the `x` tag against `sha256`, and for a
    /// destructive verb consumes the single-use nonce. Call this before the
    /// operation takes effect — for a destructive verb that ordering is what
    /// makes the event unusable a second time.
    pub async fn bind(&self, state: &AppState, sha256: &str) -> AppResult<()> {
        match self.operation {
            Operation::Upload | Operation::Mirror => {
                auth::validate_upload_auth(&self.event, sha256)?;
            }
            Operation::ChunkUpload => {
                auth::validate_chunk_upload_auth(&self.event, sha256)?;
            }
            Operation::Delete => auth::validate_delete_auth(&self.event, sha256)?,
        }

        if self.operation.is_destructive() {
            consume_single_use(
                &state.destructive_event_replays,
                &self.event.id.to_string(),
                self.expiration,
            )
            .await?;
        }
        Ok(())
    }
}

/// Authenticate the caller and check it may attempt `operation`.
///
/// Verifies the BUD-11 event grammar, then the whitelist / web-of-trust / DVM
/// policy implied by the operation's feature mode. The returned value must be
/// bound to a blob hash before the operation takes effect.
pub async fn authorize(
    headers: &HeaderMap,
    state: &AppState,
    operation: Operation,
) -> AppResult<Authorized> {
    let mode = match operation.feature(state) {
        Some(feature) => mode_for(feature, !state.allowed_pubkeys.is_empty()).ok_or_else(|| {
            AppError::Forbidden(format!("{} feature is disabled", operation.name()))
        })?,
        None => AuthMode::Strict,
    };

    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    let event = auth::parse_auth_header(auth_header)?;
    let expiration = auth::verify_event_with_policy(&event, state)?;
    auth::validate_event_kind(&event, 24242)?;
    auth::check_pubkey_authorization(&event, state, mode).await?;

    Ok(Authorized {
        event,
        operation,
        expiration,
    })
}

/// Reject an event id that has already been presented, until it expires.
///
/// Shared by every destructive path so none of them can quietly omit it.
/// Takes the replay set rather than the whole runtime state, so the guarantee
/// it provides can actually be tested.
pub async fn consume_single_use(
    replays: &RwLock<HashMap<String, u64>>,
    event_id: &str,
    expires_at: u64,
) -> AppResult<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut seen = replays.write().await;
    // Expired ids are forgotten, so the set cannot grow without bound.
    seen.retain(|_, expiry| *expiry >= now);
    if seen.insert(event_id.to_string(), expires_at).is_some() {
        return Err(AppError::Conflict(
            "Authorization event was already used".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_feature_refuses_every_caller() {
        assert!(mode_for(FeatureMode::Off, true).is_none());
        assert!(mode_for(FeatureMode::Off, false).is_none());
    }

    #[test]
    fn public_restricts_only_when_a_whitelist_exists() {
        // The same feature mode means two different things depending on
        // whether the operator configured ALLOWED_NPUBS. This split used to be
        // written out in three handlers.
        assert!(matches!(
            mode_for(FeatureMode::Public, true),
            Some(AuthMode::Strict)
        ));
        assert!(matches!(
            mode_for(FeatureMode::Public, false),
            Some(AuthMode::Unrestricted)
        ));
    }

    #[test]
    fn trust_modes_ignore_whether_a_whitelist_exists() {
        // WoT and DVM consult the whitelist themselves as a fast path, so the
        // mode must not change with it.
        for has_whitelist in [true, false] {
            assert!(matches!(
                mode_for(FeatureMode::Wot, has_whitelist),
                Some(AuthMode::WotOnly)
            ));
            assert!(matches!(
                mode_for(FeatureMode::Dvm, has_whitelist),
                Some(AuthMode::DvmOnly)
            ));
        }
    }

    #[test]
    fn only_deletion_is_destructive_and_it_is_never_feature_gated() {
        assert!(Operation::Delete.is_destructive());
        for operation in [
            Operation::Upload,
            Operation::Mirror,
            Operation::ChunkUpload,
        ] {
            assert!(!operation.is_destructive());
        }
    }

    fn far_future() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600
    }

    #[tokio::test]
    async fn a_destructive_event_is_accepted_exactly_once() {
        // The regression this module exists for: DELETE consumed no nonce, so
        // one signed delete event could be replayed until it expired.
        let replays = RwLock::new(HashMap::new());
        let expires_at = far_future();

        assert!(consume_single_use(&replays, "event-a", expires_at)
            .await
            .is_ok());
        let replayed = consume_single_use(&replays, "event-a", expires_at).await;
        assert!(
            matches!(replayed, Err(AppError::Conflict(_))),
            "a replayed destructive event must be refused, got {replayed:?}"
        );
    }

    #[tokio::test]
    async fn distinct_events_do_not_block_each_other() {
        let replays = RwLock::new(HashMap::new());
        let expires_at = far_future();
        assert!(consume_single_use(&replays, "event-a", expires_at)
            .await
            .is_ok());
        assert!(consume_single_use(&replays, "event-b", expires_at)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn an_expired_id_is_forgotten_rather_than_retained() {
        // Without eviction the replay set grows for the life of the process.
        let replays = RwLock::new(HashMap::new());
        assert!(consume_single_use(&replays, "stale", 1).await.is_ok());
        // A later consumption sweeps the expired entry, so the id is free.
        assert!(consume_single_use(&replays, "other", far_future())
            .await
            .is_ok());
        assert!(!replays.read().await.contains_key("stale"));
    }
}
