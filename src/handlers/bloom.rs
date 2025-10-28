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
pub struct BloomQuery {
	// format: json|bin (default: json)
	pub format: Option<String>,
	// false positive rate (default ~0.01)
	pub fp: Option<f64>,
}

pub async fn get_bloom(
	State(state): State<AppState>,
	Query(q): Query<BloomQuery>,
	_headers: HeaderMap,
) -> Result<Response, axum::http::StatusCode> {
	// Collect all keys (sha256 prefixes) from index
	let index = state.file_index.read().await;
	let num_items = index.len().max(1);
	let fp = q.fp.unwrap_or(0.01).clamp(1e-6, 0.2);
	let mut bloom = Bloom::new_for_fp_rate(num_items as usize, fp);
	for key in index.keys() {
		// Keys in index are filename prefixes (first 64 chars of sha256). Insert as bytes.
		bloom.set(key.as_bytes());
	}
	let bits = bloom.bitmap();

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
