use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bloomfilter::Bloom;
use chrono::Utc;
use rmp::encode;
use serde::Deserialize;
use xorf::{BinaryFuse8, BinaryFuse16, BinaryFuse32};

use crate::models::AppState;

#[derive(Deserialize)]
pub struct FilterQuery {
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

/// Serialize BinaryFuse filter as MessagePack array instead of struct
/// The JavaScript expects: [seed, segment_length, segment_length_mask, segment_count_length, fingerprints]
fn serialize_binary_fuse_as_array<T>(filter: &T) -> Result<Vec<u8>, String>
where
    T: serde::Serialize,
{
    // First serialize to JSON to extract fields
    let json_value = serde_json::to_value(filter)
        .map_err(|e| format!("Failed to serialize to JSON: {}", e))?;

    let obj = json_value.as_object()
        .ok_or_else(|| "Expected object".to_string())?;

    let seed = obj.get("seed")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Missing or invalid seed".to_string())?;

    let segment_length = obj.get("segment_length")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Missing or invalid segment_length".to_string())? as u32;

    let segment_length_mask = obj.get("segment_length_mask")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Missing or invalid segment_length_mask".to_string())? as u32;

    let segment_count_length = obj.get("segment_count_length")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Missing or invalid segment_count_length".to_string())? as u32;

    let fingerprints = obj.get("fingerprints")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Missing or invalid fingerprints".to_string())?;

    // Convert fingerprints to Vec<u16> or Vec<u8> or Vec<u32> depending on filter type
    let fingerprint_values: Vec<u64> = fingerprints.iter()
        .filter_map(|v| v.as_u64())
        .collect();

    // Manually encode as MessagePack array: [seed, segment_length, segment_length_mask, segment_count_length, fingerprints]
    let mut buf = Vec::new();

    // Write array header (5 elements)
    encode::write_array_len(&mut buf, 5)
        .map_err(|e| format!("Failed to write array header: {}", e))?;

    // Write seed (u64)
    encode::write_u64(&mut buf, seed)
        .map_err(|e| format!("Failed to write seed: {}", e))?;

    // Write segment_length (u32)
    encode::write_u32(&mut buf, segment_length)
        .map_err(|e| format!("Failed to write segment_length: {}", e))?;

    // Write segment_length_mask (u32)
    encode::write_u32(&mut buf, segment_length_mask)
        .map_err(|e| format!("Failed to write segment_length_mask: {}", e))?;

    // Write segment_count_length (u32)
    encode::write_u32(&mut buf, segment_count_length)
        .map_err(|e| format!("Failed to write segment_count_length: {}", e))?;

    // Write fingerprints array
    encode::write_array_len(&mut buf, fingerprint_values.len() as u32)
        .map_err(|e| format!("Failed to write fingerprints array header: {}", e))?;

    for fp in fingerprint_values {
        // Write each fingerprint based on its size (u8, u16, or u32)
        if fp <= u8::MAX as u64 {
            encode::write_u8(&mut buf, fp as u8)
                .map_err(|e| format!("Failed to write fingerprint: {}", e))?;
        } else if fp <= u16::MAX as u64 {
            encode::write_u16(&mut buf, fp as u16)
                .map_err(|e| format!("Failed to write fingerprint: {}", e))?;
        } else if fp <= u32::MAX as u64 {
            encode::write_u32(&mut buf, fp as u32)
                .map_err(|e| format!("Failed to write fingerprint: {}", e))?;
        } else {
            encode::write_u64(&mut buf, fp)
                .map_err(|e| format!("Failed to write fingerprint: {}", e))?;
        }
    }

    Ok(buf)
}

/// Enum to hold different filter types for unified handling
enum FilterType {
    Bloom(Bloom<Vec<u8>>),
    Fuse8(BinaryFuse8),
    Fuse16(BinaryFuse16),
    Fuse32(BinaryFuse32),
}

impl FilterType {
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
            FilterType::Fuse8(filter) => serialize_binary_fuse_as_array(filter).unwrap_or_default(),
            FilterType::Fuse16(filter) => serialize_binary_fuse_as_array(filter).unwrap_or_default(),
            FilterType::Fuse32(filter) => serialize_binary_fuse_as_array(filter).unwrap_or_default(),
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

        let body = serde_json::to_vec(&payload)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "max-age=60")
            .body(axum::body::Body::from(body))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    }

    let filter = match build_filter(algorithm, &keys, &fingerprints, fp_rate) {
        Ok(f) => f,
        Err(msg) => {
            let payload = serde_json::json!({
                "error": format!("failed to construct filter: {}", msg),
            });
            let body = serde_json::to_vec(&payload)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body))
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

    let body = serde_json::to_vec(&payload)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "max-age=60")
        .body(axum::body::Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_fuse16_messagepack_serialization() {
        // Create a simple BinaryFuse16 filter with known data
        let keys: Vec<u64> = vec![1, 2, 3, 4, 5];
        let filter = BinaryFuse16::try_from(&keys).expect("Failed to create filter");

        // Serialize using our custom array-based serialization
        let serialized = serialize_binary_fuse_as_array(&filter).expect("Failed to serialize");

        println!("Serialized bytes: {:?}", serialized);
        println!("Serialized length: {}", serialized.len());
        println!("First byte (should be 0x95 for 5-element array): 0x{:02x}", serialized[0]);

        // Verify the first byte is 0x95 (MessagePack fixarray with 5 elements)
        assert_eq!(serialized[0], 0x95, "First byte should be 0x95 (5-element array)");

        // Decode the array manually to verify structure
        let decoded: (u64, u32, u32, u32, Vec<u16>) = rmp_serde::from_slice(&serialized)
            .expect("Failed to deserialize as array");

        println!("Decoded array: seed={}, segment_length={}, segment_length_mask={}, segment_count_length={}, fingerprints.len()={}",
            decoded.0, decoded.1, decoded.2, decoded.3, decoded.4.len());

        // Verify we have data
        assert!(decoded.4.len() > 0, "Should have fingerprints");
        assert!(serialized.len() > 0, "Serialized data should not be empty");
    }
}
