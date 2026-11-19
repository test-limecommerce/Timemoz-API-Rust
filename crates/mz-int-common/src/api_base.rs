//! URL dasar API eksternal. Produksi: konstanta bawaan (sama dengan Node). Uji integrasi
//! mengalihkannya ke [`crate::mock::MockServer`] lewat env (`TestApp::with_vars`).
//! Env override BUKAN fitur Node; tanpa env perilaku identik.

use mz_core::AppState;

/// `(env override, bawaan Node)`.
#[derive(Clone, Copy, Debug)]
pub struct ApiBase {
    pub env: &'static str,
    pub default: &'static str,
}

impl ApiBase {
    /// URL dasar tanpa `/` di akhir.
    pub fn get(self, state: &AppState) -> String {
        state
            .config
            .var(self.env)
            .map(|v| v.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| self.default.to_owned())
    }
}

/// `https://api.clickup.com/api/v2`.
pub const CLICKUP: ApiBase = ApiBase {
    env: "MZ_CLICKUP_API_BASE",
    default: "https://api.clickup.com/api/v2",
};
/// `https://api.atlassian.com` (REST Jira: `/ex/jira/{cloudId}/rest/api/3/...`,
/// resources: `/oauth/token/accessible-resources`).
pub const ATLASSIAN_API: ApiBase = ApiBase {
    env: "MZ_ATLASSIAN_API_BASE",
    default: "https://api.atlassian.com",
};
/// `https://auth.atlassian.com` — HANYA untuk `/oauth/token` (server-ke-server). URL
/// `/authorize` yang dikirim ke browser tetap konstanta Node, jangan lewat override ini.
pub const ATLASSIAN_AUTH: ApiBase = ApiBase {
    env: "MZ_ATLASSIAN_AUTH_BASE",
    default: "https://auth.atlassian.com",
};
/// `https://discord.com/api/v10` (URL `discord.com/oauth2/authorize` untuk browser tetap
/// konstanta).
pub const DISCORD: ApiBase = ApiBase {
    env: "MZ_DISCORD_API_BASE",
    default: "https://discord.com/api/v10",
};
/// `https://oauth2.googleapis.com` (`/token`, `/revoke`). URL consent
/// `accounts.google.com/o/oauth2/v2/auth` untuk browser tetap konstanta.
pub const GOOGLE_OAUTH: ApiBase = ApiBase {
    env: "MZ_GOOGLE_OAUTH_BASE",
    default: "https://oauth2.googleapis.com",
};
/// `https://www.googleapis.com` (`/calendar/v3/...`, `/oauth2/v2/userinfo`).
pub const GOOGLE_APIS: ApiBase = ApiBase {
    env: "MZ_GOOGLE_APIS_BASE",
    default: "https://www.googleapis.com",
};
