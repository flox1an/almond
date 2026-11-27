//! Durable paid-retention state and purchase journal.
//!
//! Lives in the Cashu wallet `SQLite` file next to CDK's own tables, under
//! `almond_*` names with an independent migration ledger. Every mutation
//! commits before the in-memory mirror changes, so cleanup can read the
//! mirror synchronously and never sees a promise that is not durable.
//!
//! Purchase lifecycle: `begin` (pending, before any mint call) → `mark_settled`
//! once the wallet holds the funds, or `mark_failed` only when no money was
//! taken → `apply` (atomic `paid_until = max(current, target)`). An operation
//! that is pending or settled protects its hash from eviction, so a timeout
//! leaves the blob protected instead of inviting a second charge.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};

use crate::error::{AppError, AppResult};

/// Applied in order, each once, recorded in `almond_migrations`.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE almond_paid_retention (
        sha256 TEXT PRIMARY KEY NOT NULL,
        paid_until INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE almond_purchase_operations (
        op_id TEXT PRIMARY KEY NOT NULL,
        sha256 TEXT NOT NULL,
        size INTEGER NOT NULL,
        paid_from INTEGER NOT NULL,
        paid_until INTEGER NOT NULL,
        amount INTEGER NOT NULL,
        unit TEXT NOT NULL,
        mint_url TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending', 'settled', 'applied', 'failed')),
        settlement TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE UNIQUE INDEX almond_purchase_operations_one_active
        ON almond_purchase_operations (sha256) WHERE state IN ('pending', 'settled');
"];

const OP_COLUMNS: &str = "op_id, sha256, size, paid_from, paid_until, amount, unit, mint_url, \
                          state, settlement, created_at, updated_at";

/// Immutable terms of one purchase, fixed before money moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurchaseTerms {
    /// Caller-chosen idempotency key, e.g. a digest of the canonical proof
    /// identities. Never the bearer token itself.
    pub op_id: String,
    pub sha256: String,
    pub size: u64,
    /// Start of the priced interval; `begin` rejects it once already covered.
    pub paid_from: u64,
    /// Absolute retention target granted on `apply`.
    pub paid_until: u64,
    pub amount: u64,
    pub unit: String,
    pub mint_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpState {
    /// Intent recorded; the mint outcome is not known to be final.
    Pending,
    /// Funds are in the wallet; entitlement not yet applied.
    Settled,
    Applied,
    /// Definitively no funds taken.
    Failed,
}

impl OpState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Settled => "settled",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "settled" => Self::Settled,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    8,
                    format!("state {other}"),
                    rusqlite::types::Type::Text,
                ))
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub terms: PurchaseTerms,
    pub state: OpState,
    /// Opaque wallet linkage (transaction/saga id). Never a token.
    pub settlement: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    /// A new pending operation was recorded; the caller may now charge.
    Started,
    /// The op id is already known: return its outcome, never charge again.
    Existing(Operation),
}

#[derive(Default)]
struct Mirror {
    paid_until: HashMap<String, u64>,
    /// Hashes with a pending or settled operation.
    active: HashSet<String>,
}

pub struct PaidStore {
    conn: Mutex<Connection>,
    mirror: RwLock<Mirror>,
}

fn db_error(error: rusqlite::Error) -> AppError {
    AppError::InternalError(format!("Paid retention database error: {error}"))
}

fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<Operation> {
    Ok(Operation {
        terms: PurchaseTerms {
            op_id: row.get(0)?,
            sha256: row.get(1)?,
            size: row.get(2)?,
            paid_from: row.get(3)?,
            paid_until: row.get(4)?,
            amount: row.get(5)?,
            unit: row.get(6)?,
            mint_url: row.get(7)?,
        },
        state: OpState::parse(&row.get::<_, String>(8)?)?,
        settlement: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn load_operation(conn: &Connection, op_id: &str) -> AppResult<Option<Operation>> {
    conn.query_row(
        &format!("SELECT {OP_COLUMNS} FROM almond_purchase_operations WHERE op_id = ?1"),
        [op_id],
        row_to_operation,
    )
    .optional()
    .map_err(db_error)
}

fn require_operation(conn: &Connection, op_id: &str) -> AppResult<Operation> {
    load_operation(conn, op_id)?
        .ok_or_else(|| AppError::NotFound(format!("Unknown purchase operation {op_id}")))
}

fn set_state(
    conn: &Connection,
    op_id: &str,
    state: OpState,
    settlement: Option<&str>,
    now: u64,
) -> AppResult<()> {
    conn.execute(
        "UPDATE almond_purchase_operations
         SET state = ?2, settlement = COALESCE(?3, settlement), updated_at = ?4
         WHERE op_id = ?1",
        params![op_id, state.as_str(), settlement, now],
    )
    .map_err(db_error)?;
    Ok(())
}

/// Monotonic upsert; returns the resulting `paid_until`.
fn upsert_paid_until(conn: &Connection, sha256: &str, until: u64, now: u64) -> AppResult<u64> {
    conn.query_row(
        "INSERT INTO almond_paid_retention (sha256, paid_until, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (sha256) DO UPDATE SET
             paid_until = max(paid_until, excluded.paid_until),
             updated_at = excluded.updated_at
         RETURNING paid_until",
        params![sha256, until, now],
        |row| row.get(0),
    )
    .map_err(db_error)
}

fn migrate(conn: &mut Connection, now: u64) -> AppResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS almond_migrations (
             version INTEGER PRIMARY KEY NOT NULL,
             applied_at INTEGER NOT NULL
         )",
    )
    .map_err(db_error)?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as u64 + 1;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let applied = tx
            .query_row(
                "SELECT 1 FROM almond_migrations WHERE version = ?1",
                [version],
                |_| Ok(()),
            )
            .optional()
            .map_err(db_error)?
            .is_some();
        if !applied {
            tx.execute_batch(sql).map_err(db_error)?;
            tx.execute(
                "INSERT INTO almond_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, now],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)?;
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl PaidStore {
    /// Open (creating if needed) the store inside the wallet database file and
    /// load every durable promise into memory. Callers must refuse to start on
    /// error: without it, cleanup could evict paid blobs.
    pub async fn open(path: &Path) -> AppResult<Arc<Self>> {
        let path: PathBuf = path.to_owned();
        tokio::task::spawn_blocking(move || Self::open_blocking(&path))
            .await
            .map_err(|error| {
                AppError::InternalError(format!("Paid store open panicked: {error}"))
            })?
    }

    fn open_blocking(path: &Path) -> AppResult<Arc<Self>> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                AppError::IoError(format!(
                    "Failed to create paid store directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut conn = Connection::open(path).map_err(db_error)?;
        // Same settings CDK uses on this file; FULL makes each commit durable.
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(db_error)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_error)?;
        migrate(&mut conn, unix_now())?;

        let mut mirror = Mirror::default();
        {
            let mut statement = conn
                .prepare("SELECT sha256, paid_until FROM almond_paid_retention")
                .map_err(db_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })
                .map_err(db_error)?;
            for row in rows {
                let (sha256, until) = row.map_err(db_error)?;
                mirror.paid_until.insert(sha256, until);
            }
            let mut statement = conn
                .prepare(
                    "SELECT sha256 FROM almond_purchase_operations
                     WHERE state IN ('pending', 'settled')",
                )
                .map_err(db_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db_error)?;
            for row in rows {
                mirror.active.insert(row.map_err(db_error)?);
            }
        }
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            mirror: RwLock::new(mirror),
        }))
    }

    /// Run one blocking DB job off the async runtime. The mirror is updated by
    /// the job after commit, under the connection lock, so mirror order equals
    /// commit order even if the awaiting caller is cancelled.
    async fn run<T, F>(self: &Arc<Self>, job: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &RwLock<Mirror>) -> AppResult<T> + Send + 'static,
    {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
            job(&mut conn, &store.mirror)
        })
        .await
        .map_err(|error| AppError::InternalError(format!("Paid store job panicked: {error}")))?
    }

    #[must_use]
    pub fn paid_until(&self, sha256: &str) -> Option<u64> {
        self.mirror
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .paid_until
            .get(sha256)
            .copied()
    }

    /// Whether ordinary cleanup must keep this hash: paid coverage remains or a
    /// purchase is unresolved.
    #[must_use]
    pub fn is_protected(&self, sha256: &str, now: u64) -> bool {
        let mirror = self.mirror.read().unwrap_or_else(PoisonError::into_inner);
        mirror.active.contains(sha256)
            || mirror
                .paid_until
                .get(sha256)
                .is_some_and(|until| *until > now)
    }

    /// Record purchase intent before any mint call. Returns `Existing` for a
    /// known op id; `Conflict` when another purchase of the hash is unresolved
    /// or `paid_from` is already covered (a stale quote must not be charged).
    pub async fn begin(self: &Arc<Self>, terms: PurchaseTerms, now: u64) -> AppResult<Begin> {
        if terms.paid_until <= terms.paid_from {
            return Err(AppError::BadRequest(
                "Purchase must end after it starts".to_string(),
            ));
        }
        self.run(move |conn, mirror| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            if let Some(existing) = load_operation(&tx, &terms.op_id)? {
                if existing.terms != terms {
                    return Err(AppError::Conflict(
                        "Purchase operation id reused with different terms".to_string(),
                    ));
                }
                return Ok(Begin::Existing(existing));
            }
            let current: Option<u64> = tx
                .query_row(
                    "SELECT paid_until FROM almond_paid_retention WHERE sha256 = ?1",
                    [&terms.sha256],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_error)?;
            if let Some(current) = current.filter(|current| *current > terms.paid_from) {
                return Err(AppError::Conflict(format!(
                    "Blob is already paid until {current}; request a new quote"
                )));
            }
            let inserted = tx.execute(
                &format!(
                    "INSERT INTO almond_purchase_operations ({OP_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, ?9, ?9)"
                ),
                params![
                    terms.op_id,
                    terms.sha256,
                    terms.size,
                    terms.paid_from,
                    terms.paid_until,
                    terms.amount,
                    terms.unit,
                    terms.mint_url,
                    now
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == ErrorCode::ConstraintViolation =>
                {
                    return Err(AppError::Conflict(
                        "Another purchase for this blob is in progress".to_string(),
                    ));
                }
                Err(error) => return Err(db_error(error)),
            }
            tx.commit().map_err(db_error)?;
            mirror
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .insert(terms.sha256);
            Ok(Begin::Started)
        })
        .await
    }

    pub async fn operation(self: &Arc<Self>, op_id: &str) -> AppResult<Option<Operation>> {
        let op_id = op_id.to_owned();
        self.run(move |conn, _| load_operation(conn, &op_id)).await
    }

    /// Pending or settled operations, for startup reconciliation.
    pub async fn unresolved(self: &Arc<Self>) -> AppResult<Vec<Operation>> {
        self.run(|conn, _| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {OP_COLUMNS} FROM almond_purchase_operations
                     WHERE state IN ('pending', 'settled') ORDER BY created_at"
                ))
                .map_err(db_error)?;
            let rows = statement
                .query_map([], row_to_operation)
                .map_err(db_error)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
        })
        .await
    }

    /// The wallet holds the funds. Idempotent for settled/applied operations.
    pub async fn mark_settled(
        self: &Arc<Self>,
        op_id: &str,
        settlement: Option<String>,
        now: u64,
    ) -> AppResult<Operation> {
        let op_id = op_id.to_owned();
        self.run(move |conn, _| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let operation = require_operation(&tx, &op_id)?;
            match operation.state {
                OpState::Settled | OpState::Applied => return Ok(operation),
                OpState::Failed => {
                    return Err(AppError::Conflict(format!(
                        "Purchase operation {op_id} was recorded as failed; reconcile manually"
                    )))
                }
                OpState::Pending => {}
            }
            set_state(&tx, &op_id, OpState::Settled, settlement.as_deref(), now)?;
            let operation = require_operation(&tx, &op_id)?;
            tx.commit().map_err(db_error)?;
            Ok(operation)
        })
        .await
    }

    /// Only for a definitive "no funds taken" outcome; never for a timeout.
    pub async fn mark_failed(self: &Arc<Self>, op_id: &str, now: u64) -> AppResult<Operation> {
        let op_id = op_id.to_owned();
        self.run(move |conn, mirror| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let operation = require_operation(&tx, &op_id)?;
            match operation.state {
                OpState::Failed => return Ok(operation),
                OpState::Settled | OpState::Applied => {
                    return Err(AppError::Conflict(format!(
                        "Purchase operation {op_id} is already settled"
                    )))
                }
                OpState::Pending => {}
            }
            set_state(&tx, &op_id, OpState::Failed, None, now)?;
            let operation = require_operation(&tx, &op_id)?;
            tx.commit().map_err(db_error)?;
            mirror
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .remove(&operation.terms.sha256);
            Ok(operation)
        })
        .await
    }

    /// Grant a settled purchase once: `paid_until = max(current, target)` and
    /// mark applied in one transaction. Re-applying returns the current value.
    pub async fn apply(self: &Arc<Self>, op_id: &str, now: u64) -> AppResult<u64> {
        let op_id = op_id.to_owned();
        self.run(move |conn, mirror| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let operation = require_operation(&tx, &op_id)?;
            let sha256 = operation.terms.sha256;
            match operation.state {
                OpState::Applied => {
                    return tx
                        .query_row(
                            "SELECT paid_until FROM almond_paid_retention WHERE sha256 = ?1",
                            [&sha256],
                            |row| row.get(0),
                        )
                        .map_err(db_error)
                }
                OpState::Pending | OpState::Failed => {
                    return Err(AppError::Conflict(format!(
                        "Purchase operation {op_id} is not settled"
                    )))
                }
                OpState::Settled => {}
            }
            let until = upsert_paid_until(&tx, &sha256, operation.terms.paid_until, now)?;
            set_state(&tx, &op_id, OpState::Applied, None, now)?;
            tx.commit().map_err(db_error)?;
            let mut mirror = mirror.write().unwrap_or_else(PoisonError::into_inner);
            mirror.paid_until.insert(sha256.clone(), until);
            mirror.active.remove(&sha256);
            Ok(until)
        })
        .await
    }

    /// Operator grant outside the purchase journal; monotonic.
    pub async fn extend(self: &Arc<Self>, sha256: &str, until: u64, now: u64) -> AppResult<u64> {
        let sha256 = sha256.to_owned();
        self.run(move |conn, mirror| {
            let until = upsert_paid_until(conn, &sha256, until, now)?;
            mirror
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .paid_until
                .insert(sha256, until);
            Ok(until)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(op_id: &str, sha256: &str, paid_from: u64, paid_until: u64) -> PurchaseTerms {
        PurchaseTerms {
            op_id: op_id.to_owned(),
            sha256: sha256.to_owned(),
            size: 10,
            paid_from,
            paid_until,
            amount: 21,
            unit: "sat".to_owned(),
            mint_url: "https://mint.example".to_owned(),
        }
    }

    fn scratch_db() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("almond-paid-{}", uuid::Uuid::new_v4()));
        let db = dir.join("wallet.db");
        (dir, db)
    }

    #[tokio::test]
    async fn purchase_lifecycle_is_idempotent_and_survives_restart() {
        let (dir, db) = scratch_db();
        std::fs::create_dir_all(&dir).unwrap();
        // CDK and Almond migrations coexist in one wallet file.
        drop(
            cdk_sqlite::wallet::WalletSqliteDatabase::new(&db)
                .await
                .unwrap(),
        );
        let store = PaidStore::open(&db).await.unwrap();
        let hash = "a".repeat(64);
        assert!(!store.is_protected(&hash, 0));

        let first = terms("op-1", &hash, 100, 200);
        assert_eq!(
            store.begin(first.clone(), 100).await.unwrap(),
            Begin::Started
        );
        assert!(store.is_protected(&hash, 1_000), "pending protects");
        assert!(matches!(
            store.begin(terms("op-2", &hash, 100, 300), 100).await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            store.apply("op-1", 100).await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            store.begin(first.clone(), 100).await.unwrap(),
            Begin::Existing(Operation {
                state: OpState::Pending,
                ..
            })
        ));

        store
            .mark_settled("op-1", Some("tx".to_owned()), 101)
            .await
            .unwrap();
        assert!(matches!(
            store.mark_failed("op-1", 101).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(store.apply("op-1", 102).await.unwrap(), 200);
        assert_eq!(store.apply("op-1", 103).await.unwrap(), 200);
        assert!(store.is_protected(&hash, 199));
        assert!(!store.is_protected(&hash, 200));
        // A quote overlapping paid time must not be charged.
        assert!(matches!(
            store.begin(terms("op-3", &hash, 150, 400), 150).await,
            Err(AppError::Conflict(_))
        ));
        // Extension never shortens.
        assert_eq!(store.extend(&hash, 50, 104).await.unwrap(), 200);

        let other = "b".repeat(64);
        store.begin(terms("op-4", &other, 0, 10), 0).await.unwrap();
        drop(store);

        let reopened = PaidStore::open(&db).await.unwrap();
        assert_eq!(reopened.paid_until(&hash), Some(200));
        assert!(
            reopened.is_protected(&other, u64::MAX),
            "pending survives restart"
        );
        let unresolved = reopened.unresolved().await.unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].terms.op_id, "op-4");
        reopened.mark_failed("op-4", 5).await.unwrap();
        assert!(!reopened.is_protected(&other, 0));
        assert!(matches!(
            reopened.mark_settled("op-4", None, 6).await,
            Err(AppError::Conflict(_))
        ));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
