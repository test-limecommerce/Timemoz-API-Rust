//! Google OAuth: `google-oauth/google.strategy.ts` + `google-oauth.guard.ts`
//! (passport-google-oauth20 2.x di atas passport-oauth2 1.7),
//! `google-oauth-state.util.ts`, dan `AuthService.loginWithGoogle` /
//! `registerWithGoogle`.
//!
//! `state` OAuth kini ditandatangani + diikat ke cookie nonce (CSRF): lihat
//! `oauth_csrf.rs`. `parse_state` di bawah menerima payload YANG SUDAH
//! diverifikasi. Catatan lama: dulu `NoopStateStore` meneruskannya
//! apa adanya dan callback menormalisasi ulang isinya (`parseGoogleOAuthState`),
//! karena isinya bisa dipalsukan pemanggil. Perilaku itu dipertahankan persis
//! supaya state yang dibuat Node bisa diselesaikan Rust dan sebaliknya.

use chrono::{Duration, Utc};
use mz_account::password::hash_password;
use mz_account::random_password::generate_random_password;
use mz_account::signup_tracking::{
    SELF_SIGNUP, SignUpEventInput, SignUpPlan, SignupAttribution, build_sign_up_properties,
    compact_signup_attribution, normalize_signup_attribution,
};
use mz_account::username_conflict::is_username_conflict_api;
use mz_account::util::{encode_uri_component, env_or_undefined};
use mz_core::company::USER_COMPANY_SCOPE_SQL;
use mz_core::enums::PricingProductType;
use mz_core::time::now_naive;
use mz_core::user_status::{STATUS_ACTIVE, STATUS_DELETED, STATUS_DISABLED, STATUS_INVITED};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value};

use mz_audit::{AuditHandle, Model, Row};

use crate::analytics::{
    DiscordNewUser, emit_sign_up, resolve_platform, spawn_app_installed, spawn_discord_new_user,
};
use crate::dto::LOGIN_PLATFORMS;
use crate::service::invite::generate_unique_invite_code;
use crate::service::register::{
    NewCompany, NewSubscription, attach_owner, company_id_of, company_str, created_user_row,
    error_message, find_tier, free_subscription_end, idle_time_in_second, insert_company,
    insert_subscription, seed_company_defaults, spawn_admin_register_info, username_exists,
};
use crate::service::session::{activate_device_session, sign_user_token};

pub const AUTHORIZATION_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://www.googleapis.com/oauth2/v4/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";

/// Batas atribusi di dalam `state` (`google-oauth.guard.ts`).
const MAX_STATE_ATTRIBUTION_LENGTH: usize = 1500;
/// `google-oauth-state.util.ts`.
const MAX_FIELD_LENGTH: usize = 255;
const MAX_STATE_LENGTH: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Login,
    Register,
}

/// Query string sebagai pasangan; kunci berulang = array (qs Express).
pub fn query_object(pairs: &[(String, String)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (k, v) in pairs {
        match map.get_mut(k) {
            Some(Value::Array(items)) => items.push(Value::String(v.clone())),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, Value::String(v.clone())]);
            }
            None => {
                map.insert(k.clone(), Value::String(v.clone()));
            }
        }
    }
    map
}

/// `stateAttribution(query)`: atribusi kompak, dibuang bila > 1500 karakter JSON.
fn state_attribution(query: &Map<String, Value>) -> Option<Map<String, Value>> {
    let compact = compact_signup_attribution(&normalize_signup_attribution(
        &Value::Object(query.clone()),
        Utc::now(),
    ))?;
    let len = serde_json::to_string(&compact)
        .map(|s| s.encode_utf16().count())
        .unwrap_or(usize::MAX);
    (len <= MAX_STATE_ATTRIBUTION_LENGTH).then_some(compact)
}

/// `JSON.stringify({ action, macAddress, os, attribution })` dari `GoogleLoginGuard` /
/// `GoogleRegisterGuard` (`macAddress = ''`, `os = ''` bila tidak dikirim).
pub fn build_state(action: Action, query: &Map<String, Value>) -> String {
    let field = |k: &str| {
        query
            .get(k)
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()))
    };
    let action = match action {
        Action::Login => "login",
        Action::Register => "register",
    };
    let mut out = format!(
        "{{\"action\":{},\"macAddress\":{},\"os\":{}",
        Value::String(action.into()),
        field("macAddress"),
        field("os")
    );
    if let Some(attr) = state_attribution(query) {
        out.push_str(&format!(",\"attribution\":{}", Value::Object(attr)));
    }
    out.push('}');
    out
}

/// `${HOST_BE}/${GOOGLE_REDIRECT_URL}`.
pub fn callback_url(state: &AppState) -> String {
    format!(
        "{}/{}",
        env_or_undefined(state.config.var("HOST_BE")),
        env_or_undefined(state.config.var("GOOGLE_REDIRECT_URL"))
    )
}

/// Lokasi redirect passport-oauth2 (`url.format` atas authorizeUrl + params, lalu `client_id`).
pub fn authorize_location(state: &AppState, oauth_state: Option<&str>) -> String {
    let mut params: Vec<(&str, String)> = vec![
        ("response_type", "code".into()),
        ("redirect_uri", callback_url(state)),
        ("scope", "email profile".into()),
    ];
    if let Some(s) = oauth_state.filter(|s| !s.is_empty()) {
        params.push(("state", s.to_owned()));
    }
    params.push((
        "client_id",
        env_or_undefined(state.config.var("GOOGLE_OAUTH_CLIENT_ID")),
    ));
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_uri_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZATION_URL}?{query}")
}

/// Hasil `parseGoogleOAuthState`.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthState {
    pub action: Action,
    pub mac_address: Option<String>,
    pub os: Option<String>,
    pub platform: Option<String>,
    pub attribution: SignupAttribution,
}

fn read_string(source: &Map<String, Value>, key: &str) -> Option<String> {
    let s = source.get(key)?.as_str()?;
    // `.trim().slice(0, 255)` lalu kosong = tidak ada.
    let t: String = s.trim().chars().take(MAX_FIELD_LENGTH).collect();
    (!t.is_empty()).then_some(t)
}

/// `parseGoogleOAuthState(raw)` — nilai apa pun yang tidak berbentuk benar dibuang.
pub fn parse_state(raw: Option<&str>) -> OAuthState {
    let fallback = OAuthState {
        action: Action::Login,
        mac_address: None,
        os: None,
        platform: None,
        attribution: normalize_signup_attribution(&Value::Null, Utc::now()),
    };
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return fallback;
    };
    if raw.encode_utf16().count() > MAX_STATE_LENGTH {
        return fallback;
    }
    let Ok(Value::Object(source)) = serde_json::from_str::<Value>(raw) else {
        return fallback;
    };
    let action = if read_string(&source, "action").as_deref() == Some("register") {
        Action::Register
    } else {
        Action::Login
    };
    let platform = read_string(&source, "platform")
        .map(|p| p.to_lowercase())
        .filter(|p| LOGIN_PLATFORMS.contains(&p.as_str()));
    OAuthState {
        action,
        mac_address: read_string(&source, "macAddress"),
        os: read_string(&source, "os"),
        platform,
        attribution: normalize_signup_attribution(
            source.get("attribution").unwrap_or(&Value::Null),
            Utc::now(),
        ),
    }
}

/// `req.user` dari `GoogleStrategy.validate`.
#[derive(Debug, Clone)]
pub struct GoogleUser {
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

/// Hasil langkah passport di callback.
pub enum CallbackAuth {
    /// `this.fail` → AuthGuard melempar `UnauthorizedException()`.
    Unauthorized,
    /// `this.error(err)` → 500.
    Error(anyhow::Error),
    /// Tanpa `code`: passport mengalihkan lagi ke Google (tanpa `state`).
    Redirect(String),
    User(GoogleUser),
}

/// `OpenIDProfile.parse` + `validate`: `emails[0].value`, `name.givenName`, `photos[0].value`
/// (profil tanpa email / nama / foto melempar TypeError di Node → 500).
pub fn user_from_profile(json: &Value) -> Result<GoogleUser, anyhow::Error> {
    let s = |k: &str| {
        json.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let email = s("email")
        .ok_or_else(|| anyhow::anyhow!("Cannot read properties of undefined (reading '0')"))?;
    let (first_name, last_name) = (s("given_name"), s("family_name"));
    if first_name.is_none() && last_name.is_none() {
        return Err(anyhow::anyhow!(
            "Cannot read properties of undefined (reading 'givenName')"
        ));
    }
    if s("picture").is_none() {
        return Err(anyhow::anyhow!(
            "Cannot read properties of undefined (reading '0')"
        ));
    }
    Ok(GoogleUser {
        email,
        first_name,
        last_name,
    })
}

/// Langkah `GoogleCallbackGuard` (passport-oauth2 `authenticate`).
pub async fn authenticate_callback(state: &AppState, query: &Map<String, Value>) -> CallbackAuth {
    let q = |k: &str| query.get(k);
    if let Some(error) = q("error").filter(|e| crate::js::truthy(Some(e))) {
        return if error.as_str() == Some("access_denied") {
            CallbackAuth::Unauthorized
        } else {
            CallbackAuth::Error(anyhow::anyhow!(
                "AuthorizationError: {}",
                q("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ))
        };
    }
    let Some(code) = q("code").filter(|c| crate::js::truthy(Some(c))) else {
        // `NoopStateStore.store` tanpa state: lokasi tanpa parameter `state`.
        return CallbackAuth::Redirect(authorize_location(state, None));
    };
    let code = match code {
        Value::String(s) => s.clone(),
        other => crate::js::template(Some(other)),
    };
    match exchange_and_fetch(state, &code).await {
        Ok(user) => CallbackAuth::User(user),
        Err(err) => CallbackAuth::Error(err),
    }
}

async fn exchange_and_fetch(state: &AppState, code: &str) -> anyhow::Result<GoogleUser> {
    let form = [
        ("grant_type", "authorization_code".to_owned()),
        ("redirect_uri", callback_url(state)),
        (
            "client_id",
            env_or_undefined(state.config.var("GOOGLE_OAUTH_CLIENT_ID")),
        ),
        (
            "client_secret",
            env_or_undefined(state.config.var("GOOGLE_OAUTH_CLIENT_SECRET")),
        ),
        ("code", code.to_owned()),
    ];
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_uri_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    let res = state
        .http
        .post(TOKEN_URL)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to obtain access token: {e}"))?;
    if !res.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to obtain access token (status {})",
            res.status().as_u16()
        ));
    }
    let token: Value = res
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to obtain access token: {e}"))?;
    let access_token = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Failed to obtain access token"))?;
    let res = state
        .http
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch user profile: {e}"))?;
    if !res.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch user profile (status {})",
            res.status().as_u16()
        ));
    }
    let profile: Value = res
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Failed to parse user profile"))?;
    user_from_profile(&profile)
}

/// Hasil `loginWithGoogle` / `registerWithGoogle` yang dipakai callback.
pub struct OAuthResult {
    pub success: bool,
    pub should_register: bool,
    pub message: Option<String>,
    pub access_token: Option<String>,
}

impl OAuthResult {
    fn fail(message: &str) -> Self {
        Self {
            success: false,
            should_register: false,
            message: Some(message.into()),
            access_token: None,
        }
    }
}

/// `AuthService.loginWithGoogle` (bagian yang menentukan hasil callback).
pub async fn login_with_google(
    state: &AppState,
    audit: &AuditHandle,
    user: &GoogleUser,
    mac_address: Option<&str>,
    os: Option<&str>,
    platform: Option<&str>,
) -> ApiResult<OAuthResult> {
    let row: Option<(i32, i32)> =
        sqlx::query_as("SELECT id, status FROM users WHERE username = $1")
            .bind(user.email.to_lowercase())
            .fetch_optional(&state.db)
            .await?;
    let Some((user_id, status)) = row else {
        return Ok(OAuthResult {
            should_register: true,
            ..OAuthResult::fail("Account not found. Please register first.")
        });
    };
    if status == STATUS_DELETED || status == STATUS_DISABLED {
        return Ok(OAuthResult::fail("Your account has been deactivated."));
    }
    if status == STATUS_INVITED {
        return Ok(OAuthResult::fail("Please activate your account via email."));
    }
    let sid = match mac_address {
        Some(mac) => Some(activate_device_session(state, user_id, mac, os).await?),
        None => None,
    };
    let access_token = sign_user_token(state, user_id, sid.map(Value::String))?;

    let sql = format!(
        r#"SELECT uc."companyId" FROM "userCompany" uc WHERE {USER_COMPANY_SCOPE_SQL} LIMIT 1"#
    );
    let company_id: Option<i32> = sqlx::query_scalar(&sql)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(company_id) = company_id else {
        return Ok(OAuthResult::fail("User has no active company assigned."));
    };
    spawn_app_installed(
        state,
        audit,
        user_id,
        Some(company_id),
        "google",
        platform,
        mac_address,
        os,
    );
    Ok(OAuthResult {
        success: true,
        should_register: false,
        message: Some("Login with Google success".into()),
        access_token: Some(access_token),
    })
}

/// `AuthService.registerWithGoogle`. Galat di dalam blok `try` Node menjadi
/// `{ success:false, message:'register with Google failed' }`.
pub async fn register_with_google(
    state: &AppState,
    audit: &AuditHandle,
    user: &GoogleUser,
    mac_address: Option<&str>,
    os: Option<&str>,
    platform: Option<&str>,
    attribution: SignupAttribution,
) -> ApiResult<OAuthResult> {
    let username = user.email.to_lowercase();
    // Google tidak mengirim familyName untuk akun bernama tunggal.
    let joined = [user.first_name.as_deref(), user.last_name.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let name = match joined.trim() {
        "" => user.email.split('@').next().unwrap_or_default().to_owned(),
        t => t.to_owned(),
    };

    if username_exists(state, &username).await? {
        return Ok(OAuthResult::fail(
            "Account already exists. Please login with Google instead.",
        ));
    }
    let default_uuid = state.config.var("GOOGLE_DEFAULT_PRICING_UUID");
    let Some(tier) = find_tier(state, default_uuid.as_deref()).await? else {
        return Ok(OAuthResult::fail(
            "Default pricing tier not configured. Please contact support.",
        ));
    };

    let inner = async {
        let quantity = tier.initial_quantity();
        let billing_interval = tier.billing_interval().to_owned();
        let now = now_naive();
        let hashed = hash_password(state, &generate_random_password(24)).await?;
        let invite_code = generate_unique_invite_code(state).await?;

        let mut tx = state.db.begin().await?;
        let (user_id, user_uuid): (i32, String) = sqlx::query_as(
            r#"INSERT INTO users (name, username, "createdAt", status, password, "emailVerifiedAt",
                   "salesOnboardingEmailEligible")
               VALUES ($1, $2, $3, $4, $5, $3, $6) RETURNING id, uuid::text"#,
        )
        .bind(&name)
        .bind(&username)
        .bind(now)
        .bind(STATUS_ACTIVE)
        .bind(&hashed)
        .bind(tier.product_type == PricingProductType::SALES)
        .fetch_one(&mut *tx)
        .await?;
        // `users.create({ select: { id, uuid, username, name } })`.
        audit
            .record_create(
                Model::Users,
                &created_user_row(user_id, &user_uuid, &username, &name),
            )
            .await;
        let company = insert_company(
            &mut tx,
            &NewCompany {
                owner_user_id: user_id,
                address: "",
                telp: "",
                register_name: &name,
                register_email: &username,
                register_roles: "Owner",
                register_total_employees: "1-10",
                tier: &tier,
                is_trial: tier.base_price > 0.0,
                invite_code: &invite_code,
            },
        )
        .await?;
        let company_id = company_id_of(&company);
        // `company.create({ select: { id, uuid } })`: label kosong, `after` = `{ uuid }`.
        audit
            .record_create(
                Model::Company,
                &Row::new()
                    .with("id", company_id)
                    .with("uuid", company_str(&company, "uuid")),
            )
            .await;
        seed_company_defaults(
            &mut tx,
            audit,
            company_id,
            user_id,
            &format!("{name} Project"),
        )
        .await?;
        // Langganan `pricePerUser: 0` → masa aktif ikut `FREE_SUBSCRIPTION_DAYS` (14 hari bila kosong).
        let end = free_subscription_end(state, now, now + Duration::days(14));
        insert_subscription(
            &mut tx,
            &NewSubscription {
                company_id,
                tier_id: tier.id,
                start: now,
                end,
                status: "ACTIVE",
                quantity,
                price_per_user: 0.0,
                billing_interval: &billing_interval,
                paypal_plan_id: None,
                paypal_subscription_id: None,
                paypal_status: None,
            },
        )
        .await?;
        attach_owner(
            &mut tx,
            audit,
            user_id,
            company_id,
            idle_time_in_second(state)?,
        )
        .await?;
        let mut access_token = sign_user_token(state, user_id, None)?;
        tx.commit().await?;

        spawn_admin_register_info(state, &name, &username);
        let sign_up = build_sign_up_properties(&SignUpEventInput {
            signup_type: SELF_SIGNUP,
            signup_method: "google",
            signup_platform: resolve_platform(platform, mac_address, os),
            product_type: tier.product_type,
            role: "SUPER_ADMIN".into(),
            company_uuid: company_str(&company, "uuid"),
            is_workspace_owner: true,
            plan: Some(SignUpPlan {
                uuid: tier.uuid.clone(),
                name: tier.name.clone(),
                billing_interval: billing_interval.clone(),
                is_trial: company
                    .get("isTrial")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            invited_by_uuid: None,
            attribution: Some(attribution),
            occurred_at: now.and_utc(),
        });
        emit_sign_up(state, &user_uuid, sign_up.clone(), now.and_utc());
        spawn_discord_new_user(
            state,
            DiscordNewUser {
                name: name.clone(),
                email: username.clone(),
                company_name: company_str(&company, "name"),
                registration_date: mz_core::time::to_js_iso(&Utc::now()),
                sign_up,
            },
        );

        if let Some(mac) = mac_address {
            // Token di dalam transaksi terbit sebelum sesi device ada → diganti token ber-`sid`.
            let sid = activate_device_session(state, user_id, mac, os).await?;
            access_token = sign_user_token(state, user_id, Some(Value::String(sid)))?;
        }
        spawn_app_installed(
            state,
            audit,
            user_id,
            Some(company_id),
            "google",
            platform,
            mac_address,
            os,
        );
        Ok::<_, ApiError>(access_token)
    };

    Ok(match inner.await {
        Ok(token) => OAuthResult {
            success: true,
            should_register: false,
            message: Some("register with Google success".into()),
            access_token: Some(token),
        },
        Err(err) if is_username_conflict_api(&err) => {
            OAuthResult::fail("Account already exists. Please login with Google instead.")
        }
        Err(err) => {
            tracing::error!(error = %error_message(&err), "register with Google failed");
            OAuthResult::fail("register with Google failed")
        }
    })
}

/// `AuthController.googleCallback` setelah guard: lokasi redirect ke dashboard.
pub async fn callback_location(
    state: &AppState,
    audit: &AuditHandle,
    user: &GoogleUser,
    raw_state: Option<&str>,
) -> ApiResult<String> {
    let fe = env_or_undefined(state.config.var("HOST_FE"));
    let st = parse_state(raw_state);
    let (mac, os, platform) = (
        st.mac_address.as_deref(),
        st.os.as_deref(),
        st.platform.as_deref(),
    );
    let result = match st.action {
        Action::Register => {
            register_with_google(state, audit, user, mac, os, platform, st.attribution).await?
        }
        Action::Login => {
            let r = login_with_google(state, audit, user, mac, os, platform).await?;
            if !r.success && r.should_register && mac.is_none() && os.is_none() {
                register_with_google(state, audit, user, mac, os, platform, st.attribution).await?
            } else {
                r
            }
        }
    };
    Ok(match (result.success, result.access_token) {
        (true, Some(token)) => {
            format!(
                "{fe}/auth/google-callback?success=true&token={}",
                encode_uri_component(&token)
            )
        }
        _ => {
            let message = result
                .message
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "Authentication failed".into());
            format!(
                "{fe}/auth/google-callback?success=false&message={}",
                encode_uri_component(&message)
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn state_roundtrip() {
        let q = query_object(&pairs(&[
            ("macAddress", "AA:BB"),
            ("os", "Windows"),
            ("utm_source", "google"),
        ]));
        let raw = build_state(Action::Register, &q);
        assert!(raw.starts_with(
            r#"{"action":"register","macAddress":"AA:BB","os":"Windows","attribution":{"#
        ));
        let parsed = parse_state(Some(&raw));
        assert_eq!(parsed.action, Action::Register);
        assert_eq!(parsed.mac_address.as_deref(), Some("AA:BB"));
        assert_eq!(parsed.os.as_deref(), Some("Windows"));
        assert_eq!(parsed.attribution.utm_source.as_deref(), Some("google"));
    }

    #[test]
    fn state_defaults_to_empty_strings() {
        let raw = build_state(Action::Login, &Map::new());
        assert_eq!(raw, r#"{"action":"login","macAddress":"","os":""}"#);
        let parsed = parse_state(Some(&raw));
        assert_eq!(parsed.action, Action::Login);
        assert_eq!(parsed.mac_address, None);
        assert_eq!(parsed.os, None);
    }

    #[test]
    fn hostile_state_falls_back() {
        for raw in ["", "not json", "[1,2]", "42", "null"] {
            let p = parse_state(Some(raw));
            assert_eq!(p.action, Action::Login);
            assert_eq!(p.mac_address, None);
        }
        let huge = format!(r#"{{"action":"register","x":"{}"}}"#, "a".repeat(5000));
        assert_eq!(parse_state(Some(&huge)).action, Action::Login);
        let odd = parse_state(Some(
            r#"{"action":["register"],"macAddress":5,"platform":" IOS ","os":"  "}"#,
        ));
        assert_eq!(odd.action, Action::Login);
        assert_eq!(odd.mac_address, None);
        assert_eq!(odd.os, None);
        assert_eq!(odd.platform.as_deref(), Some("ios"));
        assert_eq!(parse_state(Some(r#"{"platform":"pc"}"#)).platform, None);
        let long = parse_state(Some(&format!(r#"{{"macAddress":"{}"}}"#, "m".repeat(300))));
        assert_eq!(long.mac_address.map(|m| m.len()), Some(255));
    }

    #[test]
    fn repeated_query_keys_become_arrays() {
        let q = query_object(&pairs(&[("macAddress", "a"), ("macAddress", "b")]));
        assert_eq!(q.get("macAddress"), Some(&json!(["a", "b"])));
        let parsed = parse_state(Some(&build_state(Action::Login, &q)));
        assert_eq!(parsed.mac_address, None);
    }

    #[test]
    fn profile_parsing_matches_passport() {
        let ok = user_from_profile(&json!({"email":"A@B.co","given_name":"Agus","picture":"p"}));
        assert!(
            ok.as_ref()
                .is_ok_and(|u| u.email == "A@B.co" && u.last_name.is_none())
        );
        assert!(user_from_profile(&json!({"given_name":"A","picture":"p"})).is_err());
        assert!(user_from_profile(&json!({"email":"a@b.co","picture":"p"})).is_err());
        assert!(user_from_profile(&json!({"email":"a@b.co","family_name":"B"})).is_err());
    }

    #[tokio::test]
    async fn authorize_location_shape() {
        let Ok(state) = AppState::for_test() else {
            return;
        };
        let loc = authorize_location(&state, Some(r#"{"action":"login"}"#));
        assert!(loc.starts_with(
            "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&redirect_uri="
        ));
        assert!(
            loc.contains(
                "&scope=email%20profile&state=%7B%22action%22%3A%22login%22%7D&client_id="
            )
        );
        assert!(!authorize_location(&state, None).contains("state="));
    }
}
