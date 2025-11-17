use axum::{
    extract::State,
    response::{IntoResponse, Response},
    http::{StatusCode, header},
};
use prometheus::{Encoder, TextEncoder};

use crate::models::AppState;

/// Handle Prometheus metrics requests
pub async fn get_metrics(State(state): State<AppState>) -> Response {
    // Update the metrics before gathering them
    let _ = state.get_stats().await;

    // Gather all metrics from the registry
    let metric_families = state.metrics_registry.gather();

    // Encode metrics in Prometheus text format
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => {
            // Return the metrics with appropriate content type
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                buffer
            ).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode metrics"
            ).into_response()
        }
    }
}
