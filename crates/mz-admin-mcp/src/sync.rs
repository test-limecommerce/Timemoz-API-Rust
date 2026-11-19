//! Port `src/admin-mixpanel/mixpanel-client.service.ts` + `mixpanel-sync.service.ts`:
//! tarik event dari Mixpanel Export API (NDJSON) ke tabel `mixpanelEvent`.
//!
//! URL dasar Export API bisa dialihkan lewat env `MZ_MIXPANEL_EXPORT_BASE` (BUKAN fitur Node,
//! hanya untuk uji dengan server tiruan; tanpa env sama dengan Node).

use std::time::Duration;

use chrono::{Datelike, NaiveDate, Utc};
use mz_core::jsfmt::num_string;
use mz_core::{AppState, db::advisory_xact_lock};
use mz_jobs::{CronJob, JobFuture, Schedule};
use serde_json::{Map, Value};
use sqlx::{Postgres, QueryBuilder};

use crate::analytics::js_ymd;

const TRACKED_EVENTS: [&str; 3] = ["Page Viewed", "CTA Clicked", "Form Submitted Succeeded"];
const LANDING_PAGE_DOMAIN: &str = "timemoz.com";
/// 22 kolom x 2000 = 44k bind parameter (< 65.535).
const INSERT_CHUNK_SIZE: usize = 2000;
const DEFAULT_BASE: &str = "https://data.mixpanel.com/api/2.0";
/// `EXPORT_IDLE_TIMEOUT_MS` / `EXPORT_TOTAL_TIMEOUT_MS`.
const EXPORT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const EXPORT_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const RANGE_DELAY: Duration = Duration::from_secs(2);

/// Satu baris `mixpanelEvent` (tanpa `id`/`createdAt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub date: String,
    pub event_name: String,
    pub distinct_id: String,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_domain: Option<String>,
    pub page_name: Option<String>,
    pub page_category: Option<String>,
    pub cta_name: Option<String>,
    pub cta_location: Option<String>,
    pub form_name: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub device_type: Option<String>,
    pub browser: Option<String>,
    pub os: Option<String>,
    pub utm_term: Option<String>,
    pub utm_content: Option<String>,
    pub hour: Option<i32>,
    pub current_domain: Option<String>,
    pub path: Option<String>,
}

/// `String(v)` JS untuk nilai JSON.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), num_string),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    js_string(x)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `str(p[key])`: `null`/hilang → NULL.
fn opt_str(p: &Map<String, Value>, key: &str) -> Option<String> {
    p.get(key).filter(|v| !v.is_null()).map(js_string)
}

/// `parseDomain`: nilai falsy → `null`; URL tak valid → `null`; selain itu `hostname`
/// (string kosong untuk URL tanpa host, seperti `new URL('mailto:x').hostname`).
fn parse_domain(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let falsy = match v {
        Value::Null | Value::Bool(false) => true,
        Value::String(s) => s.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    };
    if falsy {
        return None;
    }
    let url = url::Url::parse(&js_string(v)).ok()?;
    Some(url.host_str().unwrap_or_default().to_owned())
}

/// `new Date(time * 1000).getUTCHours()`.
fn utc_hour(time: f64) -> Option<i32> {
    let ms = (time * 1000.0).trunc();
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let ms = ms as i64;
    i32::try_from(ms.div_euclid(3_600_000).rem_euclid(24)).ok()
}

/// `toRow`.
pub fn to_row(
    p: &Map<String, Value>,
    date: &str,
    event: &str,
    current_domain: Option<String>,
) -> EventRow {
    let country = p
        .get("$country_code")
        .filter(|v| !v.is_null())
        .or_else(|| p.get("mp_country_code"))
        .filter(|v| !v.is_null())
        .map(js_string);
    EventRow {
        date: date.to_owned(),
        event_name: event.to_owned(),
        distinct_id: p
            .get("distinct_id")
            .filter(|v| !v.is_null())
            .map(js_string)
            .unwrap_or_default(),
        utm_source: opt_str(p, "utm_source"),
        utm_medium: opt_str(p, "utm_medium"),
        utm_campaign: opt_str(p, "utm_campaign"),
        referrer_domain: opt_str(p, "referrer_domain"),
        page_name: opt_str(p, "page_name"),
        page_category: opt_str(p, "page_category"),
        cta_name: opt_str(p, "cta_name"),
        cta_location: opt_str(p, "cta_location"),
        form_name: opt_str(p, "form_name"),
        country,
        city: opt_str(p, "$city"),
        device_type: opt_str(p, "$device"),
        browser: opt_str(p, "$browser"),
        os: opt_str(p, "$os"),
        utm_term: opt_str(p, "utm_term"),
        utm_content: opt_str(p, "utm_content"),
        hour: match p.get("time") {
            Some(Value::Number(n)) => n.as_f64().and_then(utc_hour),
            _ => None,
        },
        current_domain,
        path: opt_str(p, "path"),
    }
}

// ───── client ─────

struct Client {
    base: String,
    username: String,
    secret: String,
    project_id: Option<String>,
}

impl Client {
    fn new(state: &AppState) -> Self {
        Self {
            base: state
                .config
                .var("MZ_MIXPANEL_EXPORT_BASE")
                .map(|v| v.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| DEFAULT_BASE.to_owned()),
            // Node: `${undefined}` → "undefined" bila env kosong.
            username: state
                .config
                .var("MIXPANEL_USERNAME")
                .unwrap_or_else(|| "undefined".to_owned()),
            secret: state
                .config
                .var("MIXPANEL_SECRET_KEY")
                .unwrap_or_else(|| "undefined".to_owned()),
            project_id: state.config.var("MIXPANEL_PROJECT_ID"),
        }
    }

    /// `openExportStream`: 429 → tunggu `Retry-After` (bawaan 60 dtk) lalu ulang (maks 3 kali);
    /// status non-2xx lain → error.
    async fn open(
        &self,
        state: &AppState,
        event: &str,
        from: &str,
        to: &str,
        mut retries: u32,
    ) -> anyhow::Result<reqwest::Response> {
        loop {
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(p) = &self.project_id {
                query.push(("project_id", p.clone()));
            }
            query.push(("from_date", from.to_owned()));
            query.push(("to_date", to.to_owned()));
            query.push(("event", serde_json::to_string(&[event])?));
            let req = state
                .http
                .get(format!("{}/export", self.base))
                .basic_auth(&self.username, Some(&self.secret))
                .header("Accept", "text/plain")
                .query(&query)
                .timeout(EXPORT_TOTAL_TIMEOUT)
                .send();
            let resp = tokio::time::timeout(EXPORT_IDLE_TIMEOUT, req)
                .await
                .map_err(|_| anyhow::anyhow!("timeout of 120000ms exceeded"))??;
            let status = resp.status();
            if status.as_u16() == 429 && retries > 0 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(60);
                tracing::warn!(
                    "Mixpanel rate limit hit for \"{event}\" ({from}). Retrying in {retry_after}s... ({retries} retries left)"
                );
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                retries -= 1;
                continue;
            }
            if !status.is_success() {
                tracing::error!(
                    "Mixpanel export failed for \"{event}\" ({from} – {to}): status={}",
                    status.as_u16()
                );
                anyhow::bail!("Request failed with status code {}", status.as_u16());
            }
            return Ok(resp);
        }
    }
}

/// Satu baris NDJSON → properti event, atau `None` bila harus dilewati
/// (kosong / bukan objek / tanpa `properties` truthy).
fn parse_line(
    line: &str,
    line_no: usize,
    event: &str,
    from: &str,
    to: &str,
) -> anyhow::Result<Option<Map<String, Value>>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let parsed: Value = serde_json::from_str(line).map_err(|_| {
        anyhow::anyhow!(
            "Mixpanel export returned invalid JSON at line {line_no} for \"{event}\" ({from} – {to})"
        )
    })?;
    let Value::Object(mut obj) = parsed else {
        return Ok(None);
    };
    match obj.remove("properties") {
        Some(Value::Object(p)) => Ok(Some(p)),
        // JS: properti truthy non-objek tetap diteruskan lalu `p[...]` bernilai undefined →
        // baris dengan semua kolom kosong. Jarang; diperlakukan sebagai objek kosong.
        Some(Value::Array(_) | Value::String(_)) => Ok(Some(Map::new())),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => Ok(Some(Map::new())),
        Some(Value::Bool(true)) => Ok(Some(Map::new())),
        _ => Ok(None),
    }
}

/// `syncEventForDate`: unduh (stream) → filter domain → hapus + sisipkan dalam satu transaksi.
async fn sync_event_for_date(
    state: &AppState,
    client: &Client,
    event: &str,
    date: &str,
) -> anyhow::Result<()> {
    let mut resp = client.open(state, event, date, date, 3).await?;
    let mut rows: Vec<EventRow> = Vec::new();
    let mut fetched = 0_usize;
    let mut line_no = 0_usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut handle = |raw: &[u8], line_no: usize| -> anyhow::Result<()> {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim_end_matches('\r');
        if let Some(p) = parse_line(line, line_no, event, date, date)? {
            fetched += 1;
            let domain = parse_domain(p.get("$current_url"));
            // `null` = tak ada info URL → tetap disimpan.
            if domain.as_deref().is_none_or(|d| d == LANDING_PAGE_DOMAIN) {
                rows.push(to_row(&p, date, event, domain));
            }
        }
        Ok(())
    };
    loop {
        let chunk = tokio::time::timeout(EXPORT_IDLE_TIMEOUT, resp.chunk())
            .await
            .map_err(|_| anyhow::anyhow!("timeout of 120000ms exceeded"))??;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            line_no += 1;
            handle(line.get(..line.len() - 1).unwrap_or(&[]), line_no)?;
        }
    }
    if !buf.is_empty() {
        line_no += 1;
        handle(&buf, line_no)?;
    }
    tracing::info!(
        "Fetched {fetched} raw \"{event}\" events from Mixpanel for {date}; after domain filter: {}/{fetched} kept",
        rows.len()
    );

    let mut tx = state.db.begin().await?;
    advisory_xact_lock(&mut tx, &format!("{event}:{date}")).await?;
    sqlx::query(r#"DELETE FROM "mixpanelEvent" WHERE "date" = $1 AND "eventName" = $2"#)
        .bind(date)
        .bind(event)
        .execute(&mut *tx)
        .await?;
    for chunk in rows.chunks(INSERT_CHUNK_SIZE) {
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new(
            r#"INSERT INTO "mixpanelEvent" ("date","eventName","distinctId","utmSource","utmMedium",
               "utmCampaign","referrerDomain","pageName","pageCategory","ctaName","ctaLocation","formName",
               "country","city","deviceType","browser","os","utmTerm","utmContent","hour","currentDomain","path") "#,
        );
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(&r.date)
                .push_bind(&r.event_name)
                .push_bind(&r.distinct_id)
                .push_bind(&r.utm_source)
                .push_bind(&r.utm_medium)
                .push_bind(&r.utm_campaign)
                .push_bind(&r.referrer_domain)
                .push_bind(&r.page_name)
                .push_bind(&r.page_category)
                .push_bind(&r.cta_name)
                .push_bind(&r.cta_location)
                .push_bind(&r.form_name)
                .push_bind(&r.country)
                .push_bind(&r.city)
                .push_bind(&r.device_type)
                .push_bind(&r.browser)
                .push_bind(&r.os)
                .push_bind(&r.utm_term)
                .push_bind(&r.utm_content)
                .push_bind(r.hour)
                .push_bind(&r.current_domain)
                .push_bind(&r.path);
        });
        qb.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    tracing::info!(
        "Synced {}/{fetched} \"{event}\" events for {date} (landing page only)",
        rows.len()
    );
    Ok(())
}

/// `syncDate`: semua event terlacak; kegagalan satu event tidak menghentikan yang lain,
/// error ringkasan dilempar sesudah semuanya dicoba.
pub async fn sync_date(state: &AppState, date: &str) -> anyhow::Result<()> {
    let client = Client::new(state);
    let mut failed: Vec<String> = Vec::new();
    for event in TRACKED_EVENTS {
        if let Err(err) = sync_event_for_date(state, &client, event, date).await {
            failed.push(format!("\"{event}\""));
            tracing::error!("Mixpanel sync failed for \"{event}\" on {date}: {err}");
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("Mixpanel sync failed for {date}: {}", failed.join(", "))
    }
}

/// `syncRange`: tanggal demi tanggal (jeda 2 dtk), `cleanupOldData` selalu dijalankan.
pub async fn sync_range(state: &AppState, from: &str, to: &str) -> anyhow::Result<()> {
    let to_d = js_ymd(to);
    let mut current = js_ymd(from);
    let mut failed: Vec<String> = Vec::new();
    while let (Some(c), Some(t)) = (current, to_d) {
        if c > t {
            break;
        }
        let date = c.format("%Y-%m-%d").to_string();
        tracing::info!("Syncing Mixpanel data for {date}");
        if sync_date(state, &date).await.is_err() {
            failed.push(date);
        }
        current = c.succ_opt();
        if current.is_some_and(|n| n <= t) {
            tokio::time::sleep(RANGE_DELAY).await;
        }
    }
    if let Err(err) = cleanup_old_data(state).await {
        tracing::error!("Mixpanel cleanup failed: {err}");
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "Mixpanel sync failed for {} date(s): {}",
            failed.len(),
            failed.join(", ")
        )
    }
}

/// `cleanupOldData`: hapus event lebih tua dari 1 tahun (`date < cutoff`).
pub async fn cleanup_old_data(state: &AppState) -> anyhow::Result<()> {
    let cutoff = one_year_ago(Utc::now().date_naive());
    let res = sqlx::query(r#"DELETE FROM "mixpanelEvent" WHERE "date" < $1"#)
        .bind(cutoff.format("%Y-%m-%d").to_string())
        .execute(&state.db)
        .await?;
    if res.rows_affected() > 0 {
        tracing::info!(
            "Cleaned up {} Mixpanel events older than 1 year",
            res.rows_affected()
        );
    }
    Ok(())
}

/// `setFullYear(getFullYear() - 1)`: 29 Feb → 1 Mar.
fn one_year_ago(today: NaiveDate) -> NaiveDate {
    today
        .with_year(today.year() - 1)
        .or_else(|| NaiveDate::from_ymd_opt(today.year() - 1, 3, 1))
        .unwrap_or(today)
}

/// Tanggal kemarin (UTC), `yyyy-mm-dd`.
pub fn yesterday() -> String {
    (Utc::now().date_naive() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string()
}

/// `handleDailySync` (`@Cron('0 1 * * *', { timeZone: 'Asia/Jakarta' })`).
async fn daily_sync(state: AppState) -> anyhow::Result<()> {
    let date = yesterday();
    tracing::info!("Starting daily Mixpanel sync for {date}");
    if let Err(err) = sync_date(&state, &date).await {
        tracing::error!("Daily Mixpanel sync for {date} finished with errors: {err}");
    }
    if let Err(err) = cleanup_old_data(&state).await {
        tracing::error!("Mixpanel cleanup failed: {err}");
    }
    Ok(())
}

fn daily_job(state: AppState) -> JobFuture {
    Box::pin(daily_sync(state))
}

/// Cron `MixpanelSyncService.handleDailySync` (01:00 WIB).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "mixpanel-daily-sync",
        schedule: Schedule::daily(&[1], &[0]),
        timeout: Duration::from_secs(60 * 60),
        run: daily_job,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn to_row_maps_fields_like_node() {
        let p = props(json!({
            "distinct_id": 42, "utm_source": "g", "$country_code": "ID", "mp_country_code": "SG",
            "$city": null, "time": 1_700_000_000.9, "page_name": ["a", null, "b"], "path": true,
        }));
        let r = to_row(&p, "2026-05-01", "Page Viewed", None);
        assert_eq!(r.distinct_id, "42");
        assert_eq!(r.utm_source.as_deref(), Some("g"));
        assert_eq!(r.country.as_deref(), Some("ID"));
        assert_eq!(r.city, None);
        assert_eq!(r.page_name.as_deref(), Some("a,,b"));
        assert_eq!(r.path.as_deref(), Some("true"));
        // 1_700_000_000 = 2023-11-14T22:13:20Z
        assert_eq!(r.hour, Some(22));
        let p = props(json!({ "mp_country_code": "SG" }));
        let r = to_row(&p, "d", "e", Some("timemoz.com".into()));
        assert_eq!(r.country.as_deref(), Some("SG"));
        assert_eq!(r.distinct_id, "");
        assert_eq!(r.hour, None);
    }

    #[test]
    fn parse_domain_rules() {
        let d = |v: Value| parse_domain(Some(&v));
        assert_eq!(
            d(json!("https://timemoz.com/x?y=1")).as_deref(),
            Some("timemoz.com")
        );
        assert_eq!(
            d(json!("https://APP.timemoz.com/")).as_deref(),
            Some("app.timemoz.com")
        );
        assert_eq!(d(json!("not a url")), None);
        assert_eq!(d(json!("")), None);
        assert_eq!(d(json!(null)), None);
        assert_eq!(parse_domain(None), None);
        assert_eq!(d(json!("mailto:a@b.c")).as_deref(), Some(""));
    }

    #[test]
    fn utc_hour_negative_and_fraction() {
        assert_eq!(utc_hour(0.0), Some(0));
        assert_eq!(utc_hour(3600.0 * 5.0 + 10.0), Some(5));
        assert_eq!(utc_hour(-1.0), Some(23));
    }

    #[test]
    fn parse_line_rules() {
        let ok = |l: &str| parse_line(l, 1, "e", "d", "d");
        assert!(matches!(ok(""), Ok(None)));
        assert!(matches!(ok("   "), Ok(None)));
        assert!(matches!(ok("{\"event\":\"x\"}"), Ok(None)));
        assert!(matches!(ok("[1]"), Ok(None)));
        assert!(matches!(ok("{\"properties\":{\"a\":1}}"), Ok(Some(_))));
        assert!(ok("{oops").is_err());
    }

    #[test]
    fn one_year_ago_leap_day() {
        let d = NaiveDate::from_ymd_opt(2028, 2, 29).unwrap_or_default();
        assert_eq!(one_year_ago(d).to_string(), "2027-03-01");
        let d = NaiveDate::from_ymd_opt(2026, 11, 17).unwrap_or_default();
        assert_eq!(one_year_ago(d).to_string(), "2025-11-17");
    }
}
