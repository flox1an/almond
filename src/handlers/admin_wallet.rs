use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{error::AppError, models::AppState, services::cashu};

fn no_store(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}

pub async fn balance(State(state): State<AppState>) -> Result<Response, AppError> {
    let wallet = state
        .cashu_wallet
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Wallet unavailable".to_owned()))?;
    let balance = cashu::wallet_balance(wallet).await?;
    Ok(no_store(Json(json!({
        "balance": balance.spendable_sats,
        "pending": balance.pending_sats,
        "reserved": balance.reserved_sats,
        "mint": state.cashu_accepted_mints.first(),
    }))))
}

#[derive(Deserialize)]
pub struct WithdrawRequest {
    amount: u64,
    request_id: String,
}

fn withdrawal_response(withdrawal: cashu::Withdrawal) -> Response {
    no_store(Json(json!({
        "id": withdrawal.request_id,
        "operation_id": withdrawal.operation_id,
        "amount": withdrawal.amount_sats,
        "fee": withdrawal.fee_sats,
        "token": withdrawal.token,
        "status": withdrawal.state,
    })))
}

pub async fn withdraw(
    State(state): State<AppState>,
    Json(request): Json<WithdrawRequest>,
) -> Result<Response, AppError> {
    if request.amount == 0 {
        return Err(AppError::BadRequest("Amount must be positive".to_owned()));
    }
    let wallet = state
        .cashu_wallet
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Wallet unavailable".to_owned()))?;
    let withdrawal = cashu::withdraw(wallet, &request.request_id, request.amount).await?;
    Ok(withdrawal_response(withdrawal))
}

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let wallet = state
        .cashu_wallet
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Wallet unavailable".to_owned()))?;
    let withdrawal = cashu::withdrawal(wallet, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Withdrawal not found".to_owned()))?;
    Ok(withdrawal_response(withdrawal))
}

pub async fn reclaim(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let wallet = state
        .cashu_wallet
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Wallet unavailable".to_owned()))?;
    let amount = cashu::reclaim_withdrawal(wallet, &id).await?;
    Ok(no_store((StatusCode::OK, Json(json!({"amount": amount})))))
}
