//! Port `src/users/invite-token.ts` — token link undangan `v1.<exp>.<sig>`.
//!
//! `sig` = HMAC-SHA256 (base64url tanpa padding) atas `v1|<uuid>|<exp>` dengan kunci
//! turunan `HMAC-SHA256(SECRET_TOKEN, "mudah:user-invite-token:v1")`. Format byte-per-byte
//! sama dengan Node, jadi token yang dibuat satu sisi diterima sisi lain.
//! Secret = `state.config.jwt_secret` (= `SECRET_TOKEN`).

use mz_core::{ApiError, AppState};
use sha2::{Digest, Sha256};

use crate::util::{encode_uri_component, env_or_undefined};

/// Masa berlaku link undangan.
pub const INVITE_TOKEN_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

const VERSION: &str = "v1";
const KEY_LABEL: &str = "mudah:user-invite-token:v1";
const FUTURE_SKEW_SECONDS: i64 = 60;
const SIG_LEN: usize = 43;

/// `InviteTokenSecretMissingError` — jadi 500 seperti Error biasa di Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteTokenSecretMissing;

impl std::fmt::Display for InviteTokenSecretMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SECRET_TOKEN is not set; cannot sign invite tokens")
    }
}

impl std::error::Error for InviteTokenSecretMissing {}

impl From<InviteTokenSecretMissing> for ApiError {
    fn from(err: InviteTokenSecretMissing) -> Self {
        ApiError::internal(anyhow::anyhow!("{err}"))
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let hashed;
    let key = if key.len() > BLOCK {
        hashed = Sha256::digest(key);
        hashed.as_slice()
    } else {
        key
    };
    let mut k = [0u8; BLOCK];
    for (slot, byte) in k.iter_mut().zip(key) {
        *slot = *byte;
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner_digest);
    outer.finalize().into()
}

fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        let pick = |shift: u32| {
            ALPHABET
                .get(((n >> shift) & 63) as usize)
                .copied()
                .map_or('?', char::from)
        };
        out.push(pick(18));
        out.push(pick(12));
        if chunk.len() > 1 {
            out.push(pick(6));
        }
        if chunk.len() > 2 {
            out.push(pick(0));
        }
    }
    out
}

fn derive_key(secret: &str) -> Option<[u8; 32]> {
    if secret.is_empty() {
        return None;
    }
    Some(hmac_sha256(secret.as_bytes(), KEY_LABEL.as_bytes()))
}

fn sign(key: &[u8; 32], user_uuid: &str, exp: i64) -> String {
    base64url_nopad(&hmac_sha256(
        key,
        format!("{VERSION}|{user_uuid}|{exp}").as_bytes(),
    ))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `signInviteToken(userUuid, nowMs)`. Secret kosong = galat keras.
pub fn sign_invite_token(
    secret: &str,
    user_uuid: &str,
    now_ms: i64,
) -> Result<String, InviteTokenSecretMissing> {
    let key = derive_key(secret).ok_or(InviteTokenSecretMissing)?;
    let exp = now_ms.div_euclid(1000) + INVITE_TOKEN_TTL_SECONDS;
    Ok(format!("{VERSION}.{exp}.{}", sign(&key, user_uuid, exp)))
}

/// `signInviteToken(userUuid)` dengan secret & jam dari `state`.
pub fn sign_invite_token_now(
    state: &AppState,
    user_uuid: &str,
) -> Result<String, InviteTokenSecretMissing> {
    sign_invite_token(
        &state.config.jwt_secret,
        user_uuid,
        chrono::Utc::now().timestamp_millis(),
    )
}

/// Memecah `v1.<digits 1-12>.<43 base64url>`; `None` bila bentuknya tidak cocok.
fn parse(token: &str) -> Option<(i64, &str)> {
    let rest = token.strip_prefix("v1.")?;
    let (exp, sig) = rest.split_once('.')?;
    let exp_ok = (1..=12).contains(&exp.len()) && exp.bytes().all(|b| b.is_ascii_digit());
    let sig_ok = sig.len() == SIG_LEN
        && sig
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !exp_ok || !sig_ok {
        return None;
    }
    Some((exp.parse().ok()?, sig))
}

/// `isInviteTokenFormat`: bentuk token undangan baru (bukan token aktivasi pendaftar mandiri).
pub fn is_invite_token_format(token: &str) -> bool {
    parse(token).is_some()
}

/// `verifyInviteToken(token, userUuid, nowMs)`: sah untuk `user_uuid` dan belum kedaluwarsa.
/// Secret kosong = selalu `false`.
pub fn verify_invite_token(secret: &str, token: &str, user_uuid: &str, now_ms: i64) -> bool {
    let Some((exp, given)) = parse(token) else {
        return false;
    };
    let now_s = now_ms.div_euclid(1000);
    if exp <= now_s || exp > now_s + INVITE_TOKEN_TTL_SECONDS + FUTURE_SKEW_SECONDS {
        return false;
    }
    let Some(key) = derive_key(secret) else {
        return false;
    };
    constant_time_eq(sign(&key, user_uuid, exp).as_bytes(), given.as_bytes())
}

/// `verifyInviteToken` dengan secret & jam dari `state`.
pub fn verify_invite_token_now(state: &AppState, token: &str, user_uuid: &str) -> bool {
    verify_invite_token(
        &state.config.jwt_secret,
        token,
        user_uuid,
        chrono::Utc::now().timestamp_millis(),
    )
}

/// `buildInviteUrl(username, token)`: `${HOST_FE}/confirm?username=..&token=..&type=invite`.
pub fn build_invite_url(state: &AppState, username: &str, token: &str) -> String {
    format!(
        "{}/confirm?username={}&token={}&type=invite",
        env_or_undefined(state.config.var("HOST_FE")),
        encode_uri_component(username),
        encode_uri_component(token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "11111111-2222-3333-4444-555555555555";
    /// Hasil Node: secret `test-secret`, exp 1800000000.
    const NODE_SIG: &str = "TkLPjFmL06zpxuoW2QYMFBt9NcGDYm5xvkRprigZQWM";

    #[test]
    fn signature_matches_node() {
        let key = derive_key("test-secret").unwrap_or([0; 32]);
        assert_eq!(sign(&key, UUID, 1_800_000_000), NODE_SIG);
    }

    #[test]
    fn roundtrip_and_format() {
        let now = 1_700_000_000_000;
        let token = sign_invite_token("s3cret", UUID, now).unwrap_or_default();
        assert!(is_invite_token_format(&token));
        assert!(verify_invite_token("s3cret", &token, UUID, now));
        assert!(verify_invite_token(
            "s3cret",
            &token,
            UUID,
            now + 6 * 24 * 3600 * 1000
        ));
    }

    #[test]
    fn rejects_wrong_user_secret_and_expiry() {
        let now = 1_700_000_000_000;
        let token = sign_invite_token("s3cret", UUID, now).unwrap_or_default();
        assert!(!verify_invite_token("s3cret", &token, "other-uuid", now));
        assert!(!verify_invite_token("other", &token, UUID, now));
        assert!(!verify_invite_token("", &token, UUID, now));
        // kedaluwarsa tepat di exp
        let exp_ms = (now / 1000 + INVITE_TOKEN_TTL_SECONDS) * 1000;
        assert!(!verify_invite_token("s3cret", &token, UUID, exp_ms));
    }

    #[test]
    fn rejects_far_future_and_tampered() {
        // token dengan exp jauh di masa depan (tidak mungkin dibuat server ini)
        let future = sign_invite_token("k", UUID, 1_700_000_000_000 + 30 * 24 * 3600 * 1000)
            .unwrap_or_default();
        assert!(!verify_invite_token("k", &future, UUID, 1_700_000_000_000));
        let ok = sign_invite_token("k", UUID, 1_700_000_000_000).unwrap_or_default();
        let tampered = format!("{}x", &ok[..ok.len() - 1]);
        assert!(!verify_invite_token(
            "k",
            &tampered,
            UUID,
            1_700_000_000_000
        ));
        assert!(!is_invite_token_format("$2b$10$abcdef"));
        assert!(!is_invite_token_format("v1.123.short"));
    }

    #[test]
    fn missing_secret_is_error() {
        assert_eq!(
            sign_invite_token("", UUID, 0),
            Err(InviteTokenSecretMissing)
        );
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
