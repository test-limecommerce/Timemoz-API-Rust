//! Port `src/auth/password-check.ts` + helper bcrypt (`BcryptFactory`).
//!
//! bcrypt itu CPU-bound (~50-300 ms), jadi semuanya lewat `spawn_blocking`.
//! Hash Node (`$2b$`) dan hash Rust saling terverifikasi.

use mz_core::{ApiError, ApiResult, AppState};
use tokio::sync::OnceCell;

/// Satu pesan untuk username tak terdaftar dan password salah (audit H3).
pub const INVALID_CREDENTIALS_MESSAGE: &str = "Invalid username or password";

/// Password yang di-hash untuk menyamakan waktu respons akun yang tidak ada.
const TIMING_EQUALIZER: &str = "timing-equalizer";

static DUMMY_HASH: OnceCell<String> = OnceCell::const_new();

async fn blocking_hash(plain: String, cost: u32) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || bcrypt::hash(plain, cost))
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("bcrypt task: {e}")))?
        .map_err(|e| ApiError::internal(anyhow::anyhow!("bcrypt hash: {e}")))
}

/// `bcrypt.hash(plain, ROUNDSOFHASHING)`. Cost dari `state.config.bcrypt_cost`.
pub async fn hash_password(state: &AppState, plain: &str) -> ApiResult<String> {
    hash_with_cost(plain, state.config.bcrypt_cost).await
}

/// Seperti [`hash_password`] tapi dengan cost eksplisit (untuk test / konfigurasi khusus).
pub async fn hash_with_cost(plain: &str, cost: u32) -> ApiResult<String> {
    blocking_hash(plain.to_owned(), cost).await
}

/// `bcrypt.compare(plain, hash)`. Hash rusak / tidak dikenal = `false` (bukan error),
/// sama dengan `comparePasswordSafely` untuk hash yang rusak.
pub async fn verify_password(plain: &str, hash: &str) -> bool {
    let (plain, hash) = (plain.to_owned(), hash.to_owned());
    tokio::task::spawn_blocking(move || bcrypt::verify(plain, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// Cost yang dipakai hash pengganti: sama dengan `getDummyHash` Node
/// (`ROUNDSOFHASHING` valid hanya 4..=15, selain itu 10).
fn dummy_cost(cost: u32) -> u32 {
    if (4..=15).contains(&cost) { cost } else { 10 }
}

async fn dummy_hash(cost: u32) -> Option<&'static String> {
    DUMMY_HASH
        .get_or_try_init(|| async {
            blocking_hash(TIMING_EQUALIZER.to_owned(), dummy_cost(cost)).await
        })
        .await
        .ok()
}

/// `comparePasswordSafely` dengan cost eksplisit. Hash kosong/`None` (user INVITED
/// tanpa password) tetap membayar satu perbandingan bcrypt ke hash pengganti lalu `false`.
pub async fn compare_password_safely_with_cost(
    cost: u32,
    password: &str,
    hash: Option<&str>,
) -> bool {
    match hash {
        Some(h) if !h.is_empty() => verify_password(password, h).await,
        _ => {
            if let Some(dummy) = dummy_hash(cost).await {
                let _ = verify_password(password, dummy).await;
            }
            false
        }
    }
}

/// `comparePasswordSafely(bcrypt, password, hash)`.
pub async fn compare_password_safely(state: &AppState, password: &str, hash: Option<&str>) -> bool {
    compare_password_safely_with_cost(state.config.bcrypt_cost, password, hash).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dihasilkan Node: `bcrypt.hashSync('hunter2-Secret', 4)`.
    const NODE_HASH: &str = "$2b$04$EvJLiqdXZiJN69nFj/b3LOzPH9bLleTt0bllMux8K7vgPUAMPzkJK";

    #[tokio::test]
    async fn node_hash_verifies() {
        assert!(verify_password("hunter2-Secret", NODE_HASH).await);
        assert!(!verify_password("wrong", NODE_HASH).await);
    }

    #[tokio::test]
    async fn rust_hash_roundtrip_is_2b() {
        let h = hash_with_cost("abc12345", 4).await.unwrap_or_default();
        assert!(h.starts_with("$2b$04$"), "{h}");
        assert!(verify_password("abc12345", &h).await);
    }

    #[tokio::test]
    async fn missing_or_broken_hash_is_false_not_error() {
        assert!(!compare_password_safely_with_cost(4, "x", None).await);
        assert!(!compare_password_safely_with_cost(4, "x", Some("")).await);
        assert!(!compare_password_safely_with_cost(4, "x", Some("not-a-bcrypt-hash")).await);
    }

    #[tokio::test]
    async fn existing_hash_path_matches() {
        assert!(compare_password_safely_with_cost(4, "hunter2-Secret", Some(NODE_HASH)).await);
        assert!(!compare_password_safely_with_cost(4, "nope", Some(NODE_HASH)).await);
    }

    #[test]
    fn dummy_cost_range() {
        assert_eq!(dummy_cost(3), 10);
        assert_eq!(dummy_cost(16), 10);
        assert_eq!(dummy_cost(12), 12);
    }
}
