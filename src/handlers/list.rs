use axum::{
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use nostr_relay_pool::prelude::*;
use serde_json::Value;
use tracing::{info, warn};

use crate::{
    models::{AppState, ListQuery},
    services::authorization::{self, Operation},
};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1000;

/// Parse the BUD-12 `/list/<pubkey>` segment.
///
/// BUD-12 requires hexadecimal pubkeys.  `npub` is deliberately not accepted
/// here: this endpoint has one canonical identity syntax, and `?as=` is not a
/// list filter.
fn parse_path_pubkey(path: &str) -> Result<Option<PublicKey>, StatusCode> {
    let Some(value) = path.strip_prefix("/list/") else {
        return Ok(None);
    };
    if value.is_empty() || value.contains('/') {
        return Err(StatusCode::BAD_REQUEST);
    }
    PublicKey::from_hex(value)
        .map(Some)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// List the operator's catalogue.
///
/// Almond intentionally does not track per-blob ownership. `ALLOWED_NPUBS`
/// therefore defines its ownership boundary: a whitelisted key represents the
/// shared operator catalogue, while every other requested pubkey has no listed
/// blobs. The endpoint is never public, even if uploading is public.
pub async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
    headers: HeaderMap,
    req: Request,
) -> Result<Json<Value>, StatusCode> {
    if !state.feature_list_enabled {
        warn!("List feature is disabled");
        return Err(StatusCode::FORBIDDEN);
    }

    let authorized = authorization::authorize(&headers, &state, Operation::List)
        .await
        .map_err(StatusCode::from)?;
    authorized.bind_without_hash().map_err(StatusCode::from)?;

    let requested_pubkey = parse_path_pubkey(req.uri().path())?;
    let serves_catalog = requested_pubkey
        .as_ref()
        .is_none_or(|pubkey| state.allowed_pubkeys.contains(pubkey));

    let since = params.since.unwrap_or(0);
    let until = params.until.unwrap_or(u64::MAX);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let cursor = if let Some(cursor) = params.cursor.as_ref() {
        if cursor.len() != 64 || !cursor.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        Some(cursor.to_ascii_lowercase())
    } else {
        None
    };

    let files = if serves_catalog {
        state
            .file_index
            .page(since, until, None, cursor.as_deref(), limit)
            .await
    } else {
        Vec::new()
    };

    let paginated_blobs = files
        .into_iter()
        .map(|(sha256, metadata)| {
            serde_json::to_value(state.create_blob_descriptor(&sha256, &metadata))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        })
        .collect::<Result<Vec<_>, _>>()?;

    info!(
        caller = %authorized.pubkey().to_hex(),
        requested_pubkey = ?requested_pubkey.map(|pubkey| pubkey.to_hex()),
        count = paginated_blobs.len(),
        "List request completed"
    );
    Ok(Json(Value::Array(paginated_blobs)))
}
