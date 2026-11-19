//! Port `src/analytics/mixpanel.service.ts` + `mixpanel-track-client.service.ts` +
//! `mixpanel.consumer.ts`.
//!
//! Node mengantrekan event ke Bull (`analytics`, job `mixpanel.track`, 5 percobaan, backoff
//! eksponensial 30 dtk, concurrency 5). Di sini antreannya `mz_core::queue` (Redis, kunci
//! `mz:q:mixpanel:*`, terpisah dari Bull) sehingga event yang sedang di-retry tahan restart.
//!
//! - [`track`] tetap sinkron: membangun payload beku (termasuk `time` dan `$insert_id`), lalu
//!   `enqueue` dari `crate::bg::spawn` (ditunggu saat shutdown) dengan `$insert_id` sebagai id job (dedup).
//! - Kalau enqueue gagal (Redis mati / belum tersambung) FALLBACK: kirim langsung dengan
//!   retry in-memory (5x, backoff 30 dtk) — tidak tahan restart, tapi tidak ada event yang
//!   hilang hanya karena Redis sedang bermasalah.
//! - [`spawn_worker`] memasang pengirimnya (concurrency 5). Pengirim membaca host & token SAAT
//!   kirim; token kosong = buang diam-diam (kredensial dicabut saat job antre).
//!
//! DEVIASI kecil: Node juga memanggil Sentry saat event final gagal; di sini cukup
//! `tracing::error!` (Sentry dihapus, lihat `plan/deviations/mz-core.md`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::queue::{self, Job, JobOpts, Outcome, WorkerConfig};

const DEFAULT_API_HOST: &str = "https://api.mixpanel.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// Nama antrean (`mz:q:mixpanel:*`).
pub const QUEUE_NAME: &str = "mixpanel";
/// `MIXPANEL_JOB_ATTEMPTS`.
const ATTEMPTS: u32 = 5;
const WORKER_CONCURRENCY: usize = 5;
/// Batas enqueue sebelum jatuh ke fallback langsung.
const ENQUEUE_TIMEOUT: Duration = Duration::from_secs(3);
/// `MIXPANEL_JOB_TIMEOUT_MS` — harus > `HTTP_TIMEOUT`.
const JOB_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF: Duration = Duration::from_secs(30);
const INSERT_ID_LENGTH: usize = 32;
const PROPERTY_MAX_LENGTH: usize = 255;
const MAX_LIST_ITEMS: usize = 20;

/// `MixpanelEvent` (`analytics/constants.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixpanelEvent {
    EmailVerified,
    OrganizationCreated,
    SignUp,
}

impl MixpanelEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EmailVerified => "Email Verified",
            Self::OrganizationCreated => "Organization Created",
            Self::SignUp => "Sign Up",
        }
    }
}

pub struct TrackInput {
    pub event: MixpanelEvent,
    pub distinct_id: String,
    pub dedup_key: String,
    pub properties: Map<String, Value>,
    /// Milidetik epoch; `None` = sekarang.
    pub occurred_at_ms: Option<i64>,
}

/// `UUID_PATTERN` `mixpanel.service.ts` = bentuk `8-4-4-4-12` hex.
fn is_uuid(s: &str) -> bool {
    crate::uuid_rules::is_uuid_shape(s)
}

fn valid_key(k: &str) -> bool {
    if k == "$device_id" || k == "$user_id" {
        return true;
    }
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && k.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn trim_str(s: &str) -> String {
    s.trim().chars().take(PROPERTY_MAX_LENGTH).collect()
}

fn sanitize(raw: Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in raw {
        if v.is_null() || !valid_key(&k) {
            continue;
        }
        match v {
            Value::String(s) => {
                let t = trim_str(&s);
                if !t.is_empty() {
                    out.insert(k, Value::String(t));
                }
            }
            Value::Number(_) | Value::Bool(_) => {
                out.insert(k, v);
            }
            Value::Array(items) => {
                let items: Vec<Value> = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(trim_str)
                    .filter(|s| !s.is_empty())
                    .take(MAX_LIST_ITEMS)
                    .map(Value::String)
                    .collect();
                out.insert(k, Value::Array(items));
            }
            _ => {}
        }
    }
    out
}

pub fn insert_id(event: &str, dedup_key: &str) -> String {
    let mut h = hex::encode(Sha256::digest(format!("{event}:{dedup_key}").as_bytes()));
    h.truncate(INSERT_ID_LENGTH);
    h
}

/// Payload beku (`MixpanelTrackPayload`): disimpan di job; token TIDAK ikut (dibaca saat kirim).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    #[serde(default)]
    event: String,
    #[serde(default)]
    distinct_id: String,
    #[serde(default)]
    insert_id: String,
    #[serde(default)]
    time: i64,
    #[serde(default)]
    properties: Map<String, Value>,
}

/// Bangun payload beku. `None` bila `distinct_id` bukan UUID.
fn build_payload(input: TrackInput, env: &str) -> Option<Value> {
    let distinct_id = input.distinct_id.trim().to_owned();
    if !is_uuid(&distinct_id) {
        return None;
    }
    let mut props = Map::new();
    props.insert("env".into(), Value::String(env.to_owned()));
    props.insert("source".into(), Value::String("server".into()));
    props.insert("user_id".into(), Value::String(distinct_id.clone()));
    let has_device = input
        .properties
        .get("$device_id")
        .is_some_and(|v| !v.is_null());
    props.extend(input.properties);
    if has_device {
        props.insert("$user_id".into(), Value::String(distinct_id.clone()));
    }
    Some(json!({
        "event": input.event.as_str(),
        "distinctId": distinct_id,
        "insertId": insert_id(input.event.as_str(), &input.dedup_key),
        "time": input
            .occurred_at_ms
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        "properties": sanitize(props),
    }))
}

/// Isi body `/track` dari payload beku + kredensial saat kirim (`MixpanelTrackClient.track`).
fn request_body(p: &Payload, token: &str) -> Value {
    let mut props = p.properties.clone();
    props.insert("token".into(), Value::String(token.to_owned()));
    props.insert("distinct_id".into(), Value::String(p.distinct_id.clone()));
    props.insert("$insert_id".into(), Value::String(p.insert_id.clone()));
    props.insert("time".into(), Value::from(p.time));
    json!([{ "event": p.event, "properties": props }])
}

/// Klasifikasi status HTTP: sukses / retry (429, 5xx) / ditolak final (4xx lain, 3xx).
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Sent,
    Retry,
    Rejected,
}

fn classify(status: u16) -> Verdict {
    match status {
        200..=299 => Verdict::Sent,
        429 | 500..=599 => Verdict::Retry,
        _ => Verdict::Rejected,
    }
}

enum SendResult {
    Sent,
    /// Kredensial kosong: buang diam-diam.
    Disabled,
    Retry(String),
    Rejected(String),
}

/// Satu percobaan kirim. Host & token dibaca dari konfigurasi SAAT ini.
async fn send(state: &AppState, p: &Payload) -> SendResult {
    let token = state
        .config
        .var("MIXPANEL_PROJECT_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_owned();
    if token.is_empty() {
        return SendResult::Disabled;
    }
    let host = state
        .config
        .var("MIXPANEL_API_HOST")
        .unwrap_or_else(|| DEFAULT_API_HOST.into())
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let res = state
        .http
        .post(format!("{host}/track?strict=1"))
        .timeout(HTTP_TIMEOUT)
        .json(&request_body(p, &token))
        .send()
        .await;
    // Jaring pengaman terakhir: token tidak boleh masuk log.
    let redact = |m: String| m.replace(&token, "***");
    match res {
        Ok(r) => {
            let status = r.status().as_u16();
            match classify(status) {
                Verdict::Sent => {
                    tracing::info!(event = %p.event, status, "Mixpanel event sent");
                    SendResult::Sent
                }
                Verdict::Retry => SendResult::Retry(format!("Mixpanel responded {status}")),
                Verdict::Rejected => {
                    if status == 401 || status == 403 {
                        tracing::error!(
                            "Mixpanel menolak MIXPANEL_PROJECT_TOKEN - cek token-nya milik project yang benar."
                        );
                    }
                    SendResult::Rejected(format!("Mixpanel responded {status}"))
                }
            }
        }
        Err(err) => SendResult::Retry(redact(err.to_string())),
    }
}

/// Handler job (`MixpanelConsumer.handleTrack`).
async fn handle(state: &AppState, job: Job) -> Outcome {
    let payload: Payload = match serde_json::from_value(job.data.clone()) {
        Ok(p) => p,
        Err(e) => return Outcome::Drop(format!("payload tidak valid: {e}")),
    };
    if payload.event.is_empty() || payload.distinct_id.is_empty() || payload.insert_id.is_empty() {
        return Outcome::Drop("payload tanpa event/distinctId/insertId".into());
    }
    match send(state, &payload).await {
        SendResult::Sent => Outcome::Done,
        SendResult::Disabled => Outcome::Drop("MIXPANEL_PROJECT_TOKEN kosong".into()),
        SendResult::Rejected(err) => {
            tracing::error!(
                event = %payload.event,
                attempt = job.attempt,
                "Mixpanel dibuang (ditolak, tidak akan berubah bila diulang): {err}"
            );
            Outcome::Drop(err)
        }
        SendResult::Retry(err) => {
            if !job.is_last_attempt() {
                tracing::warn!(
                    event = %payload.event,
                    "Mixpanel gagal (percobaan {}/{}), dicoba ulang: {err}",
                    job.attempt,
                    job.max_attempts
                );
            }
            // Percobaan terakhir: antrean yang mencatat error "percobaan habis".
            Outcome::Retry(err)
        }
    }
}

/// Pasang worker pengirim Mixpanel (`@Process({ concurrency: 5 })`). Selalu dijalankan,
/// tidak di-gate `MZ_JOBS`: antrean hanya diisi Rust dan kuncinya terpisah dari Bull Node.
pub fn spawn_worker(state: &AppState) -> tokio::task::JoinHandle<()> {
    let st = state.clone();
    let mut cfg = WorkerConfig::new(WORKER_CONCURRENCY);
    cfg.handler_timeout = JOB_TIMEOUT;
    queue::spawn_worker_with(state, QUEUE_NAME, cfg, move |job| {
        let st = st.clone();
        async move { handle(&st, job).await }
    })
}

/// Fallback saat Redis tidak bisa dipakai: kirim langsung, retry in-memory.
async fn send_direct_with_retry(state: &AppState, payload: Payload) {
    let mut delay = BACKOFF;
    for attempt in 1..=ATTEMPTS {
        match send(state, &payload).await {
            SendResult::Sent | SendResult::Disabled => return,
            SendResult::Rejected(err) => {
                tracing::error!(event = %payload.event, "Mixpanel menolak event: {err}");
                return;
            }
            SendResult::Retry(err) => {
                tracing::warn!(event = %payload.event, attempt, "Mixpanel retry: {err}");
            }
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
    }
    tracing::error!(event = %payload.event, "Mixpanel dibuang setelah {ATTEMPTS} percobaan");
}

/// `MixpanelService.track` — tidak pernah gagal di sisi pemanggil.
pub fn track(state: &AppState, input: TrackInput) {
    let token = state
        .config
        .var("MIXPANEL_PROJECT_TOKEN")
        .unwrap_or_default();
    if token.trim().is_empty() {
        return;
    }
    let env = state
        .config
        .var("MIXPANEL_ENV")
        .unwrap_or_else(|| "development".into())
        .trim()
        .to_lowercase();
    let Some(data) = build_payload(input, &env) else {
        return;
    };
    let Ok(payload) = serde_json::from_value::<Payload>(data.clone()) else {
        return;
    };
    let state = state.clone();
    crate::bg::spawn(async move {
        let opts = JobOpts::new(ATTEMPTS, BACKOFF);
        let enq = tokio::time::timeout(
            ENQUEUE_TIMEOUT,
            queue::enqueue(
                &state,
                QUEUE_NAME,
                Some(payload.insert_id.clone()),
                data,
                opts,
            ),
        )
        .await;
        let err = match enq {
            Ok(Ok(_)) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("timeout {ENQUEUE_TIMEOUT:?}"),
        };
        tracing::error!(
            event = %payload.event,
            "Gagal enqueue event Mixpanel, kirim langsung tanpa antrean: {err}"
        );
        send_direct_with_retry(&state, payload).await;
    });
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const UID: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn input(props: Value) -> TrackInput {
        TrackInput {
            event: MixpanelEvent::SignUp,
            distinct_id: format!(" {UID} "),
            dedup_key: "k".into(),
            properties: props.as_object().cloned().unwrap_or_default(),
            occurred_at_ms: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn payload_beku_tanpa_token() {
        let v = build_payload(
            input(json!({"signup_type": "invite", "BadKey": 1, "$device_id": "dev"})),
            "production",
        )
        .unwrap_or_default();
        assert_eq!(v["event"], "Sign Up");
        assert_eq!(v["distinctId"], UID);
        assert_eq!(v["time"], 1_700_000_000_000_i64);
        assert_eq!(v["insertId"], insert_id("Sign Up", "k"));
        let p = &v["properties"];
        assert_eq!(p["env"], "production");
        assert_eq!(p["source"], "server");
        assert_eq!(p["user_id"], UID);
        assert_eq!(p["$user_id"], UID);
        assert_eq!(p["$device_id"], "dev");
        assert_eq!(p["signup_type"], "invite");
        assert!(p.get("BadKey").is_none());
        assert!(p.get("token").is_none());
    }

    #[test]
    fn tanpa_device_id_tidak_ada_user_id_bawaan_mixpanel() {
        let v = build_payload(input(json!({})), "development").unwrap_or_default();
        assert!(v["properties"].get("$user_id").is_none());
    }

    #[test]
    fn distinct_id_bukan_uuid_ditolak() {
        let mut i = input(json!({}));
        i.distinct_id = "bukan-uuid".into();
        assert!(build_payload(i, "development").is_none());
    }

    #[test]
    fn payload_round_trip_dan_body_kirim() {
        let v = build_payload(input(json!({"a_b": 1})), "development").unwrap_or_default();
        let p: Result<Payload, _> = serde_json::from_value(v);
        let Ok(p) = p else {
            panic!("payload harus bisa dibaca ulang");
        };
        let body = request_body(&p, "tok");
        let first = &body[0];
        assert_eq!(first["event"], "Sign Up");
        assert_eq!(first["properties"]["token"], "tok");
        assert_eq!(first["properties"]["distinct_id"], UID);
        assert_eq!(first["properties"]["$insert_id"], p.insert_id.as_str());
        assert_eq!(first["properties"]["time"], 1_700_000_000_000_i64);
        assert_eq!(first["properties"]["a_b"], 1);
    }

    #[test]
    fn klasifikasi_status() {
        assert_eq!(classify(200), Verdict::Sent);
        assert_eq!(classify(204), Verdict::Sent);
        assert_eq!(classify(429), Verdict::Retry);
        assert_eq!(classify(500), Verdict::Retry);
        assert_eq!(classify(503), Verdict::Retry);
        for s in [400, 401, 403, 404, 302] {
            assert_eq!(classify(s), Verdict::Rejected, "status {s}");
        }
    }
}
