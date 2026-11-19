//! Galat yang di Node "dilempar lalu ditangkap" (`catch (error) { return
//! { success: false, message: error.message, ... } }`) dibedakan dari galat
//! HTTP yang memang dilempar ulang ke klien.

use std::future::Future;
use std::time::{Duration, Instant};

use mz_core::ApiError;

/// `throw` di dalam `try` service.
#[derive(Debug)]
pub enum Thrown {
    /// `throw new Error(msg)` / TypeError JS yang ditiru.
    Msg(String),
    /// Galat Prisma/DB.
    Db(sqlx::Error),
    /// `PrismaClientValidationError` (mis. `startAt` bukan ISO-8601 penuh). Respons Node
    /// memuat objek error-nya di `data`: `{ clientVersion, name }`.
    Prisma(String),
    /// Transaksi melewati `TIMELINE_TX_OPTIONS` (P2028).
    TxTimeout,
    /// `HttpException` yang dilempar ulang (`if (error instanceof HttpException) throw error`).
    Http(ApiError),
}

impl From<sqlx::Error> for Thrown {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl From<ApiError> for Thrown {
    fn from(e: ApiError) -> Self {
        Self::Http(e)
    }
}

impl Thrown {
    pub fn msg(m: impl Into<String>) -> Self {
        Self::Msg(m.into())
    }

    /// `error.message`.
    pub fn message(&self) -> String {
        match self {
            Self::Msg(m) | Self::Prisma(m) => m.clone(),
            Self::Db(e) => {
                tracing::error!(error = %e, "tracking-timelines db error");
                e.to_string()
            }
            Self::TxTimeout => "Transaction API error: Transaction already closed: \
                A query cannot be executed on an expired transaction."
                .to_owned(),
            Self::Http(e) => e.to_string(),
        }
    }

    /// `data` respons gagal = `JSON.stringify(error)` Node: properti enumerable error.
    /// Hanya error Prisma yang punya (`clientVersion`, `name`); `Error` biasa → `{}`.
    pub fn data(&self) -> serde_json::Value {
        match self {
            Self::Prisma(_) => serde_json::json!({
                "clientVersion": "5.22.0",
                "name": "PrismaClientValidationError",
            }),
            _ => serde_json::json!({}),
        }
    }

    /// `isTransientDbError` (P1001/P1002/P1008/P1017/P2024/P2028/P2034).
    fn is_transient(&self) -> bool {
        match self {
            Self::TxTimeout => true,
            Self::Db(e) => match e {
                sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) | sqlx::Error::PoolClosed => true,
                sqlx::Error::Database(db) => matches!(
                    db.code().as_deref(),
                    // deadlock, serialization, koneksi putus, admin shutdown
                    Some("40P01" | "40001" | "57P01" | "08000" | "08003" | "08006")
                ),
                _ => false,
            },
            _ => false,
        }
    }
}

pub type TResult<T> = Result<T, Thrown>;

/// `TIMELINE_TX_OPTIONS`: maxWait 5 dtk + timeout 10 dtk.
const TX_BUDGET: Duration = Duration::from_secs(15);
/// `TIMELINE_TX_RETRY.retryWindowMs`.
const RETRY_WINDOW: Duration = Duration::from_millis(4_000);

/// Satu percobaan transaksi dengan batas waktu Prisma.
pub async fn with_tx_timeout<T, F>(fut: F) -> TResult<T>
where
    F: Future<Output = TResult<T>>,
{
    tokio::time::timeout(TX_BUDGET, fut)
        .await
        .unwrap_or(Err(Thrown::TxTimeout))
}

/// `retryTransientDbError(label, fn, TIMELINE_TX_RETRY)`: 3 percobaan, jeda
/// 150 ms × 2^(n-1) + jitter, percobaan baru hanya dalam 4 dtk pertama.
pub async fn retry_transient<T, F, Fut>(label: &str, mut f: F) -> TResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = TResult<T>>,
{
    const ATTEMPTS: u32 = 3;
    const BASE_MS: u64 = 150;
    let started = Instant::now();
    let mut attempt = 1;
    loop {
        match with_tx_timeout(f()).await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if attempt >= ATTEMPTS || !err.is_transient() {
                    return Err(err);
                }
                let jitter = rand::random::<u64>() % BASE_MS;
                let delay = Duration::from_millis(BASE_MS * (1 << (attempt - 1)) + jitter);
                if started.elapsed() + delay >= RETRY_WINDOW {
                    return Err(err);
                }
                tracing::warn!(
                    label,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retry transient db error"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}
