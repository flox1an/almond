use axum::{
	extract::{Query, State},
	http::{header, HeaderMap},
	response::Response,
};
use xorf::{BinaryFuse16, Filter};
use serde::Deserialize;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

use crate::models::AppState;

#[derive(Deserialize)]
pub struct WotQuery {
	// format: json|bin (default: json)
	pub format: Option<String>,
    // optional hex pubkey to test against the filter
    pub test: Option<String>,
}

pub async fn get_wot(
	State(state): State<AppState>,
	Query(q): Query<WotQuery>,
	_headers: HeaderMap,
) -> Result<Response, axum::http::StatusCode> {
	// Collect all pubkeys from trusted_pubkeys (the WOT)
	let trusted_pubkeys = state.trusted_pubkeys.read().await;
	let num_items = trusted_pubkeys.len();

	// Binary fuse filters need at least one item
	if num_items == 0 {
		let payload = serde_json::json!({
			"count": 0,
			"error": "no items in WOT",
		});
		let resp = Response::builder()
			.status(axum::http::StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/json")
			.body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
			.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
		return Ok(resp);
	}

	// Hash pubkeys to u64 values for the filter
	let keys: Vec<u64> = trusted_pubkeys.keys().map(|pubkey| {
		let mut hasher = DefaultHasher::new();
		pubkey.to_hex().hash(&mut hasher);
		hasher.finish()
	}).collect();

	// Build the binary fuse filter
	let filter = BinaryFuse16::try_from(&keys)
		.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    // If a test pubkey is provided, respond with JSON test result regardless of format
    if let Some(mut probe) = q.test {
        // normalize: lowercase and validate hex
        probe = probe.trim().to_lowercase();
        if probe.len() != 64 || !probe.chars().all(|c| c.is_ascii_hexdigit()) {
            let payload = serde_json::json!({
                "error": "invalid pubkey hex (need exactly 64 hex chars)",
            });
            let resp = Response::builder()
                .status(axum::http::StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            return Ok(resp);
        }
        // Hash the test pubkey to u64
        let mut hasher = DefaultHasher::new();
        probe.hash(&mut hasher);
        let key_hash = hasher.finish();
        let maybe = filter.contains(&key_hash);
        let payload = serde_json::json!({
            "test": probe,
            "maybe": maybe,
            "count": num_items,
        });
        let resp = Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
            .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(resp);
    }

	let want_json = q
		.format
		.as_deref()
		.map(|f| f.eq_ignore_ascii_case("json"))
		.unwrap_or(true);

	if want_json {
		// Get the filter's internal data
		// BinaryFuse16 stores fingerprints as Vec<u16>
		let fingerprints_bytes: Vec<u8> = filter.fingerprints.iter()
			.flat_map(|&fp| fp.to_le_bytes())
			.collect();

		// Structure: { count, fingerprints_b64, len }
		let payload = serde_json::json!({
			"count": num_items,
			"len": filter.len(),
			"fingerprints_b64": BASE64.encode(&fingerprints_bytes),
		});
		let resp = Response::builder()
			.status(axum::http::StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/json")
			.body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
			.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
		Ok(resp)
	} else {
		// Binary: raw fingerprints as u16 little-endian
		let fingerprints_bytes: Vec<u8> = filter.fingerprints.iter()
			.flat_map(|&fp| fp.to_le_bytes())
			.collect();

		let resp = Response::builder()
			.status(axum::http::StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/octet-stream")
			.body(axum::body::Body::from(fingerprints_bytes))
			.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
		Ok(resp)
	}
}
