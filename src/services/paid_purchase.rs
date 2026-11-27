use axum::http::HeaderMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    error::{AppError, AppResult},
    models::AppState,
    services::{
        cashu, file_storage,
        paid_store::{Begin, OpState, PurchaseTerms},
    },
};

const PERIOD: u64 = 30 * 24 * 60 * 60;
const MAX_PERIOD: u64 = 365 * 24 * 60 * 60;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn target(headers: &HeaderMap, now: u64) -> AppResult<u64> {
    let target = match headers.get("X-Expiration") {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AppError::BadRequest("Invalid X-Expiration".to_owned()))?,
        None => now.saturating_add(PERIOD),
    };
    if target <= now || target > now.saturating_add(MAX_PERIOD) {
        return Err(AppError::BadRequest(
            "X-Expiration must be within the next year".to_owned(),
        ));
    }
    Ok(target)
}

pub fn quote(
    state: &AppState,
    sha256: &str,
    size: u64,
    until: u64,
    now: u64,
) -> AppResult<(u64, u64)> {
    let store = state
        .paid_store
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Paid retention is unavailable".to_owned()))?;
    let from = store.paid_until(sha256).unwrap_or(now).max(now);
    if until <= from {
        return Ok((from, 0));
    }
    let periods = (until - from).div_ceil(PERIOD);
    let mib = size.div_ceil(1024 * 1024).max(1);
    let amount = mib
        .checked_mul(periods)
        .and_then(|n| n.checked_mul(state.cashu_price_per_mb))
        .ok_or_else(|| AppError::BadRequest("Price overflow".to_owned()))?
        .max(1);
    Ok((from, amount))
}

pub fn payment_required(state: &AppState, amount: u64) -> AppError {
    AppError::PaymentRequired {
        amount_sats: amount,
        unit: "sat".to_owned(),
        mints: state.cashu_accepted_mints.clone(),
    }
}

/// Record the immutable purchase before touching the mint. Retried proofs locate
/// the original operation, not a newly computed relative deadline.
pub async fn purchase(
    state: &AppState,
    headers: &HeaderMap,
    sha256: &str,
    size: u64,
    until: u64,
    require_indexed: bool,
) -> AppResult<String> {
    let now = now();
    let store = state
        .paid_store
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Paid retention is unavailable".to_owned()))?;
    let wallet = state
        .cashu_wallet
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Wallet is unavailable".to_owned()))?;
    let raw = cashu::extract_cashu_header(headers);
    let (from, amount) = quote(state, sha256, size, until, now)?;
    let Some(raw) = raw else {
        return Err(payment_required(state, amount));
    };
    let token = cashu::parse_token(&raw)?;
    let id = cashu::payment_id(&token)?;
    if let Some(operation) = store.operation(&id).await? {
        let target_mismatch =
            headers.contains_key("X-Expiration") && operation.terms.paid_until != until;
        if operation.terms.sha256 != sha256 || operation.terms.size != size || target_mismatch {
            return Err(AppError::Conflict(
                "Payment already belongs to another purchase".to_owned(),
            ));
        }
        match operation.state {
            OpState::Applied | OpState::Settled => return Ok(id),
            OpState::Failed => return Err(AppError::BadRequest("Payment was rejected".to_owned())),
            OpState::Pending => {
                if let Some(received) = cashu::find_payment(wallet, &id).await? {
                    if received < operation.terms.amount {
                        return Err(AppError::Conflict(
                            "Settled payment is insufficient; operator reconciliation required"
                                .to_owned(),
                        ));
                    }
                    store.mark_settled(&id, Some(id.clone()), now).await?;
                    return Ok(id);
                }
                return Err(AppError::ServiceUnavailable(
                    "Payment outcome pending; retry the same token later".to_owned(),
                ));
            }
        }
    }
    if amount == 0 {
        return Err(AppError::Conflict(
            "Blob is already retained through that time".to_owned(),
        ));
    }
    cashu::verify_token_basics(&token, amount, &state.cashu_accepted_mints)?;
    let mint_url = state
        .cashu_accepted_mints
        .first()
        .cloned()
        .ok_or_else(|| AppError::ServiceUnavailable("Mint unavailable".to_owned()))?;
    let terms = PurchaseTerms {
        op_id: id.clone(),
        sha256: sha256.to_owned(),
        size,
        paid_from: from,
        paid_until: until,
        amount,
        unit: "sat".to_owned(),
        mint_url,
    };
    match file_storage::begin_paid_operation(state, terms, require_indexed, now).await? {
        Begin::Existing(_) => {
            return Err(AppError::Conflict(
                "Purchase already in progress; retry".to_owned(),
            ))
        }
        Begin::Started => {}
    }
    match cashu::receive_for_purchase(wallet, &token, amount).await {
        Ok(received) if received >= amount => {
            store.mark_settled(&id, Some(id.clone()), now).await?;
            Ok(id)
        }
        Ok(_) => Err(AppError::Conflict(
            "Settled payment is insufficient; operator reconciliation required".to_owned(),
        )),
        Err(error @ AppError::BadRequest(_)) => {
            store.mark_failed(&id, now).await?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
