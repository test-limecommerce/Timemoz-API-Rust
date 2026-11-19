//! Bagian `analytics/analytics.service.ts`, `MixpanelService` dan notifikasi
//! Discord yang dipanggil alur auth. Semuanya fire-and-forget (`mz_core::bg::spawn`)
//! dan tidak pernah menggagalkan alur bisnis.

use chrono::{DateTime, Utc};
use mz_core::AppState;
use mz_core::company::USER_COMPANY_SCOPE_SQL;
use mz_core::mixpanel::{MixpanelEvent, TrackInput, track};
use mz_core::time::{now_naive, parse_js_date};
use rand::Rng;
use serde_json::{Map, Value, json};

/// `PLATFORM_WEB` / `KNOWN_PLATFORMS`.
pub const PLATFORM_WEB: &str = "web";
const KNOWN_PLATFORMS: [&str; 4] = ["desktop", "android", "ios", PLATFORM_WEB];
pub const APP_INSTALLED: &str = "app_installed";
pub const FIRST_TRACKED_HOUR: &str = "first_tracked_hour";
pub const DAY_7_STILL_TRACKING: &str = "day_7_still_tracking";
pub const DAY_30_RETAINED: &str = "day_30_retained";
/// `EVENTS_REQUIRING_NEW_USER` = semua event GA kecuali `upgrade_to_pro`.
const EVENT_NOT_REQUIRING_NEW_USER: &str = "upgrade_to_pro";
const DEFAULT_NEW_USER_SINCE: &str = "2026-08-01T00:00:00Z";
pub(crate) const GA_PARAM_MAX_LENGTH: usize = 100;
const GA_MAX_CUSTOM_PARAMS: usize = 23;

/// `resolvePlatform({ platform, macAddress, os })`.
pub fn resolve_platform(
    platform: Option<&str>,
    mac_address: Option<&str>,
    os: Option<&str>,
) -> String {
    let declared = platform
        .map(|p| p.trim().to_lowercase())
        .unwrap_or_default();
    if KNOWN_PLATFORMS.contains(&declared.as_str()) {
        return declared;
    }
    let has = |v: Option<&str>| v.is_some_and(|s| !s.trim().is_empty());
    if has(mac_address) && has(os) {
        return "desktop".into();
    }
    PLATFORM_WEB.into()
}

/// `isAppPlatform`.
pub fn is_app_platform(platform: &str) -> bool {
    platform != PLATFORM_WEB
}

pub(crate) fn new_user_cutoff(state: &AppState) -> chrono::NaiveDateTime {
    state
        .config
        .var("GA_NEW_USER_SINCE")
        .and_then(|raw| parse_js_date(&raw))
        .or_else(|| parse_js_date(DEFAULT_NEW_USER_SINCE))
        .unwrap_or_default()
}

/// `sanitizeParams`: key berformat GA, string dipotong 100, maks 23 param.
pub(crate) fn sanitize_params(raw: Vec<(&str, Value)>) -> Map<String, Value> {
    let key_ok = |k: &str| {
        let mut chars = k.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && k.len() <= 40
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let mut out = Map::new();
    for (k, v) in raw {
        if out.len() >= GA_MAX_CUSTOM_PARAMS {
            break;
        }
        if v.is_null() || !key_ok(k) {
            continue;
        }
        match v {
            Value::String(s) => {
                let t: String = s.trim().chars().take(GA_PARAM_MAX_LENGTH).collect();
                if !t.is_empty() {
                    out.insert(k.to_owned(), Value::String(t));
                }
            }
            Value::Number(_) | Value::Bool(_) => {
                out.insert(k.to_owned(), v);
            }
            _ => {}
        }
    }
    out
}

/// `generateClientId`: `{10 digit acak}.{unix seconds}` (CSPRNG).
pub(crate) fn generate_client_id() -> String {
    let random: u64 = rand::rng().random_range(1_000_000_000..10_000_000_000);
    format!("{random}.{}", Utc::now().timestamp())
}

/// `AnalyticsService.claimAndEmit` tanpa enqueue: klaim baris `PENDING` lalu
/// `ensureClientId`. Mengembalikan id baris bila klaim menang (`None` = dilewati
/// atau sudah diklaim). Pengiriman dilakukan pemanggil (`ga::send_event`).
///
/// DEVIASI: Node mengantrekan job Bull `ga.send` sesudah klaim. Rust hanya klaim;
/// pengiriman langsung hanya bila `MZ_JOBS` mengizinkan, selain itu sweeper.
pub async fn claim_event(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    event_name: &str,
    params: Vec<(&str, Value)>,
    occurred_at: chrono::NaiveDateTime,
) -> anyhow::Result<Option<i32>> {
    if user_id <= 0 {
        return Ok(None);
    }
    let user: Option<(chrono::NaiveDateTime, Option<String>)> =
        sqlx::query_as(r#"SELECT "createdAt", "gaClientId" FROM users WHERE id = $1"#)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((created_at, ga_client_id)) = user else {
        return Ok(None);
    };
    if event_name != EVENT_NOT_REQUIRING_NEW_USER && created_at < new_user_cutoff(state) {
        return Ok(None);
    }
    let env = state
        .config
        .var("GA_ENV")
        .unwrap_or_else(|| "development".into())
        .trim()
        .to_lowercase();
    let mut all = vec![("env", Value::String(env))];
    all.extend(params);
    let params = sanitize_params(all);

    let claimed: Option<i32> = sqlx::query_scalar(
        r#"INSERT INTO "analyticsEvent" ("userId", "eventName", status, "occurredAt", params)
           VALUES ($1, $2, 'PENDING', $3, $4)
           ON CONFLICT ("userId", "eventName") DO NOTHING
           RETURNING id"#,
    )
    .bind(user_id)
    .bind(event_name)
    .bind(occurred_at)
    .bind(Value::Object(params))
    .fetch_optional(&state.db)
    .await?;
    let Some(claimed_id) = claimed else {
        return Ok(None);
    };
    if ga_client_id.is_none() {
        // Audit `users.updateMany`: `gaClientId` di luar allowlist → normalnya tanpa baris.
        let pre = audit
            .pre_read_many(
                mz_audit::Model::Users,
                mz_audit::Lookup::filter(move |qb| {
                    qb.push("id = ").push_bind(user_id);
                    qb.push(r#" AND "gaClientId" IS NULL"#);
                }),
            )
            .await;
        let client_id = generate_client_id();
        let done = sqlx::query(
            r#"UPDATE users SET "gaClientId" = $2 WHERE id = $1 AND "gaClientId" IS NULL"#,
        )
        .bind(user_id)
        .bind(&client_id)
        .execute(&state.db)
        .await?;
        audit
            .record_update_many(
                mz_audit::Model::Users,
                pre,
                done.rows_affected(),
                &mz_audit::Row::new().with("gaClientId", client_id),
                serde_json::json!({ "id": user_id, "gaClientId": null }),
            )
            .await;
    }
    tracing::info!(user_id, event = event_name, "Analytics event diklaim");
    Ok(Some(claimed_id))
}

/// `claimAndEmit` untuk `app_installed` (jalur login).
async fn claim_app_installed(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    params: Vec<(&str, Value)>,
) -> anyhow::Result<Option<i32>> {
    claim_event(state, audit, user_id, APP_INSTALLED, params, now_naive()).await
}

/// `emitAppInstalled(userId, companyId, loginMethod, source)` — fire-and-forget.
#[allow(clippy::too_many_arguments)] // + handle audit (ikut ke task, seperti ALS Node)
pub fn spawn_app_installed(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    company_id: Option<i32>,
    login_method: &'static str,
    platform: Option<&str>,
    mac_address: Option<&str>,
    os: Option<&str>,
) {
    let platform = resolve_platform(platform, mac_address, os);
    if !is_app_platform(&platform) {
        return;
    }
    let state = state.clone();
    // Handle request ikut ke task (Node: ALS ikut ke promise fire-and-forget); entri yang
    // datang sesudah respons ditulis dibuang, sama seperti buffer Node yang sudah di-flush.
    let audit = audit.clone();
    mz_core::bg::spawn(async move {
        let company_id = match company_id {
            Some(c) => Some(c),
            None => mz_core::company::find_company_id(&state.db, user_id)
                .await
                .ok()
                .flatten(),
        };
        let params = vec![
            ("platform", Value::String(platform)),
            ("company_id", company_id.map_or(Value::Null, Value::from)),
            ("login_method", Value::String(login_method.into())),
        ];
        match claim_app_installed(&state, &audit, user_id, params).await {
            Ok(Some(id)) if crate::ga::direct_send_enabled(&state) => {
                // Pengiriman langsung (`queue.add` Node → worker di proses ini).
                let state = state.clone();
                mz_core::bg::spawn(async move { crate::ga::send_event(&state, id).await });
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(user_id, error = %err, "Gagal klaim event \"app_installed\"");
            }
        }
    });
}

/// `emitSignUp(userUuid, properties, occurredAt)`.
pub fn emit_sign_up(
    state: &AppState,
    user_uuid: &str,
    properties: Map<String, Value>,
    occurred_at: DateTime<Utc>,
) {
    track(
        state,
        TrackInput {
            event: MixpanelEvent::SignUp,
            distinct_id: user_uuid.to_owned(),
            dedup_key: user_uuid.to_owned(),
            properties,
            occurred_at_ms: Some(occurred_at.timestamp_millis()),
        },
    );
}

/// `emitEmailVerified(userId, userUuid)`.
pub fn spawn_email_verified(state: &AppState, user_id: i32, user_uuid: String) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        let sql = format!(
            r#"SELECT c.uuid::text FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
               WHERE {USER_COMPANY_SCOPE_SQL} LIMIT 1"#
        );
        let company_uuid: Option<String> = match sqlx::query_scalar(&sql)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, "Emisi Mixpanel Email Verified dilewati");
                return;
            }
        };
        let mut properties = Map::new();
        properties.insert("verification_method".into(), json!("email_link"));
        if let Some(uuid) = company_uuid {
            properties.insert("company_id".into(), Value::String(uuid));
        }
        track(
            &state,
            TrackInput {
                event: MixpanelEvent::EmailVerified,
                distinct_id: user_uuid.clone(),
                dedup_key: user_uuid,
                properties,
                occurred_at_ms: None,
            },
        );
    });
}

pub struct OrganizationCreated {
    pub uuid: String,
    pub name: String,
    pub website_url: Option<String>,
    pub register_total_employees: Option<String>,
    pub modules: Vec<&'static str>,
}

/// `emitOrganizationCreated(userId, company)`.
pub fn spawn_organization_created(state: &AppState, user_id: i32, company: OrganizationCreated) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        let uuid: Option<String> =
            match sqlx::query_scalar("SELECT uuid::text FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&state.db)
                .await
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(error = %err, "Emisi Mixpanel Organization Created dilewati");
                    return;
                }
            };
        let Some(user_uuid) = uuid.filter(|u| !u.is_empty()) else {
            return;
        };
        let mut properties = Map::new();
        properties.insert("company_id".into(), Value::String(company.uuid.clone()));
        properties.insert("company_name".into(), Value::String(company.name));
        properties.insert(
            "total_employees".into(),
            company
                .register_total_employees
                .map_or(Value::Null, Value::String),
        );
        properties.insert(
            "has_website".into(),
            Value::Bool(company.website_url.is_some_and(|w| !w.is_empty())),
        );
        properties.insert("modules".into(), json!(company.modules));
        track(
            &state,
            TrackInput {
                event: MixpanelEvent::OrganizationCreated,
                distinct_id: user_uuid,
                dedup_key: company.uuid,
                properties,
                occurred_at_ms: None,
            },
        );
    });
}

// ── upgrade_to_pro ───────────────────────────────────────────────────────────

/// Nama event GA konversi berbayar.
pub const UPGRADE_TO_PRO: &str = EVENT_NOT_REQUIRING_NEW_USER;

/// `UpgradeToProInput`.
///
/// Pemanggil di Rust: webhook Midtrans (mz-settings) lewat [`spawn_upgrade_to_pro`]. PayPal
/// (`paypal.controller.ts`, 3 tempat) belum di-port dan kelak wajib memakai fungsi yang sama
/// (Node: `void emitUpgradeToPro(...)`).
#[derive(Debug, Clone)]
pub struct UpgradeToPro {
    pub company_id: i32,
    /// Dibaca SEBELUM update: true hanya untuk transisi gratis/trial → berbayar.
    /// Renewal harus `false`.
    pub is_first_paid_transition: bool,
    pub tier_name: Option<String>,
    pub billing_interval: Option<String>,
    /// `'paypal' | 'midtrans'`.
    pub payment_provider: &'static str,
    pub value: Option<f64>,
    pub currency: Option<String>,
}

/// Angka gaya JS untuk params jsonb: `10` bukan `10.0`; non-finite → `null` (dibuang
/// `sanitize_params`, seperti `Number.isFinite` di Node).
fn js_number(v: f64) -> Value {
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        return Value::from(v as i64);
    }
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

/// `resolveCompanyOwnerUserId(companyId)`:
/// 0) `company.ownerUserId` bila terisi;
/// 1) SUPER_ADMIN bawaan (`isSystem`, `roleCodeFilter(['SUPER_ADMIN'])`) aktif di company,
///    `userCompany.createdAt` paling awal (seri: `id`);
/// 2) `userCompany` aktif terlama. Kosong → `None` (jangan menebak).
pub async fn resolve_company_owner_user_id(
    db: &sqlx::PgPool,
    company_id: i32,
) -> sqlx::Result<Option<i32>> {
    let owner: Option<Option<i32>> =
        sqlx::query_scalar(r#"SELECT "ownerUserId" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    if let Some(Some(owner)) = owner
        && owner != 0
    {
        return Ok(Some(owner));
    }
    let super_admin: Option<i32> = sqlx::query_scalar(
        r#"SELECT uc."userId" FROM "userCompany" uc
            WHERE uc."companyId" = $1 AND uc.status = 1
              AND EXISTS (
                SELECT 1 FROM "userAccessLevel" ual
                  JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                 WHERE ual."userId" = uc."userId" AND ual."companyId" = $1 AND ual.status = 1
                   AND al."isSystem" AND al.code = 'SUPER_ADMIN')
            ORDER BY uc."createdAt" ASC, uc.id ASC
            LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    if super_admin.is_some() {
        return Ok(super_admin);
    }
    sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1
            ORDER BY "createdAt" ASC, id ASC LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await
}

/// `emitUpgradeToPro(input)`: event company-level, diatribusikan ke owner/SUPER_ADMIN
/// (deterministik, supaya retry webhook mendarat di user yang sama). Tidak pernah gagal:
/// galat hanya dicatat. Mengembalikan id baris `analyticsEvent` bila klaim menang.
/// Pengiriman GA: langsung bila `MZ_JOBS` mengizinkan (lihat [`crate::ga::direct_send_enabled`]),
/// selain itu sweeper — sama dengan `app_installed` (deviasi mz-auth #1).
pub async fn emit_upgrade_to_pro(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    input: UpgradeToPro,
) -> Option<i32> {
    // Guard anti-renewal: company yang sudah lama Pro tidak boleh tercatat konversi baru.
    if !input.is_first_paid_transition || input.company_id <= 0 {
        return None;
    }
    let company_id = input.company_id;
    let user_id = match resolve_company_owner_user_id(&state.db, company_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::error!(
                company_id,
                "upgrade_to_pro dilewati: tidak ada user aktif untuk company {company_id}"
            );
            return None;
        }
        Err(err) => {
            tracing::error!(error = %err, "Gagal emit upgrade_to_pro");
            return None;
        }
    };
    let params = vec![
        ("company_id", Value::from(company_id)),
        (
            "tier_name",
            input.tier_name.map_or(Value::Null, Value::String),
        ),
        (
            "billing_interval",
            input.billing_interval.map_or(Value::Null, Value::String),
        ),
        (
            "payment_provider",
            Value::String(input.payment_provider.into()),
        ),
        ("value", input.value.map_or(Value::Null, js_number)),
        (
            "currency",
            input.currency.map_or(Value::Null, Value::String),
        ),
    ];
    match claim_event(state, audit, user_id, UPGRADE_TO_PRO, params, now_naive()).await {
        Ok(Some(id)) => {
            if crate::ga::direct_send_enabled(state) {
                let state = state.clone();
                mz_core::bg::spawn(async move { crate::ga::send_event(&state, id).await });
            }
            Some(id)
        }
        Ok(None) => None,
        Err(err) => {
            tracing::error!(user_id, error = %err, "Gagal klaim event \"upgrade_to_pro\"");
            None
        }
    }
}

/// `void analytics.emitUpgradeToPro(...)` — fire-and-forget, dilacak `mz_core::bg`.
pub fn spawn_upgrade_to_pro(state: &AppState, audit: &mz_audit::AuditHandle, input: UpgradeToPro) {
    let state = state.clone();
    let audit = audit.clone();
    mz_core::bg::spawn(async move {
        emit_upgrade_to_pro(&state, &audit, input).await;
    });
}

// ── Discord ──────────────────────────────────────────────────────────────────

fn text_of(v: Option<&Value>) -> Option<String> {
    let t = v?.as_str()?.trim();
    // Batas field Discord 1024 karakter (Node memotong 1000 unit UTF-16).
    (!t.is_empty()).then(|| t.chars().take(1000).collect())
}

/// `discordSignUpFields(signUp)`.
pub fn discord_sign_up_fields(sign_up: &Map<String, Value>) -> Vec<Value> {
    let plan = [
        text_of(sign_up.get("plan_name")),
        match sign_up.get("is_trial") {
            Some(Value::Bool(true)) => Some("Trial".to_owned()),
            Some(Value::Bool(false)) => Some("Paid".to_owned()),
            _ => None,
        },
        text_of(sign_up.get("billing_interval")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let channel = text_of(sign_up.get("acquisition_channel"));
    let ad_platform = text_of(sign_up.get("ad_platform"));
    let utm = ["utm_source", "utm_medium", "utm_campaign"]
        .iter()
        .filter_map(|k| text_of(sign_up.get(*k)))
        .collect::<Vec<_>>()
        .join(" / ");
    let method_label = match sign_up.get("signup_method").and_then(Value::as_str) {
        Some("email") => Some("Email".to_owned()),
        Some("google") => Some("Google".to_owned()),
        _ => None,
    };
    let method = [method_label, text_of(sign_up.get("signup_platform"))]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let non_empty = |s: String| (!s.is_empty()).then_some(s);
    let fields: [(&str, Option<String>, bool); 7] = [
        ("Product", text_of(sign_up.get("product")), true),
        ("Plan", non_empty(plan), true),
        ("Sign-up Method", non_empty(method), true),
        (
            "Channel",
            channel.map(|c| match ad_platform {
                Some(p) => format!("{c} ({p})"),
                None => c,
            }),
            true,
        ),
        ("UTM (source / medium / campaign)", non_empty(utm), false),
        ("Referrer", text_of(sign_up.get("referrer_domain")), true),
        ("Landing Page", text_of(sign_up.get("landing_page")), true),
    ];
    fields
        .into_iter()
        .filter_map(|(name, value, inline)| {
            value.map(|v| json!({ "name": name, "value": v, "inline": inline }))
        })
        .collect()
}

pub struct DiscordNewUser {
    pub name: String,
    pub email: String,
    pub company_name: String,
    pub registration_date: String,
    pub sign_up: Map<String, Value>,
}

/// Body webhook `sendDiscordNewUserNotification`.
pub fn discord_payload(data: &DiscordNewUser) -> Value {
    let is_paid_ads = matches!(data.sign_up.get("is_paid_ads"), Some(Value::Bool(true)));
    let mut fields = vec![
        json!({ "name": "Name", "value": data.name, "inline": true }),
        json!({ "name": "Email", "value": data.email, "inline": true }),
        json!({ "name": "Company", "value": data.company_name, "inline": true }),
    ];
    fields.extend(discord_sign_up_fields(&data.sign_up));
    fields
        .push(json!({ "name": "Registered At", "value": data.registration_date, "inline": false }));
    json!({
        // Nilai atribusi datang dari browser: `@everyone` di utm tidak boleh memicu mention.
        "allowed_mentions": { "parse": [] },
        "embeds": [{
            "title": if is_paid_ads { "\u{1F4B0} New User Registered (Paid Ads)" } else { "\u{1F389} New User Registered" },
            "color": if is_paid_ads { 0x00f1_c40f } else { 0x0058_65f2 },
            "fields": fields,
            "timestamp": mz_core::time::to_js_iso(&Utc::now()),
        }],
    })
}

/// `sendDiscordNewUserNotification` — fire-and-forget; tanpa `DISCORD_WEBHOOK_URL` = no-op.
pub fn spawn_discord_new_user(state: &AppState, data: DiscordNewUser) {
    let Some(url) = state.config.var("DISCORD_WEBHOOK_URL") else {
        return;
    };
    let http = state.http.clone();
    mz_core::bg::spawn(async move {
        let res = http.post(url).json(&discord_payload(&data)).send().await;
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::error!(
                status = r.status().as_u16(),
                "Failed to send Discord notification"
            ),
            Err(err) => tracing::error!(error = %err, "Failed to send Discord notification"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_resolution() {
        assert_eq!(resolve_platform(Some(" Android "), None, None), "android");
        assert_eq!(
            resolve_platform(Some("pc"), Some("aa"), Some("Windows")),
            "desktop"
        );
        assert_eq!(resolve_platform(None, Some("aa"), Some(" ")), "web");
        assert!(!is_app_platform("web"));
        assert!(is_app_platform("ios"));
    }

    #[test]
    fn params_are_sanitized() {
        let out = sanitize_params(vec![
            ("env", json!("production")),
            ("platform", json!("  desktop ")),
            ("company_id", Value::Null),
            ("Bad-Key", json!("x")),
            ("login_method", json!("password")),
        ]);
        assert_eq!(
            out,
            json!({"env":"production","platform":"desktop","login_method":"password"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
    }

    #[test]
    fn client_id_shape() {
        let id = generate_client_id();
        let (a, b) = id.split_once('.').unwrap_or_default();
        assert_eq!(a.len(), 10);
        assert!(b.parse::<i64>().is_ok());
    }

    #[test]
    fn discord_fields_skip_empty() {
        let sign_up = json!({
            "product": "Time Tracking", "plan_name": "Free", "is_trial": true, "billing_interval": "MONTH",
            "signup_method": "google", "signup_platform": "web", "acquisition_channel": "Paid Search",
            "ad_platform": "Google Ads", "utm_source": "google", "utm_campaign": "promo"
        });
        let fields = discord_sign_up_fields(sign_up.as_object().unwrap_or(&Map::new()));
        let names: Vec<&str> = fields
            .iter()
            .filter_map(|f| f.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            vec![
                "Product",
                "Plan",
                "Sign-up Method",
                "Channel",
                "UTM (source / medium / campaign)"
            ]
        );
        assert_eq!(
            fields.get(1).and_then(|f| f.get("value").cloned()),
            Some(json!("Free · Trial · MONTH"))
        );
        assert_eq!(
            fields.get(3).and_then(|f| f.get("value").cloned()),
            Some(json!("Paid Search (Google Ads)"))
        );
        assert_eq!(
            fields.get(4).and_then(|f| f.get("value").cloned()),
            Some(json!("google / promo"))
        );
    }
}
