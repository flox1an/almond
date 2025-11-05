use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::models::{AppState, ListQuery};

/// Handle list requests (supports both /list and /list/:id routes)
pub async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
    headers: HeaderMap,
    req: Request,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Check if list feature is enabled
    if !state.feature_list_enabled {
        warn!("List feature is disabled");
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Log request details for debugging
    let method = req.method();
    let uri = req.uri();
    let path = uri.path();
    
    // Extract ID from path if present (for /list/:id routes)
    let id_from_path = if path.starts_with("/list/") && path.len() > 6 {
        let potential_id = &path[6..]; // Skip "/list/"
        // Check if it looks like a SHA256 hash (64 hex chars) or other ID
        if !potential_id.contains('/') && !potential_id.contains('?') {
            Some(potential_id.to_string())
        } else {
            None
        }
    } else {
        None
    };
    
    if let Some(id) = &id_from_path {
        info!("📋 LIST request with ID: {} {} (id: {}, query: {:?})", method, uri, id, params);
        debug!("📋 Path parameter (id): {} (length: {} chars)", id, id.len());
        
        // If an ID is provided in the path, we could filter by it in the future
        // For now, we'll just log it and continue with normal listing
        // TODO: Consider filtering results by this ID if it's a SHA256 hash
    } else {
        info!("📋 LIST request: {} {} (query: {:?})", method, uri, params);
    }

    // Log request origin and referer for CORS debugging
    if let Some(origin) = headers.get(header::ORIGIN) {
        if let Ok(origin_str) = origin.to_str() {
            info!("🌐 Request origin: {}", origin_str);
        }
    }
    if let Some(referer) = headers.get(header::REFERER) {
        if let Ok(referer_str) = referer.to_str() {
            debug!("🔗 Request referer: {}", referer_str);
        }
    }

    // Log all headers for debugging
    debug!("📋 Request headers:");
    for (name, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            // Mask sensitive Authorization header content for logging
            if name == header::AUTHORIZATION {
                let masked = if value_str.len() > 20 {
                    format!("{}...{}", &value_str[..10], &value_str[value_str.len()-10..])
                } else {
                    "***".to_string()
                };
                debug!("  {}: {}", name, masked);
            } else {
                debug!("  {}: {}", name, value_str);
            }
        } else {
            debug!("  {}: <binary>", name);
        }
    }

    // Check for Authorization header and log details
    let auth_header = headers.get(header::AUTHORIZATION);
    match auth_header {
        Some(auth) => {
            info!("🔐 Authorization header present");
            match auth.to_str() {
                Ok(auth_str) => {
                    debug!("🔐 Authorization header length: {} chars", auth_str.len());
                    
                    // Check if it starts with "Nostr "
                    if let Some(base64_part) = auth_str.strip_prefix("Nostr ") {
                        info!("✅ Authorization header has 'Nostr ' prefix");
                        debug!("🔐 Base64 part length: {} chars", base64_part.len());
                        
                        // Try to decode base64 to see if it's valid
                        if let Ok(decoded) = STANDARD.decode(base64_part) {
                            debug!("✅ Base64 decoding successful, {} bytes decoded", decoded.len());
                            
                            // Try to parse as JSON
                            match String::from_utf8(decoded) {
                                Ok(json_str) => {
                                    debug!("✅ UTF-8 conversion successful, JSON length: {} chars", json_str.len());
                                    debug!("🔐 JSON preview (first 200 chars): {}", 
                                           if json_str.len() > 200 { 
                                               format!("{}...", &json_str[..200]) 
                                           } else { 
                                               json_str.clone() 
                                           });
                                    
                                    // Try to parse as Nostr event
                                    match serde_json::from_str::<serde_json::Value>(&json_str) {
                                        Ok(json_value) => {
                                            debug!("✅ JSON parsing successful");
                                            if let Some(kind) = json_value.get("kind") {
                                                debug!("🔐 Event kind: {}", kind);
                                            }
                                            if let Some(pubkey) = json_value.get("pubkey") {
                                                debug!("🔐 Event pubkey: {}", pubkey);
                                            }
                                            if let Some(id) = json_value.get("id") {
                                                debug!("🔐 Event id: {}", id);
                                            }
                                            if let Some(sig) = json_value.get("sig") {
                                                let sig_str = sig.as_str().unwrap_or("");
                                                debug!("🔐 Event signature: {}...", 
                                                       if sig_str.len() > 20 { 
                                                           &sig_str[..20] 
                                                       } else { 
                                                           sig_str 
                                                       });
                                            }
                                        }
                                        Err(e) => {
                                            warn!("⚠️  JSON parsing failed: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("⚠️  UTF-8 conversion failed: {}", e);
                                }
                            }
                        } else {
                            warn!("⚠️  Base64 decoding failed");
                        }
                    } else {
                        warn!("⚠️  Authorization header does NOT start with 'Nostr ' prefix. Got: '{}'", 
                              if auth_str.len() > 20 { 
                                  format!("{}...", &auth_str[..20]) 
                              } else { 
                                  auth_str.to_string() 
                              });
                    }
                }
                Err(e) => {
                    warn!("⚠️  Authorization header is not valid UTF-8: {}", e);
                }
            }
        }
        None => {
            info!("ℹ️  No Authorization header present (list endpoint does not require auth)");
        }
    }

    // Validate query parameters according to BUD-02
    // since and until are unix timestamps to filter by uploaded date
    let since = params.since.unwrap_or(0);
    let until = params.until.unwrap_or(u64::MAX);
    info!("📋 Query parameters: since={} (unix timestamp), until={} (unix timestamp)", since, until);

    let file_index = state.file_index.read().await;
    let total_files = file_index.len();
    info!("📋 Total files in index: {}", total_files);
    
    // Build list of blobs with correct format and filter by since/until
    let mut blobs: Vec<serde_json::Value> = file_index
        .iter()
        .filter_map(|(sha256, metadata)| {
            // Filter by uploaded timestamp (created_at)
            if metadata.created_at >= since && metadata.created_at <= until {
                // Build URL: {public_url}/{sha256}.{extension} or {public_url}/{sha256}
                let url = match &metadata.extension {
                    Some(ext) => format!("{}/{}.{}", state.public_url, sha256, ext),
                    None => format!("{}/{}", state.public_url, sha256),
                };
                
                let mut blob = json!({
                    "url": url,
                    "sha256": sha256,
                    "size": metadata.size,
                    "type": metadata.mime_type.as_ref().unwrap_or(&"application/octet-stream".to_string()),
                    "uploaded": metadata.created_at
                });

                // Add expiration if present
                if let Some(expiration) = metadata.expiration {
                    if let Some(obj) = blob.as_object_mut() {
                        obj.insert("expiration".to_string(), json!(expiration));
                    }
                }

                Some(blob)
            } else {
                None
            }
        })
        .collect();

    info!("📋 Collected {} files from index (filtered by since/until)", blobs.len());

    // Sort by uploaded descending (newest first)
    blobs.sort_by(|a, b| {
        let a_time = a["uploaded"].as_u64().unwrap_or(0);
        let b_time = b["uploaded"].as_u64().unwrap_or(0);
        b_time.cmp(&a_time)
    });

    info!("📋 Files sorted by uploaded (newest first)");

    let paginated_blobs = blobs;

    info!("✅ LIST request completed successfully: {} blobs returned", paginated_blobs.len());

    // Return just the array of blobs
    Ok(Json(serde_json::Value::Array(paginated_blobs)))
}
