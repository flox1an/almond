//! Cashu payment processing service (BUD-07 / NUT-24)
//!
//! Handles token verification and wallet operations for paid uploads/downloads:
//! receiving payments, crash recovery, and operator withdrawals.

use crate::error::AppError;
#[cfg(all(unix, feature = "cashu"))]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(feature = "cashu")]
use {
    cashu::nuts::nut00::token::Token,
    cashu::nuts::{CheckStateRequest, KeySetInfo, PreMintSecrets, PublicKey, State},
    cdk::wallet::types::{
        KeysetLoadPolicy, OperationData, ReceiveSagaState, SendSagaState, Transaction,
        TransactionDirection, TransactionId, TransactionStatus, WalletSagaState,
    },
    cdk::wallet::{ReceiveOptions, SendOptions, Wallet as CdkWallet},
    cdk_sqlite::wallet::WalletSqliteDatabase,
    std::collections::HashMap,
    std::io::Write,
    std::path::{Path, PathBuf},
    std::str::FromStr,
    std::sync::Arc,
    tokio::sync::{Mutex, RwLock},
    tracing::{error, info, warn},
    uuid::Uuid,
};

/// A request that money can be demanded for.
///
/// Naming the operation keeps the feature gate in one place: every caller used
/// to read its own `feature_paid_*` flag and then re-spell the settlement
/// sequence, and the spellings drifted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
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

    #[cfg(not(feature = "cashu"))]
    {
        // Unreachable in practice: `Config::validate` rejects paid
        // operations when the crate is built without the `cashu` feature.
        let _ = token_str;
        Err(AppError::ServiceUnavailable(
            "Payment service is not initialized".to_string(),
        ))
    }
    #[cfg(feature = "cashu")]
    settle(state, &token_str, quoted, operation).await
}

#[cfg(feature = "cashu")]
async fn settle(
    state: &crate::models::AppState,
    token_str: &str,
    quoted: Quote,
    operation: PaidOperation,
) -> Result<(), AppError> {
    let token = parse_token(token_str)?;
    verify_token_basics(&token, quoted.amount_sats, &state.cashu_accepted_mints)?;

    // A configured paid feature without a wallet cannot take money, so it must
    // not hand out the blob either.
    let Some(wallet) = &state.cashu_wallet else {
        return Err(AppError::ServiceUnavailable(
            "Payment service is not initialized".to_string(),
        ));
    };

    // The fee is checked before the swap; a short settlement here means the
    // mint changed its fees mid-flight, and short payment must not buy the
    // operation.
    let received_sats = receive_for_purchase(wallet, &token, quoted.amount_sats).await?;
    if received_sats < quoted.amount_sats {
        warn!(
            required = quoted.amount_sats,
            received = received_sats,
            ?operation,
            "Mint settled below the quoted amount"
        );
        return Err(AppError::BadRequest(
            "Mint settled less than the required amount".to_owned(),
        ));
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
#[cfg(feature = "cashu")]
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

/// Verify token is from an accepted mint, in sats, and has sufficient amount
///
/// NUT-24: a wrong mint, wrong unit or short amount is a `400`.
///
/// # Arguments
/// * `token` - The parsed Cashu token
/// * `required_amount` - The minimum amount required in satoshis
/// * `accepted_mints` - List of accepted mint URLs
///
/// # Returns
/// Ok(()) if the token is valid, or an error describing the issue
#[cfg(feature = "cashu")]
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

    // A token without a unit is a sat token (NUT-00 default).
    let unit = token.unit().unwrap_or_default();
    if unit != cashu::nuts::CurrencyUnit::Sat {
        return Err(AppError::BadRequest(format!(
            "Token unit {unit} is not accepted; pay in sat"
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
#[cfg(feature = "cashu")]
fn write_seed_0600(path: &Path, seed: &[u8; 64]) -> Result<(), AppError> {
    let temporary = path.with_extension(format!("seed.tmp.{}", Uuid::new_v4()));
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

/// Read the wallet seed that lives next to the wallet database.
#[cfg(feature = "cashu")]
fn read_seed(wallet_path: &Path) -> Result<[u8; 64], AppError> {
    let seed_path = wallet_path.with_extension("seed");
    #[cfg(unix)]
    std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o600)).map_err(
        |error| AppError::InternalError(format!("Failed to secure wallet seed: {error}")),
    )?;
    let seed_bytes = std::fs::read(&seed_path)
        .map_err(|e| AppError::InternalError(format!("Failed to read wallet seed: {}", e)))?;
    <[u8; 64]>::try_from(seed_bytes)
        .map_err(|_| AppError::InternalError("Invalid seed file size".to_string()))
}

/// Initialize the CDK wallet for receiving payments
///
/// Creates or opens a SQLite-backed wallet configured for the first accepted mint.
/// The wallet is used to swap incoming tokens for fresh proofs. Call
/// [`recover_wallet`] once before serving requests.
///
/// # Arguments
/// * `wallet_path` - Path to the `SQLite` database file
/// * `accepted_mints` - List of accepted mint URLs (first one is used for wallet)
///
/// # Returns
/// An Arc-wrapped RwLock-protected wallet instance, or an error if initialization fails
#[cfg(feature = "cashu")]
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
    let existing_wallet = wallet_path.exists();
    if existing_wallet && !wallet_path.with_extension("seed").exists() {
        return Err(AppError::InternalError("Wallet database exists but its seed is missing; restore the original seed before starting".to_owned()));
    }

    // Create SQLite database for wallet storage
    let localstore = WalletSqliteDatabase::new(wallet_path).await.map_err(|e| {
        error!("Failed to create wallet database: {}", e);
        AppError::InternalError(format!("Failed to initialize wallet database: {}", e))
    })?;

    let seed: [u8; 64] = if wallet_path.with_extension("seed").exists() {
        read_seed(wallet_path)?
    } else {
        let mut seed = [0u8; 64];
        rand::fill(&mut seed);
        write_seed_0600(&wallet_path.with_extension("seed"), &seed)?;
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

#[cfg(feature = "cashu")]
fn wallet_db_error(error: impl std::fmt::Display) -> AppError {
    error!("Cashu wallet database error: {error}");
    AppError::InternalError("Cashu wallet database error".to_string())
}

#[cfg(feature = "cashu")]
fn mint_error(error: impl std::fmt::Display) -> AppError {
    warn!("Cashu mint request failed: {error}");
    AppError::BadGateway(format!("Cashu mint request failed: {error}"))
}

/// Canonical id of the payment carried by `token`, computed without a mint
/// call: the proof-`Y` transaction id over the token's proofs.
///
/// CDK keys saga-managed transactions by saga id, so [`find_payment`] matches
/// incoming transactions by their proof `Y`s, answering "did this token pay
/// us?" across crashes.
#[cfg(feature = "cashu")]
pub fn payment_id(token: &Token) -> Result<String, AppError> {
    let ys = token
        .token_secrets()
        .into_iter()
        .map(|secret| cashu::dhke::hash_to_curve(secret.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::BadRequest(format!("Invalid Cashu token secret: {e}")))?;
    if ys.is_empty() {
        return Err(AppError::BadRequest(
            "Cashu token has no proofs".to_string(),
        ));
    }
    Ok(TransactionId::new(ys).to_string())
}

/// Receive `token` into the wallet as payment of `required_sats`.
///
/// Rejects before any swap when the token would net less than `required_sats`
/// after the mint's input fee; overpayment is accepted (NUT-24: `>=`).
/// Returns the net sats the mint settled.
///
/// Errors: `BadRequest` only when the mint definitively did not swap (spent,
/// pending or duplicate inputs, wrong unit/mint). Anything else is
/// `BadGateway` and AMBIGUOUS: the swap may have settled. Keep the purchase
/// pending and resolve it with [`find_payment`] after the next
/// [`recover_wallet`].
#[cfg(feature = "cashu")]
pub async fn receive_for_purchase(
    wallet: &Arc<RwLock<CdkWallet>>,
    token: &Token,
    required_sats: u64,
) -> Result<u64, AppError> {
    let wallet = wallet.read().await;

    // Tokens may use inactive keysets, so resolve short ids against all of them.
    let keysets: Vec<KeySetInfo> = wallet
        .keysets(KeysetLoadPolicy::default())
        .await
        .map_err(mint_error)?
        .into_iter()
        .map(|keyset| KeySetInfo {
            id: keyset.id,
            unit: keyset.unit,
            active: keyset.active.unwrap_or_default(),
            input_fee_ppk: keyset.input_fee_ppk,
            final_expiry: keyset.final_expiry,
        })
        .collect();
    let proofs = token
        .proofs(&keysets)
        .map_err(|e| AppError::BadRequest(format!("Invalid Cashu token: {e}")))?;
    let fee_sats: u64 = wallet
        .get_proofs_fee(&proofs)
        .await
        .map_err(mint_error)?
        .total
        .into();
    let net_sats = get_token_amount(token)?.saturating_sub(fee_sats);
    if net_sats < required_sats {
        return Err(AppError::BadRequest(format!(
            "Insufficient payment: token nets {net_sats} sats after the mint's {fee_sats} sat fee \
             but {required_sats} sats are required"
        )));
    }

    let amount = wallet
        .receive(&token.to_string(), ReceiveOptions::default())
        .await
        .map_err(|e| match e {
            cdk::Error::TokenAlreadySpent
            | cdk::Error::TokenPending
            | cdk::Error::DuplicateInputs
            | cdk::Error::UnsupportedUnit
            | cdk::Error::IncorrectMint
            | cdk::Error::MultiMintTokenNotSupported => {
                AppError::BadRequest(format!("Cashu token rejected: {e}"))
            }
            other => {
                error!("Cashu receive failed, settlement unknown: {other}");
                AppError::BadGateway("Cashu wallet settlement failed".to_string())
            }
        })?;

    let amount_sats: u64 = amount.into();
    info!("Received {} sats into wallet", amount_sats);
    Ok(amount_sats)
}

/// Net sats received for [`payment_id`], or `None` if the wallet holds no
/// record of that payment settling.
///
/// Only a `Completed` incoming transaction counts: CDK records `Pending`
/// before the swap and `Failed` on rollback. A receive still in the recovery
/// journal is unproven even if CDK marked it completed (inputs spent, outputs
/// not restored).
#[cfg(feature = "cashu")]
pub async fn find_payment(
    wallet: &Arc<RwLock<CdkWallet>>,
    payment_id: &str,
) -> Result<Option<u64>, AppError> {
    let id = TransactionId::from_hex(payment_id)
        .map_err(|_| AppError::BadRequest("Invalid payment id".to_string()))?;
    let wallet = wallet.read().await;
    // ponytail: linear scan of incoming transactions; index proof Ys if receives reach many thousands.
    for transaction in wallet
        .list_transactions(Some(TransactionDirection::Incoming))
        .await
        .map_err(wallet_db_error)?
    {
        if transaction.status != TransactionStatus::Completed
            || TransactionId::new(transaction.ys.clone()) != id
        {
            continue;
        }
        if let Some(saga_id) = transaction.saga_id {
            if wallet
                .localstore
                .kv_read(JOURNAL.0, JOURNAL.1, &saga_id.to_string())
                .await
                .map_err(wallet_db_error)?
                .is_some()
            {
                continue;
            }
        }
        return Ok(Some(transaction.amount.into()));
    }
    Ok(None)
}

/// Serializes withdrawals, reclaims, restore and recovery.
// ponytail: process-wide lock; a second Almond process on the same wallet DB is unsupported.
#[cfg(feature = "cashu")]
static WALLET_OPS: Mutex<()> = Mutex::const_new(());

/// Outgoing-transaction metadata naming the withdrawal's `request_id`.
#[cfg(feature = "cashu")]
const WITHDRAW_REQUEST: &str = "almond_withdraw_request";
/// Outgoing-transaction metadata marking a withdrawal whose funds came back.
#[cfg(feature = "cashu")]
const WITHDRAW_RETURNED: &str = "almond_withdraw_returned";
/// Outgoing-transaction metadata written before a reclaim swap is attempted.
#[cfg(feature = "cashu")]
const WITHDRAW_RECLAIMING: &str = "almond_withdraw_reclaiming";

/// Send-saga memo carrying the `request_id` from `prepare_send` on, before any
/// transaction exists. `include_memo: false` keeps it out of the token.
#[cfg(feature = "cashu")]
fn withdraw_memo(request_id: &str) -> String {
    format!("almond-withdraw:{request_id}")
}
/// KV namespace (in the wallet DB) journaling receives that may have reached
/// the mint, written before CDK recovery can delete their sagas.
#[cfg(feature = "cashu")]
const JOURNAL: (&str, &str) = ("almond", "receive_journal");

#[cfg(feature = "cashu")]
#[derive(serde::Serialize)]
pub struct WalletBalance {
    pub mint: String,
    /// Unspent proofs the operator can withdraw.
    pub spendable_sats: u64,
    /// In flight: receives awaiting the mint and unclaimed withdrawal tokens.
    pub pending_sats: u64,
    /// Held by a prepared operation.
    pub reserved_sats: u64,
}

/// Local wallet balance; no mint call.
#[cfg(feature = "cashu")]
pub async fn wallet_balance(wallet: &Arc<RwLock<CdkWallet>>) -> Result<WalletBalance, AppError> {
    let wallet = wallet.read().await;
    let balance = |states: Vec<State>| {
        wallet.localstore.get_balance(
            Some(wallet.mint_url.clone()),
            Some(wallet.unit.clone()),
            Some(states),
        )
    };
    Ok(WalletBalance {
        mint: wallet.mint_url.to_string(),
        spendable_sats: balance(vec![State::Unspent])
            .await
            .map_err(wallet_db_error)?,
        pending_sats: balance(vec![State::Pending, State::PendingSpent])
            .await
            .map_err(wallet_db_error)?,
        reserved_sats: balance(vec![State::Reserved])
            .await
            .map_err(wallet_db_error)?,
    })
}

#[cfg(feature = "cashu")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WithdrawalState {
    /// Token issued and not yet claimed; `token` is the exported ecash.
    Outstanding,
    /// Funds are back in the wallet (reclaimed, or rolled back before the
    /// token existed).
    Returned,
    /// The mint reports every token proof spent and no reclaim was attempted:
    /// the recipient claimed it.
    Finalized,
    /// Not provable right now (mint unreachable, or a reclaim whose outcome
    /// was lost to a crash). Check the balance; never treat as claimed.
    Unknown,
}

#[cfg(feature = "cashu")]
#[derive(Clone, Debug, serde::Serialize)]
pub struct Withdrawal {
    pub request_id: String,
    pub operation_id: String,
    pub amount_sats: u64,
    pub fee_sats: u64,
    /// The same `cashuB` string on every read while outstanding.
    pub token: Option<String>,
    pub state: WithdrawalState,
}

#[cfg(feature = "cashu")]
fn validate_request_id(request_id: &str) -> Result<(), AppError> {
    let valid = (1..=64).contains(&request_id.len())
        && request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "request_id must be 1-64 characters of [A-Za-z0-9_-]".to_string(),
        ))
    }
}

/// The outgoing transaction CDK recorded for `request_id`, if any. CDK writes
/// it (with our metadata) in the same confirm step that creates the token.
#[cfg(feature = "cashu")]
async fn withdrawal_transaction(
    wallet: &CdkWallet,
    request_id: &str,
) -> Result<Option<Transaction>, AppError> {
    // ponytail: linear scan of outgoing transactions; index request ids if withdrawals reach many thousands.
    Ok(wallet
        .list_transactions(Some(TransactionDirection::Outgoing))
        .await
        .map_err(wallet_db_error)?
        .into_iter()
        .find(|t| t.metadata.get(WITHDRAW_REQUEST).map(String::as_str) == Some(request_id)))
}

#[cfg(feature = "cashu")]
async fn describe_withdrawal(
    wallet: &CdkWallet,
    request_id: &str,
    transaction: &Transaction,
) -> Result<Withdrawal, AppError> {
    let operation_id = transaction.saga_id.ok_or_else(|| {
        AppError::InternalError("Withdrawal transaction has no operation id".to_string())
    })?;
    let (state, token) = match wallet
        .localstore
        .get_saga(&operation_id)
        .await
        .map_err(wallet_db_error)?
    {
        Some(saga) => match (saga.state, saga.data) {
            (
                WalletSagaState::Send(SendSagaState::TokenCreated | SendSagaState::RollingBack),
                OperationData::Send(data),
            ) => (WithdrawalState::Outstanding, data.token),
            // Only a crash inside CDK's confirm leaves a transaction beside an
            // unconfirmed saga; startup recovery rolls it back.
            _ => {
                return Err(AppError::ServiceUnavailable(
                    "Withdrawal was interrupted before its token was created; restart Almond \
                     to run wallet recovery"
                        .to_string(),
                ))
            }
        },
        None if transaction.metadata.contains_key(WITHDRAW_RETURNED) => {
            (WithdrawalState::Returned, None)
        }
        None => {
            let claimed = !transaction.metadata.contains_key(WITHDRAW_RECLAIMING)
                && mint_says_all_spent(wallet, transaction.ys.clone()).await == Some(true);
            let state = if claimed {
                WithdrawalState::Finalized
            } else {
                WithdrawalState::Unknown
            };
            (state, None)
        }
    };
    Ok(Withdrawal {
        request_id: request_id.to_string(),
        operation_id: operation_id.to_string(),
        amount_sats: transaction.amount.into(),
        fee_sats: transaction.fee.into(),
        token,
        state,
    })
}

/// `Some(true)` only when the mint answered and every proof is spent.
#[cfg(feature = "cashu")]
async fn mint_says_all_spent(wallet: &CdkWallet, ys: Vec<PublicKey>) -> Option<bool> {
    match wallet
        .mint_connector()
        .post_check_state(CheckStateRequest { ys })
        .await
    {
        Ok(response) => Some(
            !response.states.is_empty() && response.states.iter().all(|s| s.state == State::Spent),
        ),
        Err(e) => {
            warn!("Mint proof state check failed: {e}");
            None
        }
    }
}

/// Set or clear a marker on the withdrawal's outgoing transaction.
#[cfg(feature = "cashu")]
async fn set_marker(
    wallet: &CdkWallet,
    transaction: &mut Transaction,
    key: &str,
    set: bool,
) -> Result<(), AppError> {
    // CDK updates the status after insert; start from the stored row so the
    // upsert below never reverts it.
    if let Some(current) = wallet
        .localstore
        .get_transaction(transaction.id())
        .await
        .map_err(wallet_db_error)?
    {
        *transaction = current;
    }
    if set {
        transaction
            .metadata
            .insert(key.to_string(), "1".to_string());
    } else {
        transaction.metadata.remove(key);
    }
    // Same id (saga id): this upserts the metadata.
    wallet
        .localstore
        .add_transaction(transaction.clone())
        .await
        .map_err(wallet_db_error)
}

/// A send saga for `request_id` that has not produced a transaction yet:
/// prepared, or interrupted inside confirm.
#[cfg(feature = "cashu")]
async fn has_unconfirmed_withdrawal(
    wallet: &CdkWallet,
    request_id: &str,
) -> Result<bool, AppError> {
    let memo = withdraw_memo(request_id);
    Ok(wallet
        .localstore
        .get_incomplete_sagas()
        .await
        .map_err(wallet_db_error)?
        .iter()
        .any(|saga| matches!(&saga.data, OperationData::Send(data) if data.memo.as_deref() == Some(memo.as_str()))))
}

#[cfg(feature = "cashu")]
fn interrupted_withdrawal(request_id: &str) -> AppError {
    AppError::ServiceUnavailable(format!(
        "Withdrawal {request_id} was interrupted before its token was recorded; restart \
         Almond to run wallet recovery, then retry"
    ))
}

/// Export `amount_sats` of ecash as a token, idempotently per `request_id`.
///
/// The request id is persisted by CDK with the send saga at `prepare_send`
/// (memo, excluded from the token) and with the outgoing transaction at
/// confirm (metadata). A retry with the same `request_id` therefore never
/// prepares a second send: it returns the stored withdrawal (the identical
/// token while outstanding), or `ServiceUnavailable` while an earlier attempt
/// is unconfirmed. Such an attempt, e.g. a mint timeout during confirm,
/// stays blocked until startup recovery rolls it back; no token from it was
/// ever returned, so retrying after that restart cannot double-withdraw.
#[cfg(feature = "cashu")]
pub async fn withdraw(
    wallet: &Arc<RwLock<CdkWallet>>,
    request_id: &str,
    amount_sats: u64,
) -> Result<Withdrawal, AppError> {
    validate_request_id(request_id)?;
    if amount_sats == 0 {
        return Err(AppError::BadRequest("amount must be positive".to_string()));
    }
    let _serial = WALLET_OPS.lock().await;
    let wallet = wallet.read().await;

    if let Some(transaction) = withdrawal_transaction(&wallet, request_id).await? {
        let existing = describe_withdrawal(&wallet, request_id, &transaction).await?;
        if existing.amount_sats != amount_sats {
            return Err(AppError::Conflict(format!(
                "request_id {request_id} was already used for {} sats",
                existing.amount_sats
            )));
        }
        return Ok(existing);
    }
    if has_unconfirmed_withdrawal(&wallet, request_id).await? {
        return Err(interrupted_withdrawal(request_id));
    }

    let options = SendOptions {
        memo: Some(cdk::wallet::SendMemo {
            memo: withdraw_memo(request_id),
            include_memo: false,
        }),
        metadata: HashMap::from([(WITHDRAW_REQUEST.to_string(), request_id.to_string())]),
        ..SendOptions::default()
    };
    let prepared = wallet
        .prepare_send(cdk::Amount::from(amount_sats), options)
        .await
        .map_err(|e| match e {
            cdk::Error::InsufficientFunds => {
                AppError::BadRequest("Insufficient wallet balance".to_string())
            }
            other => mint_error(other),
        })?;
    let operation_id = prepared.operation_id();
    let fee_sats: u64 = prepared.fee().into();
    let token = prepared.confirm(None).await.map_err(|e| {
        error!("Withdrawal {request_id} ({operation_id}) failed to confirm: {e}");
        AppError::BadGateway(format!(
            "Withdrawal {request_id} failed at the mint ({e}); no token was issued. Retrying \
             the same request_id may require a restart first"
        ))
    })?;
    info!("Withdrawal {request_id} issued {amount_sats} sats ({operation_id})");

    Ok(Withdrawal {
        request_id: request_id.to_string(),
        operation_id: operation_id.to_string(),
        amount_sats,
        fee_sats,
        token: Some(token.to_string()),
        state: WithdrawalState::Outstanding,
    })
}

/// Current state of the withdrawal for `request_id`.
///
/// While outstanding, asks the mint whether the token was claimed; if the
/// mint is unreachable the local state (still outstanding) is returned.
#[cfg(feature = "cashu")]
pub async fn withdrawal(
    wallet: &Arc<RwLock<CdkWallet>>,
    request_id: &str,
) -> Result<Option<Withdrawal>, AppError> {
    validate_request_id(request_id)?;
    let _serial = WALLET_OPS.lock().await;
    let wallet = wallet.read().await;
    let Some(transaction) = withdrawal_transaction(&wallet, request_id).await? else {
        if has_unconfirmed_withdrawal(&wallet, request_id).await? {
            return Err(interrupted_withdrawal(request_id));
        }
        return Ok(None);
    };
    let mut found = describe_withdrawal(&wallet, request_id, &transaction).await?;
    if found.state == WithdrawalState::Outstanding {
        let operation_id = transaction.saga_id.expect("described above");
        match wallet.check_send_status(operation_id).await {
            // CDK reports false while a reclaim is rolling back, so true means
            // the recipient spent every proof.
            Ok(true) => {
                found.state = WithdrawalState::Finalized;
                found.token = None;
            }
            Ok(false) => {}
            Err(e) => warn!("Could not check withdrawal {request_id} with the mint: {e}"),
        }
    }
    Ok(Some(found))
}

/// Swap an unclaimed withdrawal token back into the wallet; returns the sats
/// recovered (after the mint's swap fee).
///
/// `Conflict` if the recipient already claimed it or it is not outstanding;
/// `BadGateway` if the mint failed, in which case the token stays outstanding
/// and the call can be retried. A crash mid-reclaim leaves the withdrawal
/// `unknown`, never `finalized`.
#[cfg(feature = "cashu")]
pub async fn reclaim_withdrawal(
    wallet: &Arc<RwLock<CdkWallet>>,
    request_id: &str,
) -> Result<u64, AppError> {
    validate_request_id(request_id)?;
    let _serial = WALLET_OPS.lock().await;
    let wallet = wallet.read().await;
    let mut transaction = withdrawal_transaction(&wallet, request_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("No withdrawal {request_id}")))?;
    let found = describe_withdrawal(&wallet, request_id, &transaction).await?;
    if found.state != WithdrawalState::Outstanding {
        return Err(AppError::Conflict(format!(
            "Withdrawal {request_id} is {:?}; nothing to reclaim",
            found.state
        )));
    }
    let operation_id = transaction.saga_id.expect("described above");
    set_marker(&wallet, &mut transaction, WITHDRAW_RECLAIMING, true).await?;
    match wallet.revoke_send(operation_id).await {
        Ok(amount) => {
            set_marker(&wallet, &mut transaction, WITHDRAW_RETURNED, true).await?;
            Ok(amount.into())
        }
        Err(e) => match wallet.localstore.get_saga(&operation_id).await {
            // CDK deletes the saga only when the mint showed the proofs spent
            // before our swap: the recipient claimed it.
            Ok(None) => {
                set_marker(&wallet, &mut transaction, WITHDRAW_RECLAIMING, false).await?;
                Err(AppError::Conflict(format!(
                    "Withdrawal {request_id} was already claimed"
                )))
            }
            // Saga reverted to outstanding; the reclaim marker no longer
            // matters because the saga exists.
            _ => Err(mint_error(e)),
        },
    }
}

/// NUT-13 restore from the seed; returns unspent sats found.
///
/// Only for funds lost to an ambiguous mint failure. Refused while any saga
/// (including an outstanding withdrawal) is open, because CDK's restore
/// upserts every mint-unspent proof as spendable, which would hand the
/// wallet the proofs of a token the operator already exported.
#[cfg(feature = "cashu")]
pub async fn restore_wallet(wallet: &Arc<RwLock<CdkWallet>>) -> Result<u64, AppError> {
    let _serial = WALLET_OPS.lock().await;
    let wallet = wallet.write().await;
    let open = wallet
        .localstore
        .get_incomplete_sagas()
        .await
        .map_err(wallet_db_error)?
        .into_iter()
        .any(|saga| saga.mint_url == wallet.mint_url && saga.unit == wallet.unit);
    if open {
        return Err(AppError::Conflict(
            "Reclaim or wait out outstanding wallet operations before restoring".to_string(),
        ));
    }
    let restored = wallet.restore().await.map_err(mint_error)?;
    Ok(restored.unspent.into())
}

#[cfg(feature = "cashu")]
#[derive(serde::Serialize, serde::Deserialize)]
struct ReceiveJournal {
    /// Incoming proof `Y`s (hex); their transaction id is the payment id.
    input_ys: Vec<String>,
    /// `Y`s of the outputs this receive asked the mint to sign (hex).
    output_ys: Vec<String>,
    input_sats: u64,
}

#[cfg(feature = "cashu")]
fn parse_ys(ys: &[String]) -> Result<Vec<PublicKey>, AppError> {
    ys.iter()
        .map(PublicKey::from_hex)
        .collect::<Result<_, _>>()
        .map_err(|e| AppError::InternalError(format!("Corrupt receive journal: {e}")))
}

/// Crash recovery. Call once at startup, after [`init_wallet`] and BEFORE
/// serving requests: CDK rolls back every in-flight operation it finds.
///
/// CDK may mark an interrupted receive completed although its outputs were
/// never restored. So each receive that may have reached the mint is first
/// journaled in the wallet DB with the `Y`s of the outputs it asked the mint
/// to sign; once CDK has finished with it, the payment is recorded as
/// completed iff those exact outputs are now in the wallet. Spent inputs
/// alone never count as payment, and [`find_payment`] ignores journaled
/// receives.
///
/// Returns how many journaled receives are still unresolved (mint
/// unreachable, or inputs spent without our outputs). Their purchases must
/// stay pending; the next startup retries. Never deletes state on error.
#[cfg(feature = "cashu")]
pub async fn recover_wallet(
    wallet: &Arc<RwLock<CdkWallet>>,
    wallet_path: &Path,
) -> Result<usize, AppError> {
    let _serial = WALLET_OPS.lock().await;
    let wallet = wallet.write().await;
    let store = &wallet.localstore;
    let seed = read_seed(wallet_path)?;

    for saga in store
        .get_incomplete_sagas()
        .await
        .map_err(wallet_db_error)?
    {
        if saga.mint_url != wallet.mint_url || saga.unit != wallet.unit {
            continue;
        }
        let (
            WalletSagaState::Receive(ReceiveSagaState::SwapRequested),
            OperationData::Receive(data),
        ) = (&saga.state, &saga.data)
        else {
            continue;
        };
        let (Some(start), Some(end), Some(first_output)) = (
            data.counter_start,
            data.counter_end,
            data.blinded_messages
                .as_ref()
                .and_then(|outputs| outputs.first()),
        ) else {
            continue;
        };
        let inputs = store
            .get_reserved_proofs(&saga.id)
            .await
            .map_err(wallet_db_error)?;
        if inputs.is_empty() {
            continue;
        }
        // Same derivation CDK's recovery uses for these outputs.
        let output_ys = PreMintSecrets::restore_batch(first_output.keyset_id, &seed, start, end)
            .map_err(|e| AppError::InternalError(format!("Output derivation failed: {e}")))?
            .secrets()
            .iter()
            .map(|secret| cashu::dhke::hash_to_curve(secret.as_bytes()).map(|y| y.to_hex()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::InternalError(format!("Output derivation failed: {e}")))?;
        let entry = ReceiveJournal {
            input_ys: inputs.iter().map(|p| p.y.to_hex()).collect(),
            output_ys,
            input_sats: inputs.iter().map(|p| u64::from(p.proof.amount)).sum(),
        };
        let value = serde_json::to_vec(&entry)?;
        store
            .kv_write(JOURNAL.0, JOURNAL.1, &saga.id.to_string(), &value)
            .await
            .map_err(wallet_db_error)?;
    }

    let report = wallet
        .recover_incomplete_sagas()
        .await
        .map_err(wallet_db_error)?;
    if report.skipped + report.failed > 0 {
        warn!(
            skipped = report.skipped,
            failed = report.failed,
            "Cashu wallet recovery incomplete; retrying at next startup"
        );
    }

    let mut unresolved = 0;
    for key in store
        .kv_list(JOURNAL.0, JOURNAL.1)
        .await
        .map_err(wallet_db_error)?
    {
        let saga_id = Uuid::parse_str(&key)
            .map_err(|e| AppError::InternalError(format!("Corrupt receive journal key: {e}")))?;
        if store
            .get_saga(&saga_id)
            .await
            .map_err(wallet_db_error)?
            .is_some()
        {
            unresolved += 1;
            continue;
        }
        let Some(value) = store
            .kv_read(JOURNAL.0, JOURNAL.1, &key)
            .await
            .map_err(wallet_db_error)?
        else {
            continue;
        };
        let entry: ReceiveJournal = serde_json::from_slice(&value)?;
        let input_ys = parse_ys(&entry.input_ys)?;
        let payment = TransactionId::new(input_ys.clone());

        // CDK has already written its own transaction under the saga id, and
        // marks it completed even when the outputs could not be restored, so
        // always decide from our outputs.
        let outputs = store
            .get_proofs_by_ys(parse_ys(&entry.output_ys)?)
            .await
            .map_err(wallet_db_error)?;
        if outputs.is_empty() {
            // Inputs still unspent at the mint: CDK rolled the receive
            // back and the payer still holds a valid token, so there is
            // nothing to credit. Anything else stays journaled.
            let unspent = match wallet
                .mint_connector()
                .post_check_state(CheckStateRequest { ys: input_ys })
                .await
            {
                Ok(response) => response.states.iter().all(|s| s.state == State::Unspent),
                Err(e) => {
                    warn!("Receive {saga_id}: mint state check failed: {e}");
                    false
                }
            };
            if !unspent {
                warn!(
                    "Receive {saga_id} (payment {payment}) is not proven settled; \
                     leaving it unresolved"
                );
                unresolved += 1;
                continue;
            }
            // Nothing was received; never leave a completed record behind.
            if let Some(mut transaction) = store
                .get_transaction(TransactionId::from_saga_id(saga_id))
                .await
                .map_err(wallet_db_error)?
            {
                if transaction.status != TransactionStatus::Failed {
                    transaction.status = TransactionStatus::Failed;
                    store
                        .add_transaction(transaction)
                        .await
                        .map_err(wallet_db_error)?;
                }
            }
        } else {
            let amount_sats: u64 = outputs.iter().map(|p| u64::from(p.proof.amount)).sum();
            // Same saga id, same transaction id: overwrites CDK's record.
            store
                .add_transaction(Transaction {
                    mint_url: wallet.mint_url.clone(),
                    direction: TransactionDirection::Incoming,
                    amount: cdk::Amount::from(amount_sats),
                    fee: cdk::Amount::from(entry.input_sats.saturating_sub(amount_sats)),
                    unit: wallet.unit.clone(),
                    ys: input_ys,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                    memo: None,
                    metadata: HashMap::from([(
                        "almond_recovered_receive".to_string(),
                        key.clone(),
                    )]),
                    quote_id: None,
                    payment_request: None,
                    payment_proof: None,
                    payment_method: None,
                    saga_id: Some(saga_id),
                    status: TransactionStatus::Completed,
                })
                .await
                .map_err(wallet_db_error)?;
            info!("Recovered receive {saga_id}: payment {payment} settled {amount_sats} sats");
        }
        store
            .kv_remove(JOURNAL.0, JOURNAL.1, &key)
            .await
            .map_err(wallet_db_error)?;
    }

    // A withdrawal CDK rolled back (crash inside confirm) left its outgoing
    // transaction behind with the proofs unspent in our wallet. Nothing can
    // spend them before this runs, so mark it now while that is provable.
    for mut transaction in wallet
        .list_transactions(Some(TransactionDirection::Outgoing))
        .await
        .map_err(wallet_db_error)?
    {
        let Some(operation_id) = transaction.saga_id else {
            continue;
        };
        if !transaction.metadata.contains_key(WITHDRAW_REQUEST)
            || transaction.metadata.contains_key(WITHDRAW_RETURNED)
            || store
                .get_saga(&operation_id)
                .await
                .map_err(wallet_db_error)?
                .is_some()
        {
            continue;
        }
        let back_in_wallet = store
            .get_proofs_by_ys(transaction.ys.clone())
            .await
            .map_err(wallet_db_error)?
            .iter()
            .any(|p| p.state == State::Unspent);
        if back_in_wallet {
            set_marker(&wallet, &mut transaction, WITHDRAW_RETURNED, true).await?;
        }
    }

    Ok(unresolved)
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
#[cfg(feature = "cashu")]
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
#[cfg(feature = "cashu")]
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

    #[cfg(feature = "cashu")]
    #[test]
    fn test_parse_token_invalid_prefix() {
        let result = parse_token("invalidtoken");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[cfg(feature = "cashu")]
    #[test]
    fn test_parse_token_empty() {
        let result = parse_token("");
        assert!(result.is_err());
    }

    #[cfg(feature = "cashu")]
    async fn open_wallet(path: &Path) -> Arc<RwLock<CdkWallet>> {
        let db = WalletSqliteDatabase::new(path.to_path_buf()).await.unwrap();
        let wallet = CdkWallet::new(
            "https://mint.example.com",
            cashu::nuts::CurrencyUnit::Sat,
            Arc::new(db),
            [7; 64],
            None,
        )
        .unwrap();
        Arc::new(RwLock::new(wallet))
    }

    #[cfg(feature = "cashu")]
    async fn paid(wallet: &Arc<RwLock<CdkWallet>>, ys: &[PublicKey]) -> Option<u64> {
        find_payment(wallet, &TransactionId::new(ys.to_vec()).to_string())
            .await
            .unwrap()
    }

    /// CDK 0.18 keys saga transactions by saga id and records `Pending` and
    /// `Failed` receives. A payment must still resolve by its proof `Y`s, only
    /// once completed, after a reopen, and for rows CDK 0.17 wrote under the
    /// proof-`Y` key that the upstream migration rekeys.
    #[cfg(feature = "cashu")]
    #[tokio::test]
    async fn find_payment_counts_only_completed_receives_by_proof_ys() {
        let dir = std::env::temp_dir().join(format!("almond-find-payment-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wallet.db");
        let [completed, pending, failed, journaled, legacy_saga, legacy_plain]: [Vec<PublicKey>;
            6] = std::array::from_fn(|_| {
            (0..2)
                .map(|_| cashu::nuts::SecretKey::generate().public_key())
                .collect()
        });
        let journaled_saga = Uuid::new_v4();
        let legacy_saga_id = Uuid::new_v4();

        let wallet = open_wallet(&path).await;
        {
            let w = wallet.read().await;
            let incoming = |ys: &Vec<PublicKey>, sats: u64, saga_id, status| Transaction {
                mint_url: w.mint_url.clone(),
                direction: TransactionDirection::Incoming,
                amount: cdk::Amount::from(sats),
                fee: cdk::Amount::ZERO,
                unit: w.unit.clone(),
                ys: ys.clone(),
                timestamp: 1,
                memo: None,
                metadata: HashMap::new(),
                quote_id: None,
                payment_request: None,
                payment_proof: None,
                payment_method: None,
                saga_id,
                status,
            };
            for transaction in [
                incoming(
                    &completed,
                    21,
                    Some(Uuid::new_v4()),
                    TransactionStatus::Completed,
                ),
                incoming(
                    &pending,
                    22,
                    Some(Uuid::new_v4()),
                    TransactionStatus::Pending,
                ),
                incoming(&failed, 23, Some(Uuid::new_v4()), TransactionStatus::Failed),
                incoming(
                    &journaled,
                    24,
                    Some(journaled_saga),
                    TransactionStatus::Completed,
                ),
                incoming(
                    &legacy_saga,
                    25,
                    Some(legacy_saga_id),
                    TransactionStatus::Completed,
                ),
                incoming(&legacy_plain, 26, None, TransactionStatus::Completed),
            ] {
                w.localstore.add_transaction(transaction).await.unwrap();
            }
            // Recovery has not proven this receive's outputs yet.
            w.localstore
                .kv_write(JOURNAL.0, JOURNAL.1, &journaled_saga.to_string(), b"{}")
                .await
                .unwrap();
        }
        drop(wallet);

        // Put one row back into the CDK 0.17 layout (keyed by proof Ys) and
        // un-record the upstream rekey migration so reopening runs it again.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE transactions SET id = ?1 WHERE saga_id = ?2",
                rusqlite::params![
                    TransactionId::new(legacy_saga.clone()).as_slice(),
                    legacy_saga_id.to_string()
                ],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM migrations WHERE name = '20260726000000_rekey_saga_transactions.sql'",
                [],
            )
            .unwrap();
        }

        let wallet = open_wallet(&path).await;
        {
            let w = wallet.read().await;
            assert!(w
                .localstore
                .get_transaction(TransactionId::from_saga_id(legacy_saga_id))
                .await
                .unwrap()
                .is_some());
            assert!(w
                .localstore
                .get_transaction(TransactionId::new(legacy_saga.clone()))
                .await
                .unwrap()
                .is_none());
        }
        assert_eq!(paid(&wallet, &completed).await, Some(21));
        assert_eq!(paid(&wallet, &pending).await, None);
        assert_eq!(paid(&wallet, &failed).await, None);
        assert_eq!(paid(&wallet, &journaled).await, None);
        assert_eq!(paid(&wallet, &legacy_saga).await, Some(25));
        assert_eq!(paid(&wallet, &legacy_plain).await, Some(26));

        drop(wallet);
        std::fs::remove_dir_all(&dir).unwrap();
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
