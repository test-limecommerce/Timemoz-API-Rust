//! Port `src/auth/login-attempts.service.ts` — batas percobaan login per username
//! (audit H3). Kunci Redis SAMA dengan Node (`la:<scope>:<sha256(lower(trim(u)))[..32]>`),
//! jadi Node dan Rust berbagi satu hitungan selama berdampingan.
//!
//! Dihitung SEBELUM bcrypt (INCR atomik lebih dulu, bukan cek-lalu-catat), dan
//! gagal-terbuka: Redis mati / lambat (>1,5 dtk) = login tetap jalan.

use std::time::Duration;

use fred::prelude::*;
use mz_core::AppState;
use sha2::{Digest, Sha256};

/// Pesan saat jatah habis; sama untuk akun yang ada maupun tidak.
pub const LOGIN_LOCKED_MESSAGE: &str =
    "Too many failed login attempts. Please try again in 15 minutes.";

pub const MAX_ATTEMPTS: i64 = 10;
pub const WINDOW_SECONDS: i64 = 15 * 60;
const OP_TIMEOUT: Duration = Duration::from_millis(1500);

/// `MULTI; SET key 0 NX EX <window>; INCR key; EXEC` dalam satu skrip Lua
/// (atomik, efek sama): TTL dipasang di percobaan pertama dan tidak pernah hilang.
const CONSUME_SCRIPT: &str = r#"
redis.call("SET", KEYS[1], "0", "NX", "EX", tonumber(ARGV[1]))
return redis.call("INCR", KEYS[1])
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Operator,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Operator => "operator",
        }
    }
}

pub fn key(scope: Scope, username: &str) -> String {
    let mut digest = hex::encode(Sha256::digest(username.trim().to_lowercase().as_bytes()));
    digest.truncate(32);
    format!("la:{}:{digest}", scope.as_str())
}

/// Jatah masih ada setelah percobaan ke-`count`?
pub fn within_limit(count: i64) -> bool {
    count <= MAX_ATTEMPTS
}

/// Catat satu percobaan; `false` bila jatah jendela ini sudah habis.
pub async fn consume(state: &AppState, scope: Scope, username: &str) -> bool {
    let key = key(scope, username);
    let op = state.redis.next().eval::<i64, _, _, _>(
        CONSUME_SCRIPT,
        vec![key],
        vec![WINDOW_SECONDS.to_string()],
    );
    match tokio::time::timeout(OP_TIMEOUT, op).await {
        Ok(Ok(count)) => within_limit(count),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "Login attempt counter unavailable, allowing request");
            true
        }
        Err(_) => {
            tracing::warn!(
                "Login attempt counter unavailable, allowing request: Redis operation timed out"
            );
            true
        }
    }
}

/// Dipanggil setelah password terbukti benar.
pub async fn reset(state: &AppState, scope: Scope, username: &str) {
    let op = state.redis.next().del::<i64, _>(key(scope, username));
    match tokio::time::timeout(OP_TIMEOUT, op).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "Failed to reset login attempt counter"),
        Err(_) => {
            tracing::warn!("Failed to reset login attempt counter: Redis operation timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_matches_node_digest() {
        // sha256("agus@shieldtag.co") — sama dengan createHash('sha256') Node.
        let full = hex::encode(Sha256::digest(b"agus@shieldtag.co"));
        let expected = format!("la:user:{}", full.get(..32).unwrap_or_default());
        assert_eq!(key(Scope::User, "  Agus@Shieldtag.co "), expected);
        assert!(key(Scope::Operator, "x").starts_with("la:operator:"));
        assert_eq!(key(Scope::User, "a").len(), "la:user:".len() + 32);
    }

    #[test]
    fn lockout_after_ten_attempts() {
        assert!(within_limit(1));
        assert!(within_limit(MAX_ATTEMPTS));
        assert!(!within_limit(MAX_ATTEMPTS + 1));
        assert_eq!(WINDOW_SECONDS, 900);
    }
}
