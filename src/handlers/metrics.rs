use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use prometheus::{Encoder, TextEncoder};

use crate::models::AppState;

/// Handle Prometheus metrics requests
pub async fn get_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = state.metrics_bearer_token.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|provided| provided == token);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Update the metrics before gathering them
    let () = state.get_stats().await;

    // Gather all metrics from the registry
    let metric_families = state.metrics.registry.gather();

    // Encode metrics in Prometheus text format
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(()) => {
            // Return the metrics with appropriate content type
            (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )],
                buffer,
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode metrics",
            )
                .into_response()
        }
    }
}
