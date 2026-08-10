//! Cashu payment processing service (BUD-07)
//!
//! Handles token verification and wallet operations for paid uploads/downloads.
//! Implements Cashu ecash token parsing, validation, and receiving.

use crate::error::AppError;
use cashu::nuts::nut00::token::Token;
use cdk::wallet::Wallet as CdkWallet;
use cdk_sqlite::wallet::WalletSqliteDatabase;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// A request that money can be demanded for.
///
/// Naming the operation keeps the feature gate in one place: every caller used
/// to read its own `feature_paid_*` flag and then re-spell the settlement
/// sequence, and the spellings drifted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaidOperation {
    Upload,
    Mirror,
    Download,
}

impl PaidOperation {
    fn is_paid(self, state: &crate::models::AppState) -> bool {
        match self {
            Self::Upload => state.feature_paid_upload,
            Self::Mirror => state.feature_paid_mirror,
            Self::Download => state.feature_paid_download,
        }
    }
}

/// What this server would charge for `size_bytes`, in the BUD-07 shape.
///
/// Price discovery (`HEAD /upload`) and the 402 response are rendered from the
/// same value, so a client cannot derive a price the server disagrees with.
pub struct Quote {
    pub amount_sats: u64,
    pub unit: &'static str,
    pub mints: Vec<String>,
}

/// Quote `size_bytes` against the configured price.
///
/// Takes the two configured values rather than the whole runtime state: a
/// price is arithmetic, and keeping it that way makes it directly testable.
#[must_use]
pub fn quote(price_per_mb: u64, mints: &[String], size_bytes: u64) -> Quote {
    Quote {
        amount_sats: calculate_price(size_bytes, price_per_mb),
        unit: "sat",
        mints: mints.to_vec(),
    }
}

impl Quote {
    /// The 402 that asks a client to pay this quote.
    fn payment_required(self) -> AppError {
        AppError::PaymentRequired {
            amount_sats: self.amount_sats,
            unit: self.unit.to_string(),
            mints: self.mints,
        }
    }
}

/// Settle payment for one operation over `size_bytes`.
///
/// Owns the whole BUD-07 dance: the feature gate, price derivation, header
/// extraction, mint validation, settlement, and the 402 shape. Returns `Ok`
/// when the operation is free, or when the mint settled at least the quoted
/// amount into the wallet.
///
/// Both reactive (402, then retry with `X-Cashu`) and preemptive (`X-Cashu` on
/// the first request) flows land here; they differ only in whether the client
/// has already seen the 402.
pub async fn charge(
    state: &crate::models::AppState,
    headers: &axum::http::HeaderMap,
    operation: PaidOperation,
    size_bytes: u64,
) -> Result<(), AppError> {
    if !operation.is_paid(state) {
        return Ok(());
    }

    let quoted = quote(
        state.cashu_price_per_mb,
        &state.cashu_accepted_mints,
        size_bytes,
    );
    let Some(token_str) = extract_cashu_header(headers) else {
        return Err(quoted.payment_required());
    };

    let token = parse_token(&token_str)?;
    verify_token_basics(&token, quoted.amount_sats, &state.cashu_accepted_mints)?;

    // A configured paid feature without a wallet cannot take money, so it must
    // not hand out the blob either.
    let Some(wallet) = &state.cashu_wallet else {
        return Err(AppError::ServiceUnavailable(
            "Payment service is not initialized".to_string(),
        ));
    };

    // The mint decides what actually settles; a token that verifies can still
    // pay short, and short payment must not buy the operation.
    let received_sats = receive_token(wallet, &token).await?;
    if received_sats < quoted.amount_sats {
        warn!(
            required = quoted.amount_sats,
            received = received_sats,
            ?operation,
            "Mint settled below the quoted amount"
        );
        return Err(quoted.payment_required());
    }

    Ok(())
}

/// Calculate required payment in sats for a given size
///
/// Rounds up to the nearest MB, with a minimum charge of 1 sat.
///
/// # Arguments
/// * `size_bytes` - The size of the file in bytes
/// * `price_per_mb` - The price per megabyte in satoshis
///
/// # Returns
/// The required payment amount in satoshis
#[must_use]
pub fn calculate_price(size_bytes: u64, price_per_mb: u64) -> u64 {
    // Round up to nearest MB, minimum 1 sat
    let size_mb = size_bytes.div_ceil(1024 * 1024);
    std::cmp::max(1, size_mb.saturating_mul(price_per_mb))
}

/// Parse a Cashu token from the X-Cashu header value
///
/// Supports both cashuA (base64 JSON) and cashuB (binary CBOR) token formats.
///
/// # Arguments
/// * `token_str` - The token string from the X-Cashu header
///
/// # Returns
/// The parsed Token on success, or an error if parsing fails
pub fn parse_token(token_str: &str) -> Result<Token, AppError> {
    let token_str = token_str.trim();

    // Token should be cashuA or cashuB prefixed
    if !token_str.starts_with("cashuA") && !token_str.starts_with("cashuB") {
        return Err(AppError::BadRequest(
            "Invalid token format: must start with cashuA or cashuB".to_string(),
        ));
    }

    Token::from_str(token_str).map_err(|e| {
        warn!("Failed to parse Cashu token: {}", e);
        AppError::BadRequest(format!("Invalid Cashu token: {}", e))
    })
}

/// Verify token is from an accepted mint and has sufficient amount
///
/// # Arguments
/// * `token` - The parsed Cashu token
/// * `required_amount` - The minimum amount required in satoshis
/// * `accepted_mints` - List of accepted mint URLs
///
/// # Returns
/// Ok(()) if the token is valid, or an error describing the issue
pub fn verify_token_basics(
    token: &Token,
    required_amount: u64,
    accepted_mints: &[String],
) -> Result<(), AppError> {
    // Get token mint URL
    let mint_url = token.mint_url().map_err(|e| {
        warn!("Failed to get mint URL from token: {}", e);
        AppError::BadRequest(format!("Invalid token: could not extract mint URL: {}", e))
    })?;

    let mint_url_str = mint_url.to_string();

    // Check mint is in accepted list
    // Normalize URLs for comparison (remove trailing slashes)
    let normalized_token_mint = mint_url_str.trim_end_matches('/');
    let mint_accepted = accepted_mints.iter().any(|accepted| {
        let normalized_accepted = accepted.trim_end_matches('/');
        normalized_token_mint.eq_ignore_ascii_case(normalized_accepted)
    });

    if !mint_accepted {
        warn!(
            "Token mint {} not in accepted list: {:?}",
            mint_url_str, accepted_mints
        );
        return Err(AppError::BadRequest(format!(
            "Mint {} is not accepted. Accepted mints: {:?}",
            mint_url_str, accepted_mints
        )));
    }

    // Check amount is sufficient
    let token_amount = token.value().map_err(|e| {
        warn!("Failed to get token value: {}", e);
        AppError::BadRequest(format!("Invalid token: could not calculate value: {}", e))
    })?;

    // Convert Amount to u64 for comparison
    let token_sats: u64 = token_amount.into();

    if token_sats < required_amount {
        warn!(
            "Token amount {} sats is less than required {} sats",
            token_sats, required_amount
        );
        return Err(AppError::BadRequest(format!(
            "Insufficient payment: token contains {} sats but {} sats required",
            token_sats, required_amount
        )));
    }

    info!(
        "Token verified: {} sats from mint {} (required: {} sats)",
        token_sats, mint_url_str, required_amount
    );

    Ok(())
}

/// Persist a newly generated wallet seed without ever exposing it under
/// process-default permissions.  The rename makes a completed seed appear
/// atomically to another process.
fn write_seed_0600(path: &std::path::Path, seed: &[u8; 64]) -> Result<(), AppError> {
    let temporary = path.with_extension(format!("seed.tmp.{}", uuid::Uuid::new_v4()));
    #[cfg(unix)]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| {
            AppError::InternalError(format!("Failed to create wallet seed: {error}"))
        })?;
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            AppError::InternalError(format!("Failed to create wallet seed: {error}"))
        })?;
    file.write_all(seed)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            AppError::InternalError(format!("Failed to write wallet seed: {error}"))
        })?;
    std::fs::rename(&temporary, path).map_err(|error| {
        AppError::InternalError(format!("Failed to install wallet seed: {error}"))
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        AppError::InternalError(format!("Failed to secure wallet seed: {error}"))
    })?;
    Ok(())
}

/// Initialize the CDK wallet for receiving payments
///
/// Creates or opens a SQLite-backed wallet configured for the first accepted mint.
/// The wallet is used to swap incoming tokens for fresh proofs.
///
/// # Arguments
/// * `wallet_path` - Path to the `SQLite` database file
/// * `accepted_mints` - List of accepted mint URLs (first one is used for wallet)
///
/// # Returns
/// An Arc-wrapped RwLock-protected wallet instance, or an error if initialization fails
pub async fn init_wallet(
    wallet_path: &PathBuf,
    accepted_mints: &[String],
) -> Result<Arc<RwLock<CdkWallet>>, AppError> {
    if accepted_mints.is_empty() {
        return Err(AppError::InternalError(
            "No accepted mints configured".to_string(),
        ));
    }

    let mint_url_str = &accepted_mints[0];

    info!(
        "Initializing Cashu wallet at {:?} for mint {}",
        wallet_path, mint_url_str
    );

    // Create SQLite database for wallet storage
    let localstore = WalletSqliteDatabase::new(wallet_path).await.map_err(|e| {
        error!("Failed to create wallet database: {}", e);
        AppError::InternalError(format!("Failed to initialize wallet database: {}", e))
    })?;

    // Seed file path (alongside wallet db)
    let seed_path = wallet_path.with_extension("seed");

    let seed: [u8; 64] = if seed_path.exists() {
        #[cfg(unix)]
        {
            std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| AppError::InternalError(format!("Failed to secure wallet seed: {error}")),
            )?;
        }
        let seed_bytes = std::fs::read(&seed_path)
            .map_err(|e| AppError::InternalError(format!("Failed to read wallet seed: {}", e)))?;
        if seed_bytes.len() != 64 {
            return Err(AppError::InternalError(
                "Invalid seed file size".to_string(),
            ));
        }
        let mut seed = [0u8; 64];
        seed.copy_from_slice(&seed_bytes);
        seed
    } else {
        let mut seed = [0u8; 64];
        rand::fill(&mut seed);
        write_seed_0600(&seed_path, &seed)?;
        seed
    };

    // Create the wallet
    // Unit is satoshis (sat)
    let unit = cashu::nuts::nut00::CurrencyUnit::Sat;
    let wallet =
        CdkWallet::new(mint_url_str, unit, Arc::new(localstore), seed, None).map_err(|e| {
            error!("Failed to create wallet: {}", e);
            AppError::InternalError(format!("Failed to create wallet: {}", e))
        })?;

    info!("Cashu wallet initialized successfully");

    Ok(Arc::new(RwLock::new(wallet)))
}

/// Receive a token into the wallet (swap to own proofs)
///
/// This swaps the incoming token proofs for fresh proofs owned by the wallet.
/// This is the recommended way to accept payments as it ensures the proofs
/// cannot be double-spent by the sender.
///
/// # Arguments
/// * `wallet` - The CDK wallet instance
/// * `token` - The Cashu token to receive
///
/// # Returns
/// The amount received in satoshis, or an error if the swap fails
pub async fn receive_token(
    wallet: &Arc<RwLock<CdkWallet>>,
    token: &Token,
) -> Result<u64, AppError> {
    let wallet_guard = wallet.read().await;

    // Use the wallet's receive method to swap the token
    let receive_options = cdk::wallet::ReceiveOptions::default();
    let amount = wallet_guard
        .receive(&token.to_string(), receive_options)
        .await
        .map_err(|e| {
            error!("Failed to receive token: {}", e);
            AppError::BadGateway("Cashu wallet settlement failed".to_string())
        })?;

    let amount_sats: u64 = amount.into();
    info!("Received {} sats into wallet", amount_sats);

    Ok(amount_sats)
}

/// Extract X-Cashu header from request headers
///
/// Handles both X-Cashu and x-cashu (case-insensitive) header names.
///
/// # Arguments
/// * `headers` - The request headers
///
/// # Returns
/// The token string if the header is present and valid UTF-8, None otherwise
#[must_use]
pub fn extract_cashu_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("X-Cashu")
        .or_else(|| headers.get("x-cashu"))
        .and_then(|h| h.to_str().ok())
        .map(ToString::to_string)
}

/// Get the total amount from a token
///
/// # Arguments
/// * `token` - The parsed Cashu token
///
/// # Returns
/// The total value in satoshis, or an error if calculation fails
pub fn get_token_amount(token: &Token) -> Result<u64, AppError> {
    let amount = token.value().map_err(|e| {
        warn!("Failed to get token value: {}", e);
        AppError::BadRequest(format!("Invalid token: could not calculate value: {}", e))
    })?;

    Ok(amount.into())
}

/// Get the mint URL from a token
///
/// # Arguments
/// * `token` - The parsed Cashu token
///
/// # Returns
/// The mint URL as a string, or an error if extraction fails
pub fn get_token_mint(token: &Token) -> Result<String, AppError> {
    let mint_url = token.mint_url().map_err(|e| {
        warn!("Failed to get mint URL from token: {}", e);
        AppError::BadRequest(format!("Invalid token: could not extract mint URL: {}", e))
    })?;
    Ok(mint_url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mints() -> Vec<String> {
        vec![
            "https://mint.example.com".to_string(),
            "https://other.example.com".to_string(),
        ]
    }

    #[test]
    fn quote_rounds_up_to_the_next_megabyte() {
        // One byte over a megabyte is charged as two.
        let quoted = quote(7, &mints(), 1024 * 1024 + 1);
        assert_eq!(quoted.amount_sats, 14);
        assert_eq!(quoted.unit, "sat");
        assert_eq!(quoted.mints, mints());
    }

    #[test]
    fn quote_charges_at_least_one_sat() {
        // An empty blob is still not free, and a zero price still costs the
        // floor rather than nothing.
        assert_eq!(quote(10, &mints(), 0).amount_sats, 1);
        assert_eq!(quote(0, &mints(), 50 * 1024 * 1024).amount_sats, 1);
    }

    #[test]
    fn payment_required_carries_the_quote_a_client_must_pay() {
        // The 402 a client sees must name the amount, the unit and the mints
        // this server will accept, or the client cannot retry successfully.
        let quoted = quote(3, &mints(), 4 * 1024 * 1024);
        match quoted.payment_required() {
            AppError::PaymentRequired {
                amount_sats,
                unit,
                mints: accepted,
            } => {
                assert_eq!(amount_sats, 12);
                assert_eq!(unit, "sat");
                assert_eq!(accepted, mints());
            }
            other => panic!("expected a 402, got {other:?}"),
        }
    }

    #[test]
    fn test_calculate_price_zero_bytes() {
        // 0 bytes should still be 1 sat minimum
        assert_eq!(calculate_price(0, 10), 1);
    }

    #[test]
    fn test_calculate_price_small_file() {
        // 1 byte rounds up to 1 MB
        assert_eq!(calculate_price(1, 10), 10);
        // 1 KB rounds up to 1 MB
        assert_eq!(calculate_price(1024, 10), 10);
        // 1 MB - 1 byte rounds up to 1 MB
        assert_eq!(calculate_price(1024 * 1024 - 1, 10), 10);
    }

    #[test]
    fn test_calculate_price_exact_mb() {
        // Exactly 1 MB
        assert_eq!(calculate_price(1024 * 1024, 10), 10);
        // Exactly 2 MB
        assert_eq!(calculate_price(2 * 1024 * 1024, 10), 20);
    }

    #[test]
    fn test_calculate_price_over_mb() {
        // 1 MB + 1 byte rounds up to 2 MB
        assert_eq!(calculate_price(1024 * 1024 + 1, 10), 20);
        // 1.5 MB rounds up to 2 MB
        assert_eq!(calculate_price(1024 * 1024 + 512 * 1024, 10), 20);
    }

    #[test]
    fn test_calculate_price_large_file() {
        // 100 MB
        assert_eq!(calculate_price(100 * 1024 * 1024, 5), 500);
        // 1 GB
        assert_eq!(calculate_price(1024 * 1024 * 1024, 1), 1024);
    }

    #[test]
    fn test_calculate_price_minimum() {
        // With price_per_mb = 0, minimum is still 1
        assert_eq!(calculate_price(1024 * 1024, 0), 1);
    }

    #[test]
    fn test_parse_token_invalid_prefix() {
        let result = parse_token("invalidtoken");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn test_parse_token_empty() {
        let result = parse_token("");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_cashu_header_present() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Cashu", "cashuAtoken123".parse().unwrap());
        assert_eq!(
            extract_cashu_header(&headers),
            Some("cashuAtoken123".to_string())
        );
    }

    #[test]
    fn test_extract_cashu_header_lowercase() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-cashu", "cashuBtoken456".parse().unwrap());
        assert_eq!(
            extract_cashu_header(&headers),
            Some("cashuBtoken456".to_string())
        );
    }

    #[test]
    fn test_extract_cashu_header_missing() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(extract_cashu_header(&headers), None);
    }
}
