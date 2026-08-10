//! The blob intake pipeline: one place that decides how much a blob may weigh
//! and one interface that admits it into storage.
//!
//! Almond accepts blobs through five doors — `PUT /upload`, `PATCH /upload`
//! reconstruction, `PUT /mirror`, the recursive HLS mirror, and the upstream
//! cache write. Each used to spell the sequence out for itself, and the
//! spellings had drifted: the size limit was computed three different ways,
//! and the upstream paths applied only `MAX_UPSTREAM_DOWNLOAD_SIZE_MB` without
//! ever consulting the absolute `MAX_BLOB_SIZE_MB` ceiling that uploads are
//! held to.

use std::sync::Arc;

use crate::error::AppResult;
use crate::models::{AppState, BlobOrigin, FileMetadata};
use crate::services::file_storage::{self, BlobPublication, TempBlob};

/// Which door a blob is arriving through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intake {
    /// A client uploading directly, whole or in chunks.
    ClientUpload,
    /// A blob pulled from another server: a mirror, an HLS segment, or an
    /// upstream cache fill.
    UpstreamFetch,
}

/// The absolute ceiling and, for fetches, the upstream bound.
///
/// `MAX_BLOB_SIZE_MB` is an absolute ceiling on anything this server stores.
/// An upstream fetch is additionally bounded by
/// `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`, but can never exceed the ceiling — which
/// the upstream download path used to, because it applied only its own bound.
const fn resolve_limit(max_blob_bytes: u64, max_upstream_bytes: u64, intake: Intake) -> u64 {
    match intake {
        Intake::ClientUpload => max_blob_bytes,
        Intake::UpstreamFetch => {
            if max_upstream_bytes < max_blob_bytes {
                max_upstream_bytes
            } else {
                max_blob_bytes
            }
        }
    }
}

/// The most this server will store for one blob arriving through `intake`.
#[must_use]
pub fn size_limit(state: &AppState, intake: Intake) -> u64 {
    resolve_limit(
        state.max_blob_size_bytes,
        state
            .max_upstream_download_size_mb
            .saturating_mul(1024 * 1024),
        intake,
    )
}

/// What a completed blob is, once its bytes are on disk and verified.
///
/// Replaces the seven positional arguments the old `finalize_upload` took, and
/// the hand-built publication the upstream cache write assembled inline.
pub struct Accepted {
    pub sha256: String,
    pub origin: BlobOrigin,
    pub size: u64,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    pub expiration: Option<u64>,
}

/// Admit a verified temporary blob into storage.
///
/// Consumes the `TempBlob`: after this returns the bytes either live at their
/// final location or have been unlinked, and no caller is left holding a path
/// it has to remember to clean up.
pub async fn accept(
    state: &AppState,
    temp: TempBlob,
    accepted: Accepted,
) -> AppResult<Arc<FileMetadata>> {
    file_storage::publish_blob(
        state,
        temp,
        BlobPublication {
            sha256: accepted.sha256,
            origin: accepted.origin,
            extension: accepted.extension,
            mime_type: accepted.mime_type,
            size: accepted.size,
            expiration: accepted.expiration,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_client_upload_is_bounded_only_by_the_absolute_ceiling() {
        // The upstream bound is irrelevant to something a client is pushing.
        assert_eq!(
            resolve_limit(100 * MIB, 10 * MIB, Intake::ClientUpload),
            100 * MIB
        );
    }

    #[test]
    fn an_upstream_fetch_takes_the_tighter_of_the_two_bounds() {
        assert_eq!(
            resolve_limit(100 * MIB, 10 * MIB, Intake::UpstreamFetch),
            10 * MIB
        );
    }

    #[test]
    fn an_upstream_fetch_can_never_exceed_the_absolute_ceiling() {
        // The regression this module exists for: the upstream download path
        // applied only MAX_UPSTREAM_DOWNLOAD_SIZE_MB, so configuring it above
        // MAX_BLOB_SIZE_MB let a cached blob outweigh anything an upload could
        // ever be.
        assert_eq!(
            resolve_limit(10 * MIB, 500 * MIB, Intake::UpstreamFetch),
            10 * MIB
        );
    }

    #[test]
    fn equal_bounds_are_not_a_special_case() {
        assert_eq!(
            resolve_limit(10 * MIB, 10 * MIB, Intake::UpstreamFetch),
            10 * MIB
        );
    }
}
