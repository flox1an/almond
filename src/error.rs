use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::fmt;

/// NUT-24 challenge for the `X-Cashu` header: a NUT-18 `creqA` payment request
/// carrying only amount, unit and mints (in-band payment, so no transport).
#[cfg(feature = "cashu")]
fn nut24_challenge(amount_sats: u64, unit: &str, mints: &[String]) -> Result<String, String> {
    use std::{fmt::Write, str::FromStr};
    let mints = mints
        .iter()
        .map(|mint| cashu::MintUrl::from_str(mint).map_err(|e| format!("mint {mint}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let unit = cashu::nuts::CurrencyUnit::from_str(unit).map_err(|e| e.to_string())?;
    let request = cashu::nuts::nut18::PaymentRequest::builder()
        .amount(amount_sats)
        .unit(unit)
        .mints(mints)
        .build();
    // `to_string` would panic if CBOR encoding failed; surface it instead.
    let mut encoded = String::new();
    write!(encoded, "{request}").map_err(|e| e.to_string())?;
    Ok(encoded)
}

/// Custom error type for the application
#[derive(Debug)]
pub enum AppError {
    /// Authentication/authorization errors
    Unauthorized(String),
    Forbidden(String),
    /// Payment required (402)
    PaymentRequired {
        amount_sats: u64,
        unit: String,
        mints: Vec<String>,
    },

    /// Validation errors
    BadRequest(String),
    LengthRequired(String),
    PayloadTooLarge(String),

    /// Resource errors
    NotFound(String),
    Conflict(String),
    InsufficientStorage(String),

    /// I/O errors
    IoError(String),

    /// Network/upstream errors
    NetworkError(String),
    Timeout(String),
    BadGateway(String),
    ServiceUnavailable(String),

    /// Internal errors
    InternalError(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            AppError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            AppError::PaymentRequired {
                amount_sats,
                unit,
                mints,
            } => {
                write!(
                    f,
                    "Payment required: {} {} (mints: {})",
                    amount_sats,
                    unit,
                    mints.join(", ")
                )
            }
            AppError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            AppError::LengthRequired(msg) => write!(f, "Length required: {}", msg),
            AppError::PayloadTooLarge(msg) => write!(f, "Payload too large: {}", msg),
            AppError::NotFound(msg) => write!(f, "Not found: {}", msg),
            AppError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            AppError::InsufficientStorage(msg) => write!(f, "Insufficient storage: {}", msg),
            AppError::IoError(msg) => write!(f, "I/O error: {}", msg),
            AppError::NetworkError(msg) => write!(f, "Network error: {}", msg),
            AppError::Timeout(msg) => write!(f, "Timeout: {}", msg),
            AppError::BadGateway(msg) => write!(f, "Bad gateway: {}", msg),
            AppError::ServiceUnavailable(msg) => write!(f, "Service unavailable: {}", msg),
            AppError::InternalError(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Handle PaymentRequired specially with the NUT-24 X-Cashu challenge
        if let AppError::PaymentRequired {
            amount_sats,
            unit,
            mints,
        } = &self
        {
            let reason = self.to_string();
            #[cfg_attr(not(feature = "cashu"), allow(unused_mut))]
            let mut response = Response::builder()
                .status(StatusCode::PAYMENT_REQUIRED)
                .header("X-Reason", &reason);
            // Without the `cashu` feature no operation can be paid
            // (`Config::validate`), so there is no challenge to encode.
            #[cfg(feature = "cashu")]
            match nut24_challenge(*amount_sats, unit, mints) {
                Ok(challenge) => response = response.header("X-Cashu", challenge),
                Err(error) => {
                    tracing::error!("Failed to encode NUT-24 payment request: {error}");
                    return AppError::InternalError("Failed to encode payment request".to_string())
                        .into_response();
                }
            }
            #[cfg(not(feature = "cashu"))]
            let _ = mints;
            return response
                .body(Body::from(format!(
                    "Payment required: {} {}",
                    amount_sats, unit
                )))
                .expect("Failed to build payment required response")
                .into_response();
        }

        let status = match &self {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::PaymentRequired { .. } => StatusCode::PAYMENT_REQUIRED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::LengthRequired(_) => StatusCode::LENGTH_REQUIRED,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::InsufficientStorage(_) => StatusCode::INSUFFICIENT_STORAGE,
            AppError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NetworkError(_) => StatusCode::BAD_GATEWAY,
            AppError::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = self.to_string();

        Response::builder()
            .status(status)
            .header("X-Reason", body.as_str())
            .body(Body::from(body))
            .expect("building an error response cannot fail")
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::IoError(err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        AppError::BadRequest(format!("JSON error: {}", err))
    }
}

/// Convert `AppError` to `StatusCode` for backward compatibility
impl From<AppError> for StatusCode {
    fn from(err: AppError) -> Self {
        match err {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::PaymentRequired { .. } => StatusCode::PAYMENT_REQUIRED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::LengthRequired(_) => StatusCode::LENGTH_REQUIRED,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::InsufficientStorage(_) => StatusCode::INSUFFICIENT_STORAGE,
            AppError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NetworkError(_) => StatusCode::BAD_GATEWAY,
            AppError::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(all(test, feature = "cashu"))]
mod tests {
    use std::str::FromStr;

    #[test]
    fn payment_required_carries_a_decodable_nut24_creq_a() {
        let response =
            axum::response::IntoResponse::into_response(super::AppError::PaymentRequired {
                amount_sats: 21,
                unit: "sat".to_string(),
                mints: vec!["https://mint.example.com".to_string()],
            });
        assert_eq!(response.status(), axum::http::StatusCode::PAYMENT_REQUIRED);
        let header = response.headers()["X-Cashu"].to_str().unwrap();
        assert!(header.starts_with("creqA"));
        let request = cashu::nuts::nut18::PaymentRequest::from_str(header).unwrap();
        assert_eq!(request.amount, Some(cashu::Amount::from(21)));
        assert_eq!(request.unit, Some(cashu::nuts::CurrencyUnit::Sat));
        assert_eq!(
            request.mints,
            vec![cashu::MintUrl::from_str("https://mint.example.com").unwrap()]
        );
        assert!(request.transports.is_empty());
    }
}
