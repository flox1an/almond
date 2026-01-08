use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use std::fmt;

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
    PayloadTooLarge(String),
    
    /// Resource errors
    NotFound(String),
    Conflict(String),
    
    /// I/O errors
    IoError(String),
    
    /// Network/upstream errors
    NetworkError(String),
    Timeout(String),
    BadGateway(String),
    
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
            AppError::PayloadTooLarge(msg) => write!(f, "Payload too large: {}", msg),
            AppError::NotFound(msg) => write!(f, "Not found: {}", msg),
            AppError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            AppError::IoError(msg) => write!(f, "I/O error: {}", msg),
            AppError::NetworkError(msg) => write!(f, "Network error: {}", msg),
            AppError::Timeout(msg) => write!(f, "Timeout: {}", msg),
            AppError::BadGateway(msg) => write!(f, "Bad gateway: {}", msg),
            AppError::InternalError(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Handle PaymentRequired specially with X-Cashu header
        if let AppError::PaymentRequired {
            amount_sats,
            unit,
            mints,
        } = &self
        {
            // Build NUT-18 payment request
            let payment_request = serde_json::json!({
                "a": amount_sats,
                "u": unit,
                "m": mints,
            });

            // Encode as cashuA (base64url of JSON)
            let encoded = format!("cashuA{}", URL_SAFE.encode(payment_request.to_string()));

            return Response::builder()
                .status(StatusCode::PAYMENT_REQUIRED)
                .header("X-Cashu", encoded)
                .body(Body::from(format!("Payment required: {} {}", amount_sats, unit)))
                .expect("Failed to build payment required response")
                .into_response();
        }

        let status = match &self {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            // Note: PaymentRequired is handled with early return above, this arm is kept for exhaustiveness
            AppError::PaymentRequired { .. } => StatusCode::PAYMENT_REQUIRED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NetworkError(_) => StatusCode::BAD_REQUEST,
            AppError::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = self.to_string();

        (status, body).into_response()
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

/// Convert AppError to StatusCode for backward compatibility
impl From<AppError> for StatusCode {
    fn from(err: AppError) -> Self {
        match err {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::PaymentRequired { .. } => StatusCode::PAYMENT_REQUIRED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NetworkError(_) => StatusCode::BAD_REQUEST,
            AppError::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
