//! Port `src/auth/auth.service.ts`, dipecah per alur.

pub mod invite;
pub mod login;
pub mod oauth;
pub mod oauth_csrf;
pub mod organization;
pub mod password;
pub mod register;
pub mod session;

use mz_account::util::{encode_uri_component, env_or_undefined};
use mz_core::AppState;

/// `${HOST_FE_COMPRO}/verify-email?username=${encodeURIComponent(u)}&token=${hashedUUID}`
/// (token bcrypt TIDAK di-encode, sama dengan Node).
pub fn confirm_url(state: &AppState, username: &str, hashed_uuid: &str) -> String {
    format!(
        "{}/verify-email?username={}&token={hashed_uuid}",
        env_or_undefined(state.config.var("HOST_FE_COMPRO")),
        encode_uri_component(username)
    )
}
