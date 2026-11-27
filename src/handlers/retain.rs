use crate::{
    error::AppError,
    models::AppState,
    services::{file_storage, paid_purchase},
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub async fn retain(
    State(state): State<AppState>,
    Path(sha256): Path<String>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> Result<Response, AppError> {
    file_storage::validate_sha256_format(&sha256)?;
    if !state.feature_paid_upload {
        return Err(AppError::Forbidden("Paid retention is disabled".to_owned()));
    }
    let metadata = state
        .file_index
        .get(&sha256)
        .await
        .ok_or_else(|| AppError::NotFound("Blob not found".to_owned()))?;
    let now = paid_purchase::now();
    let until = paid_purchase::target(&headers, now)?;
    let (_, amount) = paid_purchase::quote(&state, &sha256, metadata.size, until, now)?;
    if method == axum::http::Method::HEAD {
        if amount == 0 {
            return Ok(axum::http::StatusCode::OK.into_response());
        }
        return Err(paid_purchase::payment_required(&state, amount));
    }
    let operation =
        paid_purchase::purchase(&state, &headers, &sha256, metadata.size, until, true).await?;
    let paid_until = file_storage::apply_paid_operation(&state, &operation, now).await?;
    Ok(Json(json!({"sha256": sha256, "paid_until": paid_until})).into_response())
}
