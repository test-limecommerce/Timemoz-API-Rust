//! Pengiriman event ke GA4 Measurement Protocol. Port `GaClientService`
//! (`src/analytics/ga-client.service.ts`) dan `AnalyticsConsumer.handleSend`
//! (`src/analytics/analytics.consumer.ts`).
//!
//! Dipakai oleh cron (`analytics_jobs.rs`) dan, bila `MZ_JOBS` mengizinkan,
//! langsung dari jalur login (`analytics::spawn_app_installed`).
//! Kontrak: tidak pernah melempar; semua galat dicatat lalu baris diperbarui.
//! Status `PENDING -> SENDING` atomik sehingga dua pengirim (Node & Rust) tidak
//! pernah mengirim baris yang sama dua kali.

use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use futures::StreamExt;
use mz_core::AppState;
use mz_core::time::now_naive;
use serde_json::{Map, Value, json};

use crate::analytics::GA_PARAM_MAX_LENGTH;

pub const PROD_ENDPOINT: &str = "https://www.google-analytics.com/mp/collect";
pub const DEBUG_ENDPOINT: &str = "https://www.google-analytics.com/debug/mp/collect";
/// `GA_HTTP_TIMEOUT_MS`.
pub const HTTP_TIMEOUT: Duration = Duration::from_millis(5000);
/// `MAX_SEND_ATTEMPTS`: setelah sekian percobaan gagal baris menjadi FAILED.
pub const MAX_SEND_ATTEMPTS: i32 = 5;
/// `SEND_CONCURRENCY` = `concurrency: 5` pada `@Process(GA_SEND_JOB)`.
pub const SEND_CONCURRENCY: usize = 5;
/// `GA_USER_PROPERTY_VALUE_MAX_LENGTH`.
const GA_USER_PROPERTY_VALUE_MAX_LENGTH: usize = 36;
/// `GA_MAX_EVENT_AGE_MS` (71 jam: margin 1 jam di bawah batas 72 jam GA).
const GA_MAX_EVENT_AGE_MS: i64 = 71 * 60 * 60 * 1000;
/// `GA_DEBUG_USER_UUIDS` (kosong, sama dengan Node).
pub const GA_DEBUG_USER_UUIDS: &[&str] = &[];

pub const STATUS_SENT: &str = "SENT";
pub const STATUS_FAILED: &str = "FAILED";
pub const STATUS_SKIPPED: &str = "SKIPPED";
pub const STATUS_PENDING: &str = "PENDING";

/// `GA_MEASUREMENT_ID`, `GA_API_SECRET`, `GA_ENV` (sama dengan Node).
#[derive(Debug, Clone)]
pub struct GaConfig {
    measurement_id: String,
    api_secret: String,
    env: String,
}

impl GaConfig {
    pub fn from_state(state: &AppState) -> Self {
        let trimmed = |key: &str| {
            state
                .config
                .var(key)
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let env = state
            .config
            .var("GA_ENV")
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "development".into());
        Self {
            measurement_id: trimmed("GA_MEASUREMENT_ID"),
            api_secret: trimmed("GA_API_SECRET"),
            env,
        }
    }

    /// Kill switch: tanpa kredensial, tidak ada HTTP yang ditembak.
    pub fn enabled(&self) -> bool {
        !self.measurement_id.is_empty() && !self.api_secret.is_empty()
    }

    /// `development` memakai endpoint `/debug` yang hanya memvalidasi payload.
    pub fn validate_only(&self) -> bool {
        self.env == "development"
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    fn endpoint(&self) -> &'static str {
        if self.validate_only() {
            DEBUG_ENDPOINT
        } else {
            PROD_ENDPOINT
        }
    }

    /// `measurementIdSuffix`: 6 karakter terakhir.
    fn measurement_suffix(&self) -> String {
        let n = self.measurement_id.chars().count();
        self.measurement_id
            .chars()
            .skip(n.saturating_sub(6))
            .collect()
    }

    /// `redact`: jangan biarkan api_secret ikut ke log/DB lewat pesan galat.
    fn redact(&self, message: &str) -> String {
        if message.is_empty() {
            return "unknown error".into();
        }
        if self.api_secret.is_empty() {
            return message.to_owned();
        }
        message.replace(&self.api_secret, "***")
    }
}

/// Input satu event (setara `GaSendInput` yang sudah diisi dari baris DB).
pub struct GaEvent<'a> {
    pub client_id: &'a str,
    pub user_id: &'a str,
    pub event_name: &'a str,
    pub params: Map<String, Value>,
    /// Kosong = tidak ada `user_properties` (Node: `undefined`).
    pub user_properties: Map<String, Value>,
    /// `occurredAt` dalam ms epoch; `None` = tanggal tak valid (dipakai "sekarang").
    pub occurred_at_ms: Option<i64>,
    pub debug: bool,
}

/// `resolveTimestampMicros`: di-clamp ke [now - 71 jam, now], bukan dibuang.
pub fn resolve_timestamp_micros(occurred_at_ms: Option<i64>, now_ms: i64) -> i64 {
    match occurred_at_ms {
        None => now_ms.saturating_mul(1000),
        Some(raw) => {
            let oldest = now_ms - GA_MAX_EVENT_AGE_MS;
            raw.clamp(oldest, now_ms).saturating_mul(1000)
        }
    }
}

/// `buildUserProperties`: value string dipotong 36 karakter, kunci kosong = tanpa properti.
fn user_properties_json(raw: &Map<String, Value>) -> Map<String, Value> {
    raw.iter()
        .map(|(k, v)| {
            let value = match v {
                Value::String(s) => Value::String(truncate(s, GA_USER_PROPERTY_VALUE_MAX_LENGTH)),
                other => other.clone(),
            };
            (k.clone(), json!({ "value": value }))
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `toUserProperties`: hanya `company_id` yang valid (string tak kosong atau angka).
pub fn user_properties_from_params(params: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    match params.get("company_id") {
        Some(v @ Value::String(s)) if !s.is_empty() => {
            out.insert("company_id".into(), v.clone());
        }
        Some(v @ Value::Number(_)) => {
            out.insert("company_id".into(), v.clone());
        }
        _ => {}
    }
    out
}

/// `buildBody`: badan Measurement Protocol. Fungsi murni (waktu diinjeksi).
pub fn build_body(ev: &GaEvent<'_>, now_ms: i64, env: &str) -> Value {
    let timestamp_micros = resolve_timestamp_micros(ev.occurred_at_ms, now_ms);
    let mut params = ev.params.clone();
    params.insert(
        "session_id".into(),
        Value::String((timestamp_micros / 1_000_000).to_string()),
    );
    params.insert("engagement_time_msec".into(), json!(1));
    if ev.debug || env == "staging" {
        params.insert("debug_mode".into(), json!(1));
    }

    let mut body = Map::new();
    body.insert(
        "client_id".into(),
        Value::String(truncate(ev.client_id, GA_PARAM_MAX_LENGTH)),
    );
    body.insert(
        "user_id".into(),
        Value::String(truncate(ev.user_id, GA_PARAM_MAX_LENGTH)),
    );
    body.insert("timestamp_micros".into(), json!(timestamp_micros));
    body.insert("non_personalized_ads".into(), json!(false));
    if !ev.user_properties.is_empty() {
        body.insert(
            "user_properties".into(),
            Value::Object(user_properties_json(&ev.user_properties)),
        );
    }
    body.insert(
        "events".into(),
        json!([{ "name": ev.event_name, "params": Value::Object(params) }]),
    );
    Value::Object(body)
}

/// Mengambil `validationMessages` dari respons `/debug/mp/collect` (dicatat, tidak menggagalkan).
fn validation_messages(text: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let messages = parsed.get("validationMessages")?.as_array()?;
    (!messages.is_empty()).then(|| Value::Array(messages.clone()))
}

/// Satu HTTP POST. `Err` berisi pesan yang sudah di-redact.
async fn post(
    state: &AppState,
    cfg: &GaConfig,
    event_name: &str,
    body: &Value,
) -> Result<(), String> {
    let redact_err = |e: reqwest::Error| cfg.redact(&e.without_url().to_string());
    let response = state
        .http
        .post(cfg.endpoint())
        .query(&[
            ("measurement_id", cfg.measurement_id.as_str()),
            ("api_secret", cfg.api_secret.as_str()),
        ])
        .timeout(HTTP_TIMEOUT)
        .json(body)
        .send()
        .await
        .map_err(redact_err)?;
    let status = response.status();
    let text = response.text().await.map_err(redact_err)?;

    if cfg.validate_only()
        && let Some(messages) = validation_messages(&text)
    {
        tracing::warn!(event = event_name, messages = %messages, "GA payload ditolak validasi");
    }
    if !status.is_success() {
        return Err(format!("GA responded {}", status.as_u16()));
    }
    tracing::info!(
        event = event_name,
        env = cfg.env(),
        mid = %cfg.measurement_suffix(),
        status = status.as_u16(),
        "GA event sent"
    );
    Ok(())
}

#[derive(sqlx::FromRow)]
struct SendRow {
    event_name: String,
    params: Option<Value>,
    occurred_at: NaiveDateTime,
    attempts: i32,
    user_uuid: Option<String>,
    ga_client_id: Option<String>,
}

/// `AnalyticsConsumer.handleSend` untuk satu baris. Tidak pernah melempar.
pub async fn send_event(state: &AppState, id: i32) {
    // Klaim atomik (sekaligus lock). 0 baris = sudah diproses pengirim lain.
    let claimed = sqlx::query(
        r#"UPDATE "analyticsEvent"
           SET status = 'SENDING', "sendingAt" = $2, attempts = attempts + 1, "updatedAt" = $2
           WHERE id = $1 AND status = 'PENDING'"#,
    )
    .bind(id)
    .bind(now_naive())
    .execute(&state.db)
    .await;
    let claimed = match claimed {
        Ok(done) => done.rows_affected(),
        Err(err) => {
            // Belum ada HTTP dan baris masih PENDING: sweeper mengambilnya lagi.
            tracing::error!(id, error = %err, "Gagal klaim pengiriman analyticsEvent");
            return;
        }
    };
    if claimed == 0 {
        return;
    }

    let row = sqlx::query_as::<_, SendRow>(
        r#"SELECT e."eventName" AS event_name, e.params, e."occurredAt" AS occurred_at,
                  e.attempts, u.uuid::text AS user_uuid, u."gaClientId" AS ga_client_id
           FROM "analyticsEvent" e
           LEFT JOIN users u ON u.id = e."userId"
           WHERE e.id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(err) => {
            revert_to_pending(state, id, Some(&format!("fetch gagal: {err}"))).await;
            return;
        }
    };

    let cfg = GaConfig::from_state(state);
    if !cfg.enabled() {
        finalize(state, id, STATUS_SKIPPED, Some("GA disabled")).await;
        return;
    }

    let client_id = row.ga_client_id.filter(|s| !s.is_empty());
    let user_uuid = row.user_uuid.filter(|s| !s.is_empty());
    let (Some(client_id), Some(user_uuid)) = (client_id, user_uuid) else {
        // Umumnya balapan dengan pembuatan gaClientId; sweeper mencoba lagi.
        revert_to_pending(state, id, Some("gaClientId/user belum tersedia")).await;
        return;
    };

    let params = match row.params {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let event = GaEvent {
        client_id: &client_id,
        user_id: &user_uuid,
        event_name: &row.event_name,
        user_properties: user_properties_from_params(&params),
        params,
        occurred_at_ms: Some(row.occurred_at.and_utc().timestamp_millis()),
        debug: GA_DEBUG_USER_UUIDS.contains(&user_uuid.as_str()),
    };
    let body = build_body(&event, Utc::now().timestamp_millis(), cfg.env());

    match post(state, &cfg, &row.event_name, &body).await {
        Ok(()) => finalize(state, id, STATUS_SENT, None).await,
        Err(error) if row.attempts >= MAX_SEND_ATTEMPTS => {
            finalize(state, id, STATUS_FAILED, Some(&error)).await;
            // Hanya transisi ke FAILED yang dilaporkan, bukan tiap retry.
            tracing::error!(
                id,
                event = %row.event_name,
                attempts = row.attempts,
                error = %error,
                "GA event FAILED"
            );
        }
        Err(error) => revert_to_pending(state, id, Some(&error)).await,
    }
}

/// Kirim beberapa baris dengan konkurensi 5 (`concurrency: 5` Bull).
pub async fn send_all(state: &AppState, ids: Vec<i32>) {
    futures::stream::iter(ids)
        .map(|id| send_event(state, id))
        .buffer_unordered(SEND_CONCURRENCY)
        .collect::<Vec<()>>()
        .await;
}

/// `finalize`: status akhir. `sentAt` hanya untuk SENT.
async fn finalize(state: &AppState, id: i32, status: &str, last_error: Option<&str>) {
    let now = now_naive();
    let sent_at = (status == STATUS_SENT).then_some(now);
    let res = sqlx::query(
        r#"UPDATE "analyticsEvent"
           SET status = $2, "lastError" = $3, "sentAt" = $4, "updatedAt" = $5
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(status)
    .bind(last_error)
    .bind(sent_at)
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(err) = res {
        // Baris tertinggal di SENDING; sweeper melaporkannya sebagai anomali.
        tracing::error!(id, status, error = %err, "Gagal set status analyticsEvent");
    }
}

/// `revertToPending`: kembalikan ke PENDING (sendingAt tidak disentuh, sama seperti Node).
pub async fn revert_to_pending(state: &AppState, id: i32, last_error: Option<&str>) {
    let res = sqlx::query(
        r#"UPDATE "analyticsEvent"
           SET status = 'PENDING', "lastError" = $2, "updatedAt" = $3
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(last_error)
    .bind(now_naive())
    .execute(&state.db)
    .await;
    if let Err(err) = res {
        tracing::error!(id, error = %err, "Gagal mengembalikan analyticsEvent ke PENDING");
    }
}

/// `process.env` `analytics-ga` atau `all` di `MZ_JOBS`: jalur login boleh
/// mengirim langsung (selain itu sweeper Node/Rust yang mengirim).
pub fn direct_send_enabled(state: &AppState) -> bool {
    mz_jobs::enabled_names(state)
        .iter()
        .any(|n| n == "analytics-ga" || n == "all")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event<'a>(params: Map<String, Value>, props: Map<String, Value>) -> GaEvent<'a> {
        GaEvent {
            client_id: "1234567890.1700000000",
            user_id: "5f0c6f1e-0000-4000-8000-000000000000",
            event_name: "first_tracked_hour",
            params,
            user_properties: props,
            occurred_at_ms: Some(1_700_000_000_000),
            debug: false,
        }
    }

    const NOW_MS: i64 = 1_700_000_100_000;

    #[test]
    fn body_shape_matches_measurement_protocol() {
        let params = json!({ "platform": "desktop", "total_seconds": 3600 });
        let props = user_properties_from_params(
            &json!({ "company_id": 7 })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let body = build_body(
            &event(params.as_object().cloned().unwrap_or_default(), props),
            NOW_MS,
            "production",
        );
        assert_eq!(body["client_id"], json!("1234567890.1700000000"));
        assert_eq!(
            body["user_id"],
            json!("5f0c6f1e-0000-4000-8000-000000000000")
        );
        assert_eq!(body["timestamp_micros"], json!(1_700_000_000_000_000_i64));
        assert_eq!(body["non_personalized_ads"], json!(false));
        assert_eq!(
            body["user_properties"],
            json!({ "company_id": { "value": 7 } })
        );
        let ev = &body["events"][0];
        assert_eq!(ev["name"], json!("first_tracked_hour"));
        assert_eq!(ev["params"]["session_id"], json!("1700000000"));
        assert_eq!(ev["params"]["engagement_time_msec"], json!(1));
        assert_eq!(ev["params"]["total_seconds"], json!(3600));
        assert!(ev["params"].get("debug_mode").is_none());
    }

    #[test]
    fn debug_mode_only_for_staging_or_debug_user() {
        let b = build_body(&event(Map::new(), Map::new()), NOW_MS, "staging");
        assert_eq!(b["events"][0]["params"]["debug_mode"], json!(1));
        let mut ev = event(Map::new(), Map::new());
        ev.debug = true;
        let b = build_body(&ev, NOW_MS, "production");
        assert_eq!(b["events"][0]["params"]["debug_mode"], json!(1));
    }

    #[test]
    fn user_properties_omitted_when_empty() {
        let b = build_body(&event(Map::new(), Map::new()), NOW_MS, "production");
        assert!(b.get("user_properties").is_none());
    }

    #[test]
    fn company_id_user_property_rules() {
        let p = |v: Value| user_properties_from_params(&v.as_object().cloned().unwrap_or_default());
        assert_eq!(p(json!({ "company_id": "" })), Map::new());
        assert_eq!(p(json!({ "company_id": null })), Map::new());
        assert_eq!(
            p(json!({ "company_id": "abc" })),
            json!({ "company_id": "abc" })
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
    }

    #[test]
    fn long_strings_are_truncated_like_node() {
        let long = "x".repeat(200);
        let mut ev = event(Map::new(), Map::new());
        ev.client_id = &long;
        let props = json!({ "company_id": long })
            .as_object()
            .cloned()
            .unwrap_or_default();
        ev.user_properties = props;
        let b = build_body(&ev, NOW_MS, "production");
        assert_eq!(b["client_id"].as_str().map(str::len), Some(100));
        assert_eq!(
            b["user_properties"]["company_id"]["value"]
                .as_str()
                .map(str::len),
            Some(36)
        );
    }

    #[test]
    fn timestamp_is_clamped_to_71_hours_and_not_future() {
        let now = 1_000_000_000_000_i64;
        let old = now - 100 * 60 * 60 * 1000;
        assert_eq!(
            resolve_timestamp_micros(Some(old), now),
            (now - GA_MAX_EVENT_AGE_MS) * 1000
        );
        assert_eq!(resolve_timestamp_micros(Some(now + 5_000), now), now * 1000);
        assert_eq!(resolve_timestamp_micros(None, now), now * 1000);
    }

    #[test]
    fn redact_hides_secret() {
        let cfg = GaConfig {
            measurement_id: "G-ABCDEF".into(),
            api_secret: "s3cr3t".into(),
            env: "production".into(),
        };
        assert_eq!(
            cfg.redact("error for url ?api_secret=s3cr3t"),
            "error for url ?api_secret=***"
        );
        assert_eq!(cfg.redact(""), "unknown error");
        assert_eq!(cfg.measurement_suffix(), "ABCDEF");
        assert!(cfg.enabled());
        assert!(!cfg.validate_only());
        assert_eq!(cfg.endpoint(), PROD_ENDPOINT);
    }

    #[test]
    fn validation_messages_only_when_non_empty() {
        assert!(validation_messages(r#"{"validationMessages":[]}"#).is_none());
        assert!(validation_messages("not json").is_none());
        assert!(validation_messages(r#"{"validationMessages":[{"description":"x"}]}"#).is_some());
    }
}
