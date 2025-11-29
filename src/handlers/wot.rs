use axum::{
	extract::{Query, State},
	http::{header, HeaderMap},
	response::Response,
};
use bloomfilter::Bloom;
use serde::Deserialize;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use crate::models::AppState;

#[derive(Deserialize)]
pub struct WotQuery {
	// format: json|bin (default: json)
	pub format: Option<String>,
	// false positive rate (default ~0.01)
	pub fp: Option<f64>,
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
	let num_items = trusted_pubkeys.len().max(1);
	let fp = q.fp.unwrap_or(0.01).clamp(1e-6, 0.2);
	let mut bloom = Bloom::new_for_fp_rate(num_items, fp);
	for pubkey in trusted_pubkeys.keys() {
		// Insert pubkey as hex string bytes
		bloom.set(pubkey.to_hex().as_bytes());
	}
    let bits = bloom.bitmap();

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
        let maybe = bloom.check(probe.as_bytes());
        let payload = serde_json::json!({
            "test": probe,
            "maybe": maybe,
            "count": num_items,
            "fp": fp,
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
		// Structure: { n, fp, k, m, bits (base64) }
		let payload = serde_json::json!({
			"count": num_items,
			"fp": fp,
			"k": bloom.number_of_hash_functions(),
			"m": bits.len() * 8,
			"bits_b64": BASE64.encode(bits),
		});
		let resp = Response::builder()
			.status(axum::http::StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/json")
			.body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
			.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
		Ok(resp)
	} else {
		// Binary: raw bitmap
		let resp = Response::builder()
			.status(axum::http::StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/octet-stream")
			.body(axum::body::Body::from(bits.to_vec()))
			.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
		Ok(resp)
	}
}
