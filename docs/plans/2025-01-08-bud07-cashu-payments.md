# BUD-07 Cashu Payments Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add paid upload/download/mirror support using Cashu ecash tokens (BUD-07/NUT-24)

**Architecture:** Server returns HTTP 402 with `X-Cashu` payment request header when payment required. Client retries with `X-Cashu` header containing `cashuB` token. Server verifies token, swaps to own wallet (CDK + SQLite), then proceeds with operation.

**Tech Stack:** `cdk` + `cdk-sqlite` for wallet, `cashu` crate for token parsing, existing `reqwest` for mint communication

---

## Configuration

New environment variables:
```
FEATURE_PAID_UPLOAD=off|on          # Default: off
FEATURE_PAID_MIRROR=off|on          # Default: off
FEATURE_PAID_DOWNLOAD=off|on        # Default: off
CASHU_PRICE_PER_MB=1                # Price in sats per MB (default: 1)
CASHU_ACCEPTED_MINTS=https://mint1.example.com,https://mint2.example.com
CASHU_WALLET_PATH=./cashu_wallet.db # SQLite wallet storage path
```

---

## Task 1: Add Dependencies

**Files:**
- Modify: `Cargo.toml`

**Step 1: Add cashu and CDK dependencies**

Add to `Cargo.toml`:
```toml
# Cashu payment support (BUD-07)
cdk = { version = "0.4", default-features = false, features = ["wallet"] }
cdk-sqlite = { version = "0.4", features = ["wallet"] }
cashu = "0.4"
```

**Step 2: Verify dependencies resolve**

Run: `cargo check`
Expected: Compiles successfully (may take a while to download)

**Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "chore: add CDK and cashu dependencies for BUD-07 payments"
```

---

## Task 2: Add PaymentRequired Error Variant

**Files:**
- Modify: `src/error.rs`

**Step 1: Add PaymentRequired variant to AppError**

Add new variant after `Forbidden`:
```rust
/// Payment required (402)
PaymentRequired {
    amount_sats: u64,
    unit: String,
    mints: Vec<String>,
},
```

**Step 2: Update Display impl**

Add match arm in `fmt`:
```rust
AppError::PaymentRequired { amount_sats, unit, mints } => {
    write!(f, "Payment required: {} {} (mints: {})", amount_sats, unit, mints.join(", "))
}
```

**Step 3: Update IntoResponse impl**

Add match arm that returns 402 with `X-Cashu` header:
```rust
AppError::PaymentRequired { amount_sats, unit, mints } => {
    use base64::{Engine, engine::general_purpose::URL_SAFE};

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
        .unwrap()
        .into_response();
}
```

**Step 4: Update StatusCode From impl**

Add match arm:
```rust
AppError::PaymentRequired { .. } => StatusCode::PAYMENT_REQUIRED,
```

**Step 5: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 6: Commit**

```bash
git add src/error.rs
git commit -m "feat: add PaymentRequired error variant for HTTP 402"
```

---

## Task 3: Add Cashu Configuration to AppState

**Files:**
- Modify: `src/models.rs`
- Modify: `src/main.rs`

**Step 1: Add Cashu fields to AppState**

In `src/models.rs`, add to `AppState` struct:
```rust
// Cashu payment configuration (BUD-07)
pub feature_paid_upload: bool,
pub feature_paid_mirror: bool,
pub feature_paid_download: bool,
pub cashu_price_per_mb: u64,
pub cashu_accepted_mints: Vec<String>,
pub cashu_wallet_path: PathBuf,
pub cashu_wallet: Option<Arc<RwLock<cdk::wallet::Wallet>>>,
```

**Step 2: Parse Cashu config in load_app_state**

In `src/main.rs`, add after other feature flag parsing:
```rust
// Parse Cashu payment configuration (BUD-07)
let feature_paid_upload = env::var("FEATURE_PAID_UPLOAD")
    .unwrap_or_else(|_| "off".to_string())
    .to_lowercase() == "on";

let feature_paid_mirror = env::var("FEATURE_PAID_MIRROR")
    .unwrap_or_else(|_| "off".to_string())
    .to_lowercase() == "on";

let feature_paid_download = env::var("FEATURE_PAID_DOWNLOAD")
    .unwrap_or_else(|_| "off".to_string())
    .to_lowercase() == "on";

let cashu_price_per_mb = env::var("CASHU_PRICE_PER_MB")
    .unwrap_or_else(|_| "1".to_string())
    .parse::<u64>()
    .expect("Invalid value for CASHU_PRICE_PER_MB");

let cashu_accepted_mints: Vec<String> = env::var("CASHU_ACCEPTED_MINTS")
    .unwrap_or_default()
    .split(',')
    .filter_map(|m| {
        let m = m.trim();
        if m.is_empty() { None } else { Some(m.to_string()) }
    })
    .collect();

let cashu_wallet_path = PathBuf::from(
    env::var("CASHU_WALLET_PATH").unwrap_or_else(|_| "./cashu_wallet.db".to_string())
);

// Validate: if any paid feature is on, mints must be configured
let any_paid_feature = feature_paid_upload || feature_paid_mirror || feature_paid_download;
if any_paid_feature && cashu_accepted_mints.is_empty() {
    panic!("CASHU_ACCEPTED_MINTS must be set when paid features are enabled");
}

if any_paid_feature {
    info!("💰 Cashu payments enabled - Price: {} sats/MB, Mints: {:?}",
          cashu_price_per_mb, cashu_accepted_mints);
}
```

**Step 3: Initialize fields in AppState construction**

Add to AppState initialization:
```rust
feature_paid_upload,
feature_paid_mirror,
feature_paid_download,
cashu_price_per_mb,
cashu_accepted_mints,
cashu_wallet_path,
cashu_wallet: None, // Initialized separately
```

**Step 4: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 5: Commit**

```bash
git add src/models.rs src/main.rs
git commit -m "feat: add Cashu payment configuration to AppState"
```

---

## Task 4: Create Cashu Service Module

**Files:**
- Create: `src/services/cashu.rs`
- Modify: `src/services/mod.rs`

**Step 1: Create cashu.rs service module**

Create `src/services/cashu.rs`:
```rust
//! Cashu payment processing service (BUD-07)
//!
//! Handles token verification and wallet operations for paid uploads/downloads.

use crate::error::AppError;
use crate::models::AppState;
use cashu::nuts::nut00::Token;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

/// Calculate required payment in sats for a given size
pub fn calculate_price(size_bytes: u64, price_per_mb: u64) -> u64 {
    // Round up to nearest MB, minimum 1 sat
    let size_mb = (size_bytes + 1024 * 1024 - 1) / (1024 * 1024);
    std::cmp::max(1, size_mb * price_per_mb)
}

/// Parse a Cashu token from the X-Cashu header value
pub fn parse_token(token_str: &str) -> Result<Token, AppError> {
    // Token should be cashuA or cashuB prefixed
    let token_str = token_str.trim();

    Token::from_str(token_str).map_err(|e| {
        warn!("Failed to parse Cashu token: {}", e);
        AppError::BadRequest(format!("Invalid Cashu token: {}", e))
    })
}

/// Verify token is from an accepted mint and has sufficient amount
pub fn verify_token_basics(
    token: &Token,
    required_amount: u64,
    accepted_mints: &[String],
) -> Result<(), AppError> {
    // Get token mint URL
    let token_mint = token.mint_url().map_err(|e| {
        AppError::BadRequest(format!("Cannot determine token mint: {}", e))
    })?;

    let token_mint_str = token_mint.to_string();

    // Check mint is accepted
    let mint_accepted = accepted_mints.iter().any(|m| {
        // Normalize URLs for comparison (remove trailing slash)
        let m_normalized = m.trim_end_matches('/');
        let token_normalized = token_mint_str.trim_end_matches('/');
        m_normalized == token_normalized
    });

    if !mint_accepted {
        return Err(AppError::BadRequest(format!(
            "Token mint {} not in accepted list: {:?}",
            token_mint_str, accepted_mints
        )));
    }

    // Check amount
    let token_amount = token.value().map_err(|e| {
        AppError::BadRequest(format!("Cannot determine token value: {}", e))
    })?;

    let token_sats: u64 = token_amount.into();

    if token_sats < required_amount {
        return Err(AppError::BadRequest(format!(
            "Insufficient payment: got {} sats, need {} sats",
            token_sats, required_amount
        )));
    }

    info!("💰 Token verified: {} sats from {}", token_sats, token_mint_str);
    Ok(())
}

/// Initialize the CDK wallet for receiving payments
pub async fn init_wallet(
    wallet_path: &std::path::Path,
    accepted_mints: &[String],
) -> Result<Arc<RwLock<cdk::wallet::Wallet>>, AppError> {
    use cdk::wallet::Wallet;
    use cdk_sqlite::wallet::WalletSqliteDatabase;

    // Create parent directory if needed
    if let Some(parent) = wallet_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            AppError::IoError(format!("Failed to create wallet directory: {}", e))
        })?;
    }

    // Initialize SQLite database
    let db = WalletSqliteDatabase::new(wallet_path).await.map_err(|e| {
        AppError::InternalError(format!("Failed to initialize wallet database: {}", e))
    })?;

    // Create wallet with first accepted mint as default
    let mint_url = accepted_mints.first()
        .ok_or_else(|| AppError::InternalError("No accepted mints configured".to_string()))?;

    let mint_url = cdk::mint_url::MintUrl::from_str(mint_url).map_err(|e| {
        AppError::InternalError(format!("Invalid mint URL {}: {}", mint_url, e))
    })?;

    let wallet = Wallet::new(
        Arc::new(db),
        &mint_url,
        cdk::nuts::CurrencyUnit::Sat,
        vec![], // No seed words needed for receiving
    ).map_err(|e| {
        AppError::InternalError(format!("Failed to create wallet: {}", e))
    })?;

    info!("💰 Cashu wallet initialized at {}", wallet_path.display());

    Ok(Arc::new(RwLock::new(wallet)))
}

/// Receive a token into the wallet (swap to own proofs)
pub async fn receive_token(
    wallet: &Arc<RwLock<cdk::wallet::Wallet>>,
    token: &Token,
) -> Result<u64, AppError> {
    let wallet = wallet.write().await;

    // Receive the token (this swaps it to our wallet)
    let amount = wallet.receive(
        &token.to_string(),
        cdk::wallet::ReceiveOptions::default(),
    ).await.map_err(|e| {
        error!("Failed to receive Cashu token: {}", e);
        AppError::BadRequest(format!("Failed to process payment: {}", e))
    })?;

    let sats: u64 = amount.into();
    info!("💰 Received {} sats into wallet", sats);

    Ok(sats)
}

/// Extract X-Cashu header from request headers
pub fn extract_cashu_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("X-Cashu")
        .or_else(|| headers.get("x-cashu"))
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}
```

**Step 2: Export cashu module**

In `src/services/mod.rs`, add:
```rust
pub mod cashu;
```

**Step 3: Verify compilation**

Run: `cargo check`
Expected: Compiles (may have warnings about unused code)

**Step 4: Commit**

```bash
git add src/services/cashu.rs src/services/mod.rs
git commit -m "feat: add Cashu service module for payment processing"
```

---

## Task 5: Initialize Wallet on Startup

**Files:**
- Modify: `src/main.rs`

**Step 1: Add wallet initialization in load_app_state**

After AppState construction, add:
```rust
// Initialize Cashu wallet if any paid feature is enabled
let cashu_wallet = if any_paid_feature {
    match crate::services::cashu::init_wallet(&cashu_wallet_path, &cashu_accepted_mints).await {
        Ok(wallet) => {
            info!("💰 Cashu wallet ready for payments");
            Some(wallet)
        }
        Err(e) => {
            error!("Failed to initialize Cashu wallet: {}", e);
            panic!("Cannot start with paid features enabled but wallet initialization failed");
        }
    }
} else {
    None
};
```

Then update the AppState construction to use this value instead of `None`.

**Step 2: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: initialize Cashu wallet on startup when paid features enabled"
```

---

## Task 6: Add Payment Check to Upload Handler

**Files:**
- Modify: `src/handlers/upload.rs`

**Step 1: Add payment verification helper**

Add at top of file:
```rust
use crate::services::cashu;

/// Check if payment is required and process it
///
/// Supports two flows:
/// 1. Preemptive: Client sends X-Cashu with first request (validated after size known)
/// 2. Reactive: Client gets 402, then retries with X-Cashu
async fn check_payment(
    state: &AppState,
    headers: &HeaderMap,
    size_bytes: u64,
    feature_enabled: bool,
) -> Result<(), AppError> {
    if !feature_enabled {
        return Ok(());
    }

    let required_sats = cashu::calculate_price(size_bytes, state.cashu_price_per_mb);

    // Check for X-Cashu header (preemptive or retry payment)
    let cashu_header = cashu::extract_cashu_header(headers);

    match cashu_header {
        None => {
            // No payment provided, return 402
            Err(AppError::PaymentRequired {
                amount_sats: required_sats,
                unit: "sat".to_string(),
                mints: state.cashu_accepted_mints.clone(),
            })
        }
        Some(token_str) => {
            // Parse and verify token
            let token = cashu::parse_token(&token_str)?;

            // Verify amount is sufficient for actual size
            cashu::verify_token_basics(&token, required_sats, &state.cashu_accepted_mints)?;

            // Receive token into wallet
            if let Some(wallet) = &state.cashu_wallet {
                cashu::receive_token(wallet, &token).await?;
            }

            Ok(())
        }
    }
}
```

**Step 2: Integrate into upload_file handler**

After streaming to temp file and getting size, add payment check:
```rust
// Stream to temp file and calculate hash
let body_stream = req.into_body().into_data_stream();
let (sha256, total_bytes) = upload::stream_to_temp_file(body_stream, &temp_path).await?;

// Check payment if required (after we know the size)
check_payment(&state, &headers, total_bytes, state.feature_paid_upload).await?;

// Validate authorization matches the hash
auth::validate_upload_auth(&auth_event, &sha256)?;
```

**Step 3: Integrate into mirror_blob handler**

After checking content_length, add payment check:
```rust
let content_length = response.content_length();
let max_size_bytes = state.max_upstream_download_size_mb * 1024 * 1024;

upload::check_size_limit(content_length, max_size_bytes)?;

// Check payment if required
if let Some(size) = content_length {
    check_payment(&state, &headers, size, state.feature_paid_mirror).await?;
}
```

**Step 4: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 5: Commit**

```bash
git add src/handlers/upload.rs
git commit -m "feat: add Cashu payment verification to upload and mirror handlers"
```

---

## Task 7: Add Payment Check to Download Handler

**Files:**
- Modify: `src/handlers/file_serving.rs`

**Step 1: Add cashu import and payment check**

Add import at top:
```rust
use crate::services::cashu;
```

**Step 2: Add payment check in handle_file_request**

After determining file exists and getting metadata, before serving:
```rust
// Check payment for download if required
if state.feature_paid_download {
    let required_sats = cashu::calculate_price(metadata.size, state.cashu_price_per_mb);

    let cashu_header = cashu::extract_cashu_header(&headers);

    match cashu_header {
        None => {
            return Err(AppError::PaymentRequired {
                amount_sats: required_sats,
                unit: "sat".to_string(),
                mints: state.cashu_accepted_mints.clone(),
            });
        }
        Some(token_str) => {
            let token = cashu::parse_token(&token_str)?;
            cashu::verify_token_basics(&token, required_sats, &state.cashu_accepted_mints)?;

            if let Some(wallet) = &state.cashu_wallet {
                cashu::receive_token(wallet, &token).await?;
            }
        }
    }
}
```

**Step 3: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/handlers/file_serving.rs
git commit -m "feat: add Cashu payment verification to download handler"
```

---

## Task 8: Add HEAD /upload for Price Discovery

**Files:**
- Modify: `src/main.rs`

**Step 1: Update head_upload handler**

Replace the placeholder `head_upload` function:
```rust
async fn head_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response<Body>, AppError> {
    use axum::http::header;

    // Return server capabilities and pricing info
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT, "application/octet-stream");

    // Add payment info if paid uploads enabled
    if state.feature_paid_upload {
        // Price per MB in sats
        response = response.header("X-Price-Per-MB", state.cashu_price_per_mb.to_string());
        response = response.header("X-Price-Unit", "sat");
        response = response.header("X-Accepted-Mints", state.cashu_accepted_mints.join(","));
    }

    Ok(response.body(Body::empty()).unwrap())
}
```

**Step 2: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add HEAD /upload endpoint for price discovery"
```

---

## Task 9: Update CLAUDE.md Documentation

**Files:**
- Modify: `CLAUDE.md`

**Step 1: Add BUD-07 documentation section**

Add new section after Authorization Configuration:
```markdown
#### Cashu Payment Configuration (BUD-07)
- `FEATURE_PAID_UPLOAD`: Enable paid uploads - `off` or `on` (default: `off`)
- `FEATURE_PAID_MIRROR`: Enable paid mirrors - `off` or `on` (default: `off`)
- `FEATURE_PAID_DOWNLOAD`: Enable paid downloads - `off` or `on` (default: `off`)
- `CASHU_PRICE_PER_MB`: Price per megabyte in satoshis (default: `1`)
- `CASHU_ACCEPTED_MINTS`: Comma-separated list of accepted Cashu mint URLs (required if any paid feature enabled)
- `CASHU_WALLET_PATH`: Path to SQLite wallet database (default: `./cashu_wallet.db`)
```

**Step 2: Add payment flow documentation**

Add new section:
```markdown
## Cashu Payments (BUD-07)

When paid features are enabled, the server implements the BUD-07/NUT-24 payment flow.

### Two Payment Flows Supported

**1. Reactive Flow (402 round-trip):**
1. Client makes request without payment
2. Server returns `HTTP 402 Payment Required` with `X-Cashu` header
3. Client obtains Cashu token from accepted mint
4. Client retries request with `X-Cashu: cashuB...` header
5. Server verifies token, receives it into wallet, proceeds

**2. Preemptive Flow (single request):**
1. Client calculates expected price (size_mb * CASHU_PRICE_PER_MB)
2. Client includes `X-Cashu: cashuB...` header with first request
3. Server validates amount is sufficient for actual size
4. If sufficient: receives token, proceeds with operation
5. If insufficient: returns 402 with remaining amount needed

### Price Discovery (HEAD /upload)

Clients can query pricing before uploading:
```bash
HEAD /upload
```
Response headers (when paid uploads enabled):
- `X-Price-Per-MB`: Price in sats per megabyte
- `X-Price-Unit`: Currency unit (always "sat")
- `X-Accepted-Mints`: Comma-separated list of accepted mint URLs

### Payment Request Format (X-Cashu header on 402 response)
```json
{
  "a": 10,           // Amount in sats
  "u": "sat",        // Unit
  "m": ["https://mint.example.com"]  // Accepted mints
}
```

### Payment Proof Format (X-Cashu header on request)
Standard Cashu token string starting with `cashuA` or `cashuB`.
```

**Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add BUD-07 Cashu payment configuration documentation"
```

---

## Task 10: Integration Testing

**Step 1: Test with paid features disabled (default)**

Run: `cargo run`
Expected: Server starts normally, no Cashu wallet initialization logged

**Step 2: Test configuration validation**

Run with invalid config:
```bash
FEATURE_PAID_UPLOAD=on cargo run
```
Expected: Panic with "CASHU_ACCEPTED_MINTS must be set"

**Step 3: Test with valid config**

```bash
FEATURE_PAID_UPLOAD=on \
CASHU_ACCEPTED_MINTS=https://mint.minibits.cash/Bitcoin \
CASHU_PRICE_PER_MB=1 \
cargo run
```
Expected: Server starts, logs "Cashu wallet initialized" and "Cashu payments enabled"

**Step 4: Test 402 response**

```bash
curl -X PUT http://localhost:3000/upload \
  -H "Authorization: Nostr <valid-auth>" \
  -H "Content-Type: application/octet-stream" \
  --data "test data"
```
Expected: HTTP 402 with X-Cashu header

**Step 5: Commit test results**

```bash
git add -A
git commit -m "feat: complete BUD-07 Cashu payment implementation"
```

---

## Summary

This implementation adds BUD-07 paid upload/download/mirror support:

- **3 feature flags**: `FEATURE_PAID_UPLOAD`, `FEATURE_PAID_MIRROR`, `FEATURE_PAID_DOWNLOAD`
- **Price config**: `CASHU_PRICE_PER_MB` (default 1 sat)
- **Mint config**: `CASHU_ACCEPTED_MINTS` (required when paid features on)
- **Wallet storage**: SQLite via CDK at `CASHU_WALLET_PATH`
- **HTTP 402**: Returns payment request in `X-Cashu` header
- **Token verification**: Checks mint, amount, then receives into wallet
- **Logging**: All payments logged with amounts

Files changed:
- `Cargo.toml` - dependencies
- `src/error.rs` - PaymentRequired variant
- `src/models.rs` - AppState fields
- `src/main.rs` - config parsing, wallet init
- `src/services/cashu.rs` - new service module
- `src/services/mod.rs` - export
- `src/handlers/upload.rs` - payment checks
- `src/handlers/file_serving.rs` - payment checks
- `CLAUDE.md` - documentation
