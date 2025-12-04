use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bloomfilter::Bloom;
use chrono::Utc;
use serde::Deserialize;
use xorf::{BinaryFuse8, BinaryFuse16, BinaryFuse32, Filter};

use crate::models::AppState;

#[derive(Deserialize)]
pub struct FilterQuery {
    /// Optional hex sha256 to test against the filter
    pub test: Option<String>,
    /// False positive rate for bloom filter (default ~0.01)
    pub fp: Option<f64>,
}

/// Convert a SHA-256 hex string (64 chars) to a u64 fingerprint
/// Takes the first 16 hex characters (8 bytes) and interprets as big-endian u64
fn sha256_to_u64(hex: &str) -> u64 {
    let bytes: Vec<u8> = (0..16)
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();

    if bytes.len() >= 8 {
        u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        0
    }
}

/// Enum to hold different filter types for unified handling
enum FilterType {
    Bloom(Bloom<Vec<u8>>),
    Fuse8(BinaryFuse8),
    Fuse16(BinaryFuse16),
    Fuse32(BinaryFuse32),
}

impl FilterType {
    fn contains_hash(&self, hash: &str) -> bool {
        match self {
            FilterType::Bloom(bloom) => {
                let key = &hash[..64.min(hash.len())];
                bloom.check(&key.as_bytes().to_vec())
            }
            FilterType::Fuse8(filter) => {
                let fp = sha256_to_u64(hash);
                filter.contains(&fp)
            }
            FilterType::Fuse16(filter) => {
                let fp = sha256_to_u64(hash);
                filter.contains(&fp)
            }
            FilterType::Fuse32(filter) => {
                let fp = sha256_to_u64(hash);
                filter.contains(&fp)
            }
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            FilterType::Bloom(_) => "bloom",
            FilterType::Fuse8(_) => "binary-fuse-8",
            FilterType::Fuse16(_) => "binary-fuse-16",
            FilterType::Fuse32(_) => "binary-fuse-32",
        }
    }

    fn serialize(&self) -> Vec<u8> {
        match self {
            FilterType::Bloom(bloom) => bloom.bitmap().to_vec(),
            FilterType::Fuse8(filter) => bincode::serialize(filter).unwrap_or_default(),
            FilterType::Fuse16(filter) => bincode::serialize(filter).unwrap_or_default(),
            FilterType::Fuse32(filter) => bincode::serialize(filter).unwrap_or_default(),
        }
    }
}

/// Build filter based on algorithm type
fn build_filter(
    algorithm: &str,
    keys: &[String],
    fingerprints: &[u64],
    fp_rate: f64,
) -> Result<FilterType, &'static str> {
    match algorithm {
        "bloom" => {
            let num_items = keys.len().max(1);
            let mut bloom: Bloom<Vec<u8>> = Bloom::new_for_fp_rate(num_items, fp_rate);
            for key in keys {
                bloom.set(&key.as_bytes().to_vec());
            }
            Ok(FilterType::Bloom(bloom))
        }
        "binary-fuse-8" => {
            if fingerprints.is_empty() {
                return Err("empty fingerprints");
            }
            BinaryFuse8::try_from(fingerprints)
                .map(FilterType::Fuse8)
                .map_err(|_| "failed to construct filter")
        }
        "binary-fuse-16" => {
            if fingerprints.is_empty() {
                return Err("empty fingerprints");
            }
            BinaryFuse16::try_from(fingerprints)
                .map(FilterType::Fuse16)
                .map_err(|_| "failed to construct filter")
        }
        "binary-fuse-32" => {
            if fingerprints.is_empty() {
                return Err("empty fingerprints");
            }
            BinaryFuse32::try_from(fingerprints)
                .map(FilterType::Fuse32)
                .map_err(|_| "failed to construct filter")
        }
        _ => Err("unknown algorithm"),
    }
}

/// GET /filter - Unified filter endpoint (BUD-11)
///
/// Returns a JSON response containing a filter of all blob hashes.
/// The algorithm is determined by the FILTER_ALGORITHM env var:
/// - "bloom": Bloom filter (configurable false positive rate via `fp` query param)
/// - "binary-fuse-8": Binary Fuse8 filter (~0.4% false positive rate)
/// - "binary-fuse-16": Binary Fuse16 filter (~0.0015% false positive rate) [default]
/// - "binary-fuse-32": Binary Fuse32 filter (~0.00000002% false positive rate)
pub async fn get_filter(
    State(state): State<AppState>,
    Query(q): Query<FilterQuery>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let algorithm = &state.filter_algorithm;
    let fp_rate = q.fp.unwrap_or(0.01).clamp(1e-6, 0.2);

    // Collect all SHA-256 hashes from index
    let index = state.file_index.read().await;
    let count = index.len();

    // Collect keys for bloom filter and fingerprints for binary fuse filters
    let keys: Vec<String> = index
        .keys()
        .filter(|key| key.len() >= 64 && key.chars().take(64).all(|c| c.is_ascii_hexdigit()))
        .map(|key| key[..64].to_string())
        .collect();

    let fingerprints: Vec<u64> = keys.iter().map(|key| sha256_to_u64(key)).collect();

    // Handle test query - check if a specific hash exists
    if let Some(mut probe) = q.test {
        probe = probe.trim().to_lowercase();
        if probe.len() < 64 || !probe.chars().take(64).all(|c| c.is_ascii_hexdigit()) {
            let payload = serde_json::json!({
                "error": "invalid sha256 hex (need at least 64 hex chars)",
            });
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
        }

        // Build filter for test
        let filter = if keys.is_empty() && !algorithm.starts_with("bloom") {
            None
        } else {
            build_filter(algorithm, &keys, &fingerprints, fp_rate).ok()
        };

        let maybe = filter
            .as_ref()
            .map(|f| f.contains_hash(&probe))
            .unwrap_or(false);

        let mut payload = serde_json::json!({
            "test": &probe[..64],
            "maybe": maybe,
            "count": count,
            "type": filter.as_ref().map(|f| f.type_name()).unwrap_or(algorithm),
        });

        // Add fp rate for bloom filters
        if algorithm == "bloom" {
            payload["fp"] = serde_json::json!(fp_rate);
        }

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    }

    // Generate ETag from count and a simple hash of keys
    let etag = format!(
        "\"{}-{}-{}\"",
        algorithm,
        count,
        fingerprints.iter().take(10).map(|f| f % 1000).sum::<u64>()
    );

    // Check If-None-Match for conditional request
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(value) = if_none_match.to_str() {
            if value == etag {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &etag)
                    .body(axum::body::Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
            }
        }
    }

    // Build the filter
    let timestamp = Utc::now().timestamp();

    // Handle empty index
    if keys.is_empty() && !algorithm.starts_with("bloom") {
        let payload = serde_json::json!({
            "type": algorithm,
            "timestamp": timestamp,
            "count": 0,
            "filter": "",
        });

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "max-age=60")
            .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    }

    let filter = match build_filter(algorithm, &keys, &fingerprints, fp_rate) {
        Ok(f) => f,
        Err(msg) => {
            let payload = serde_json::json!({
                "error": format!("failed to construct filter: {}", msg),
            });
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
        }
    };

    let filter_b64 = BASE64.encode(filter.serialize());
    let filter_type = filter.type_name();

    // Build response per BUD-11 spec
    let mut payload = serde_json::json!({
        "type": filter_type,
        "timestamp": timestamp,
        "count": keys.len(),
        "filter": filter_b64,
    });

    // Add bloom-specific fields
    if let FilterType::Bloom(ref bloom) = filter {
        payload["fp"] = serde_json::json!(fp_rate);
        payload["k"] = serde_json::json!(bloom.number_of_hash_functions());
        payload["m"] = serde_json::json!(bloom.bitmap().len() * 8);
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "max-age=60")
        .body(axum::body::Body::from(serde_json::to_vec(&payload).unwrap()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
}
