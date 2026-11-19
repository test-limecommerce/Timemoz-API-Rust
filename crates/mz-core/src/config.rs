//! Konfigurasi dari variabel lingkungan. Nama variabel SAMA dengan Mudah-API,
//! jadi satu berkas `.env` bisa dipakai kedua proses selama masa transisi.

use std::env;

/// Variabel lain dibaca lewat [`Config::var`] oleh crate domain.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub db_max_connections: u32,
    pub redis_host: String,
    pub redis_port: u16,
    pub redis_password: Option<String>,
    /// `REDIS_DB` — indeks database Redis (khusus Rust; Node selalu 0). Test memakai 15.
    pub redis_db: Option<u8>,
    /// `SECRET_TOKEN` — secret HS256 untuk JWT user (sama dengan Node).
    pub jwt_secret: String,
    /// `JWTEXPIRED_IN` (mis. `7d`). Hanya dipakai saat MENERBITKAN token; saat
    /// memverifikasi, expiry diabaikan (kebijakan pemilik, lihat CLAUDE.md Node).
    pub jwt_expires_in: Option<String>,
    /// `ROUNDSOFHASHING` — cost bcrypt.
    pub bcrypt_cost: u32,
    /// `RATE_LIMIT_MAX` — batas default limiter ber-token (bawaan 20/menit).
    pub rate_limit_max: u64,
    pub free_subscription_days: Option<i64>,
    pub free_subscription_grace_days: i64,
    pub manual_subscription_grace_days: i64,
    /// Nilai [`Config::var`] yang menang atas env proses. Hanya diisi test
    /// (`mz-testkit`), supaya test tidak perlu mengubah env proses.
    pub var_overrides: std::collections::HashMap<String, String>,
}

fn var_opt(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Meniru `/^\d+$/` + `Number(raw)` di `free-subscription.ts`.
fn digits(key: &str) -> Option<i64> {
    let raw = var_opt(key)?;
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url =
            var_opt("DATABASE_URL").ok_or_else(|| anyhow::anyhow!("DATABASE_URL wajib diisi"))?;
        let jwt_secret =
            var_opt("SECRET_TOKEN").ok_or_else(|| anyhow::anyhow!("SECRET_TOKEN wajib diisi"))?;

        Ok(Self {
            port: var_opt("RS_PORT")
                .or_else(|| var_opt("PORT"))
                .and_then(|p| p.parse().ok())
                .unwrap_or(3100),
            database_url,
            db_max_connections: var_opt("RS_DB_MAX_CONNECTIONS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            redis_host: var_opt("REDIS_HOST").unwrap_or_else(|| "localhost".into()),
            redis_port: var_opt("REDIS_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(6379),
            redis_password: var_opt("REDIS_PASSWORD"),
            redis_db: var_opt("REDIS_DB").and_then(|v| v.trim().parse().ok()),
            jwt_secret,
            jwt_expires_in: var_opt("JWTEXPIRED_IN"),
            bcrypt_cost: var_opt("ROUNDSOFHASHING")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            rate_limit_max: var_opt("RATE_LIMIT_MAX")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            free_subscription_days: digits("FREE_SUBSCRIPTION_DAYS").filter(|d| *d > 0),
            free_subscription_grace_days: digits("FREE_SUBSCRIPTION_GRACE_DAYS").unwrap_or(3),
            manual_subscription_grace_days: digits("MANUAL_SUBSCRIPTION_GRACE_DAYS").unwrap_or(7),
            var_overrides: std::collections::HashMap::new(),
        })
    }

    /// Variabel lain (FRONTEND_URL, HOST_FE, MAIL_FROM, ...). Kosong = `None`,
    /// sama seperti `process.env.X || ...` di Node.
    pub fn var(&self, key: &str) -> Option<String> {
        if let Some(v) = self.var_overrides.get(key) {
            return Some(v.clone()).filter(|v| !v.trim().is_empty());
        }
        var_opt(key)
    }

    /// `CACHE_INVALIDATION_ENABLED` — bawaan aktif; hanya `false` (tak peka huruf) mematikan.
    pub fn cache_invalidation_enabled(&self) -> bool {
        !self
            .var("CACHE_INVALIDATION_ENABLED")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("false"))
    }

    /// `CACHE_INVALIDATION_CHANNEL` — channel Pub/Sub invalidasi cache (lihat
    /// `plan/cache-invalidation-protocol.md`).
    pub fn cache_invalidation_channel(&self) -> String {
        self.var("CACHE_INVALIDATION_CHANNEL")
            .map_or_else(|| "mz:cache-invalidate".to_owned(), |v| v.trim().to_owned())
    }

    /// Untuk test.
    pub fn for_test() -> Self {
        Self {
            port: 0,
            database_url: String::new(),
            db_max_connections: 1,
            redis_host: "localhost".into(),
            redis_port: 6379,
            redis_password: None,
            redis_db: None,
            jwt_secret: "test-secret".into(),
            jwt_expires_in: None,
            bcrypt_cost: 4,
            rate_limit_max: 20,
            free_subscription_days: None,
            free_subscription_grace_days: 3,
            manual_subscription_grace_days: 7,
            var_overrides: std::collections::HashMap::new(),
        }
    }
}
