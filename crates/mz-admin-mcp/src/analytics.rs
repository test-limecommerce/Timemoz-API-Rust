//! Port `src/admin-mixpanel/admin-mixpanel.service.ts`: query analitik atas tabel `mixpanelEvent`.
//!
//! Bentuk & urutan kunci JSON sama dengan Node (`serde_json` memakai `preserve_order`).
//! Angka desimal dikirim seperti `JSON.stringify` (`100`, bukan `100.0`).

use chrono::{Datelike, Duration, NaiveDate, Utc};
use mz_core::jsfmt::num;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

const BREAKDOWN_LIMIT: i64 = 20;
const DASHBOARD_DOMAIN: &str = "app.timemoz.com";
/// Batas pengaman jumlah titik deret waktu (Node tidak membatasi; rentang tahun 0001-9999
/// membuat respons jutaan baris). Rentang wajar (<= ~270 tahun harian) tidak terpengaruh.
const MAX_SERIES_POINTS: usize = 100_000;

/// `Partial<MixpanelDateRange>` dari query string.
#[derive(Debug, Clone, Default)]
pub struct Range {
    pub from: Option<String>,
    pub to: Option<String>,
    pub unit: Option<String>,
}

struct Resolved {
    from: String,
    to: String,
    unit: String,
}

/// `defaultRange()`: 29 hari sebelum hari ini s.d. hari ini (UTC).
fn default_range() -> (String, String) {
    let today = Utc::now().date_naive();
    let from = today - Duration::days(29);
    (iso(from), iso(today))
}

fn resolve(r: &Range) -> Resolved {
    let (df, dt) = default_range();
    Resolved {
        from: r.from.clone().unwrap_or(df),
        to: r.to.clone().unwrap_or(dt),
        unit: r.unit.clone().unwrap_or_else(|| "day".to_owned()),
    }
}

/// `date.toISOString().slice(0, 10)`. Tahun di luar 0000..9999 memakai format tahun
/// diperluas JS (`-005947-09-01T…`), lalu dipotong 10 karakter → `-005947-09` (paritas adm-mcp).
fn iso(d: NaiveDate) -> String {
    let y = d.year();
    if (0..=9999).contains(&y) {
        return d.format("%Y-%m-%d").to_string();
    }
    let sign = if y < 0 { '-' } else { '+' };
    let full = format!(
        "{sign}{:06}-{:02}-{:02}",
        y.unsigned_abs(),
        d.month(),
        d.day()
    );
    full.chars().take(10).collect()
}

/// `new Date(s + 'T00:00:00Z')` untuk `YYYY-MM-DD` (V8 menggulung hari 29-31 yang melebihi
/// panjang bulan ke bulan berikutnya). Tidak valid → `None` (Invalid Date).
pub fn js_ymd(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let y = i32::try_from(digits(0..4)?).ok()?;
    let m = digits(5..7)?;
    let d = digits(8..10)?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    NaiveDate::from_ymd_opt(y, m, 1)?.checked_add_signed(Duration::days(i64::from(d) - 1))
}

/// Hari dalam minggu `getUTCDay()` (0 = Minggu).
fn js_weekday(d: NaiveDate) -> u32 {
    d.weekday().num_days_from_sunday()
}

/// Mundur ke hari Senin (`diff = day === 0 ? -6 : 1 - day`).
fn week_start(d: NaiveDate) -> NaiveDate {
    let day = i64::from(js_weekday(d));
    let diff = if day == 0 { -6 } else { 1 - day };
    d + Duration::days(diff)
}

fn add_month(d: NaiveDate) -> Option<NaiveDate> {
    d.checked_add_months(chrono::Months::new(1))
}

/// `generateSeries`. `unit` selain `month`/`week` diperlakukan harian (cabang `else` Node).
fn generate_series(from: &str, to: &str, unit: &str) -> Vec<String> {
    let (Some(mut current), Some(to_date)) = (js_ymd(from), js_ymd(to)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if unit == "month" {
        current = current.with_day(1).unwrap_or(current);
        while current <= to_date && out.len() < MAX_SERIES_POINTS {
            out.push(iso(current));
            match add_month(current) {
                Some(n) => current = n,
                None => break,
            }
        }
    } else if unit == "week" {
        current = week_start(current);
        while current <= to_date && out.len() < MAX_SERIES_POINTS {
            out.push(iso(current));
            current += Duration::days(7);
        }
    } else {
        while current <= to_date && out.len() < MAX_SERIES_POINTS {
            out.push(iso(current));
            current += Duration::days(1);
        }
    }
    out
}

/// `bucketDate`: `day` apa adanya, `month` awal bulan, selain itu awal minggu (Senin).
fn bucket_date(date: &str, unit: &str) -> String {
    if unit == "day" {
        return date.to_owned();
    }
    let Some(d) = js_ymd(date) else {
        return date.to_owned();
    };
    if unit == "month" {
        return format!("{:04}-{:02}-01", d.year(), d.month());
    }
    iso(week_start(d))
}

fn build_time_series(rows: &[(String, i64)], series: &[String], unit: &str) -> Vec<(String, i64)> {
    let mut buckets: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (date, count) in rows {
        *buckets.entry(bucket_date(date, unit)).or_insert(0) += count;
    }
    series
        .iter()
        .map(|d| (d.clone(), buckets.get(d).copied().unwrap_or(0)))
        .collect()
}

fn series_json(series: &[(String, i64)]) -> Value {
    Value::Array(
        series
            .iter()
            .map(|(d, v)| json!({ "date": d, "value": v }))
            .collect(),
    )
}

fn series_total(series: &[(String, i64)]) -> i64 {
    series.iter().map(|(_, v)| *v).sum()
}

/// `Math.round`: setengah dibulatkan ke atas (+inf), juga untuk angka negatif.
pub fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

fn ratio(n: i64, d: i64, scale: f64, div: f64) -> f64 {
    if d > 0 {
        #[allow(clippy::cast_precision_loss)]
        let r = js_round((n as f64 / d as f64) * scale) / div;
        r
    } else {
        0.0
    }
}

/// `toBreakdown`.
fn to_breakdown(rows: &[(Option<String>, i64)]) -> Value {
    let total: i64 = rows.iter().map(|(_, c)| *c).sum();
    Value::Array(
        rows.iter()
            .map(|(label, count)| {
                json!({
                    "label": label.clone().unwrap_or_else(|| "(none)".to_owned()),
                    "value": count,
                    "percent": num(ratio(*count, total, 1000.0, 10.0)),
                })
            })
            .collect(),
    )
}

// ───── SQL ─────

/// `where` Prisma: `eventName`, filter opsional, rentang `date`, dan `DOMAIN_FILTER`
/// (`NOT { currentDomain: 'app.timemoz.com' }` → SQL `NOT ("currentDomain" = $n)`: baris
/// `currentDomain` NULL TIDAK ikut, logika tiga nilai SQL; paritas adm-mcp 2026-11-17).
#[derive(Clone, Default)]
struct Filter {
    event: &'static str,
    form_name: Option<&'static str>,
    page_name: Option<&'static str>,
    hour_not_null: bool,
    from: Option<String>,
    to: Option<String>,
    /// `date < lt` (new-vs-returning).
    before: Option<String>,
}

fn base(event: &'static str, from: &str, to: &str) -> Filter {
    Filter {
        event,
        from: Some(from.to_owned()),
        to: Some(to.to_owned()),
        ..Filter::default()
    }
}

fn register(mut f: Filter) -> Filter {
    f.form_name = Some("Register");
    f
}

fn push_where(qb: &mut QueryBuilder<'static, Postgres>, f: &Filter) {
    qb.push(r#" WHERE "eventName" = "#).push_bind(f.event);
    if let Some(v) = f.form_name {
        qb.push(r#" AND "formName" = "#).push_bind(v);
    }
    if let Some(v) = f.page_name {
        qb.push(r#" AND "pageName" = "#).push_bind(v);
    }
    if f.hour_not_null {
        qb.push(r#" AND "hour" IS NOT NULL"#);
    }
    if let Some(v) = &f.from {
        qb.push(r#" AND "date" >= "#).push_bind(v.clone());
    }
    if let Some(v) = &f.to {
        qb.push(r#" AND "date" <= "#).push_bind(v.clone());
    }
    if let Some(v) = &f.before {
        qb.push(r#" AND "date" < "#).push_bind(v.clone());
    }
    qb.push(r#" AND NOT ("currentDomain" = "#)
        .push_bind(DASHBOARD_DOMAIN)
        .push(")");
}

async fn count(state: &AppState, f: &Filter) -> ApiResult<i64> {
    let mut qb = QueryBuilder::new(r#"SELECT count(*) FROM "mixpanelEvent""#);
    push_where(&mut qb, f);
    Ok(qb.build_query_scalar().fetch_one(&state.db).await?)
}

/// `findMany({ select: { distinctId }, distinct: ['distinctId'] }).length`.
async fn distinct_count(state: &AppState, f: &Filter) -> ApiResult<i64> {
    let mut qb = QueryBuilder::new(r#"SELECT count(DISTINCT "distinctId") FROM "mixpanelEvent""#);
    push_where(&mut qb, f);
    Ok(qb.build_query_scalar().fetch_one(&state.db).await?)
}

/// `groupBy({ by: [col], _count, orderBy: { _count: { id: 'desc' } }, take })`.
/// `col` HARUS konstanta kode (disisipkan ke SQL); urutan seri diikat dengan label supaya
/// stabil (Node tidak menjamin urutan untuk jumlah sama).
async fn group(
    state: &AppState,
    f: &Filter,
    col: &'static str,
    limit: Option<i64>,
) -> ApiResult<Vec<(Option<String>, i64)>> {
    let mut qb = QueryBuilder::new(format!(
        r#"SELECT "{col}"::text, count(*) FROM "mixpanelEvent""#
    ));
    push_where(&mut qb, f);
    qb.push(format!(
        r#" GROUP BY "{col}" ORDER BY count(*) DESC, "{col}" ASC NULLS LAST"#
    ));
    if let Some(l) = limit {
        qb.push(" LIMIT ").push_bind(l);
    }
    Ok(qb.build_query_as().fetch_all(&state.db).await?)
}

/// `groupBy({ by: ['date'], orderBy: { date: 'asc' } })`.
async fn daily(state: &AppState, f: &Filter) -> ApiResult<Vec<(String, i64)>> {
    let mut qb = QueryBuilder::new(r#"SELECT "date", count(*) FROM "mixpanelEvent""#);
    push_where(&mut qb, f);
    qb.push(r#" GROUP BY "date" ORDER BY "date" ASC"#);
    Ok(qb.build_query_as().fetch_all(&state.db).await?)
}

fn meta(from: &str, to: &str) -> Value {
    json!({ "from": from, "to": to })
}

// ───── endpoint ─────

pub async fn overview(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let views = base("Page Viewed", &r.from, &r.to);
    let total_page_views = count(state, &views).await?;
    let unique_visitors = distinct_count(state, &views).await?;
    let total_cta_clicks = count(state, &base("CTA Clicked", &r.from, &r.to)).await?;
    let total_registrations = count(
        state,
        &register(base("Form Submitted Succeeded", &r.from, &r.to)),
    )
    .await?;
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "total_page_views": total_page_views,
            "unique_visitors": unique_visitors,
            "total_cta_clicks": total_cta_clicks,
            "total_registrations": total_registrations,
            "conversion_rate": num(ratio(total_registrations, unique_visitors, 10000.0, 100.0)),
            "cta_click_rate": num(ratio(total_cta_clicks, unique_visitors, 10000.0, 100.0)),
        },
    }))
}

pub async fn traffic(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = base("Page Viewed", &r.from, &r.to);
    let series = generate_series(&r.from, &r.to, &r.unit);
    let daily_rows = daily(state, &f).await?;
    let by_source = group(state, &f, "utmSource", Some(BREAKDOWN_LIMIT)).await?;
    let by_medium = group(state, &f, "utmMedium", Some(BREAKDOWN_LIMIT)).await?;
    let by_referrer = group(state, &f, "referrerDomain", Some(BREAKDOWN_LIMIT)).await?;
    let sd = build_time_series(&daily_rows, &series, &r.unit);
    Ok(json!({
        "success": true,
        "meta": { "from": r.from, "to": r.to, "unit": r.unit },
        "data": {
            "series": series_json(&sd),
            "by_source": to_breakdown(&by_source),
            "by_medium": to_breakdown(&by_medium),
            "by_referrer": to_breakdown(&by_referrer),
            "summary": { "total": series_total(&sd) },
        },
    }))
}

pub async fn pages(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = base("Page Viewed", &r.from, &r.to);
    let by_page_name = group(state, &f, "pageName", Some(BREAKDOWN_LIMIT)).await?;
    let by_category = group(state, &f, "pageCategory", Some(BREAKDOWN_LIMIT)).await?;
    let by_path = group(state, &f, "path", Some(BREAKDOWN_LIMIT)).await?;
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "by_page_name": to_breakdown(&by_page_name),
            "by_category": to_breakdown(&by_category),
            "by_path": to_breakdown(&by_path),
        },
    }))
}

pub async fn cta(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = base("CTA Clicked", &r.from, &r.to);
    let series = generate_series(&r.from, &r.to, "day");
    let by_cta_name = group(state, &f, "ctaName", Some(BREAKDOWN_LIMIT)).await?;
    let by_location = group(state, &f, "ctaLocation", Some(BREAKDOWN_LIMIT)).await?;
    let by_page = group(state, &f, "pageName", Some(BREAKDOWN_LIMIT)).await?;
    let daily_rows = daily(state, &f).await?;
    let sd = build_time_series(&daily_rows, &series, "day");
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "series": series_json(&sd),
            "by_cta_name": to_breakdown(&by_cta_name),
            "by_location": to_breakdown(&by_location),
            "by_page": to_breakdown(&by_page),
            "summary": { "total": series_total(&sd) },
        },
    }))
}

pub async fn funnel(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let views = base("Page Viewed", &r.from, &r.to);
    let step1 = distinct_count(state, &views).await?;
    let step2 = distinct_count(state, &base("CTA Clicked", &r.from, &r.to)).await?;
    let mut signup = views.clone();
    signup.page_name = Some("Sign Up");
    let step3 = distinct_count(state, &signup).await?;
    let step4 = distinct_count(
        state,
        &register(base("Form Submitted Succeeded", &r.from, &r.to)),
    )
    .await?;
    let pct = |n: i64, b: i64| num(ratio(n, b, 1000.0, 10.0));
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "steps": [
                {
                    "step": 1,
                    "name": "Page Viewed",
                    "description": "Visitor melihat halaman manapun",
                    "count": step1,
                    "conversion_from_start": 100,
                    "conversion_from_prev": 100,
                },
                {
                    "step": 2,
                    "name": "CTA Clicked",
                    "description": "Visitor klik tombol CTA",
                    "count": step2,
                    "conversion_from_start": pct(step2, step1),
                    "conversion_from_prev": pct(step2, step1),
                },
                {
                    "step": 3,
                    "name": "Signup Page Viewed",
                    "description": "Visitor buka halaman Sign Up",
                    "count": step3,
                    "conversion_from_start": pct(step3, step1),
                    "conversion_from_prev": pct(step3, step2),
                },
                {
                    "step": 4,
                    "name": "Registered",
                    "description": "Visitor submit form Register",
                    "count": step4,
                    "conversion_from_start": pct(step4, step1),
                    "conversion_from_prev": pct(step4, step3),
                },
            ],
            "overall_conversion": pct(step4, step1),
        },
    }))
}

pub async fn campaigns(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let views = group(
        state,
        &base("Page Viewed", &r.from, &r.to),
        "utmCampaign",
        Some(BREAKDOWN_LIMIT),
    )
    .await?;
    let clicks = group(
        state,
        &base("CTA Clicked", &r.from, &r.to),
        "utmCampaign",
        None,
    )
    .await?;
    let regs = group(
        state,
        &register(base("Form Submitted Succeeded", &r.from, &r.to)),
        "utmCampaign",
        None,
    )
    .await?;
    // `new Set([...viewMap.keys(), ...clickMap.keys(), ...regMap.keys()])`: urutan kemunculan.
    let mut order: Vec<Option<String>> = Vec::new();
    for (k, _) in views.iter().chain(clicks.iter()).chain(regs.iter()) {
        if !order.contains(k) {
            order.push(k.clone());
        }
    }
    let lookup = |rows: &[(Option<String>, i64)], k: &Option<String>| {
        rows.iter().find(|(l, _)| l == k).map_or(0, |(_, c)| *c)
    };
    let mut rows: Vec<(Option<String>, i64, i64, i64)> = order
        .into_iter()
        .map(|k| {
            let (v, c, g) = (lookup(&views, &k), lookup(&clicks, &k), lookup(&regs, &k));
            (k, v, c, g)
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1)); // stabil, seperti Array.sort
    let campaigns: Vec<Value> = rows
        .into_iter()
        .map(|(k, v, c, g)| {
            json!({
                "campaign": k.unwrap_or_else(|| "(none)".to_owned()),
                "page_views": v,
                "cta_clicks": c,
                "registrations": g,
                "cta_rate": num(ratio(c, v, 1000.0, 10.0)),
                "conversion_rate": num(ratio(g, v, 1000.0, 10.0)),
            })
        })
        .collect();
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": { "campaigns": campaigns },
    }))
}

pub async fn geo(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = base("Page Viewed", &r.from, &r.to);
    let by_country = group(state, &f, "country", Some(BREAKDOWN_LIMIT)).await?;
    let by_city = group(state, &f, "city", Some(BREAKDOWN_LIMIT)).await?;
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "by_country": to_breakdown(&by_country),
            "by_city": to_breakdown(&by_city),
        },
    }))
}

pub async fn devices(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = base("Page Viewed", &r.from, &r.to);
    let by_device = group(state, &f, "deviceType", Some(BREAKDOWN_LIMIT)).await?;
    let by_browser = group(state, &f, "browser", Some(BREAKDOWN_LIMIT)).await?;
    let by_os = group(state, &f, "os", Some(BREAKDOWN_LIMIT)).await?;
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "by_device": to_breakdown(&by_device),
            "by_browser": to_breakdown(&by_browser),
            "by_os": to_breakdown(&by_os),
        },
    }))
}

pub async fn hourly(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let mut f = base("Page Viewed", &r.from, &r.to);
    f.hour_not_null = true;
    let mut qb = QueryBuilder::new(r#"SELECT "hour", count(*) FROM "mixpanelEvent""#);
    push_where(&mut qb, &f);
    qb.push(r#" GROUP BY "hour""#);
    let rows: Vec<(i32, i64)> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut buckets = [0_i64; 24];
    for (h, c) in rows {
        if let Some(slot) = usize::try_from(h).ok().and_then(|i| buckets.get_mut(i)) {
            *slot = c;
        }
    }
    let mut peak: Option<(usize, i64)> = None;
    let series: Vec<Value> = buckets
        .iter()
        .enumerate()
        .map(|(h, v)| {
            if peak.is_none_or(|(_, pv)| *v > pv) {
                peak = Some((h, *v));
            }
            json!({ "hour": h, "label": format!("{h:02}:00"), "value": v })
        })
        .collect();
    let peak_hour = peak
        .and_then(|(h, _)| series.get(h).cloned())
        .unwrap_or(Value::Null);
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": { "series": series, "peak_hour": peak_hour },
    }))
}

pub async fn day_of_week(state: &AppState, range: &Range) -> ApiResult<Value> {
    const DAY_NAMES: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let r = resolve(range);
    let rows = daily(state, &base("Page Viewed", &r.from, &r.to)).await?;
    let mut buckets = [0_i64; 7];
    for (date, c) in rows {
        if let Some(d) = js_ymd(&date)
            && let Some(slot) = buckets.get_mut(js_weekday(d) as usize)
        {
            *slot += c;
        }
    }
    let mut peak: Option<(usize, i64)> = None;
    let series: Vec<Value> = buckets
        .iter()
        .enumerate()
        .map(|(d, v)| {
            if peak.is_none_or(|(_, pv)| *v > pv) {
                peak = Some((d, *v));
            }
            json!({ "day": d, "label": DAY_NAMES.get(d).copied().unwrap_or(""), "value": v })
        })
        .collect();
    let peak_day = peak
        .and_then(|(d, _)| series.get(d).cloned())
        .unwrap_or(Value::Null);
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": { "series": series, "peak_day": peak_day },
    }))
}

pub async fn new_vs_returning(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let in_range = base("Page Viewed", &r.from, &r.to);
    let total = distinct_count(state, &in_range).await?;
    if total == 0 {
        return Ok(json!({
            "success": true,
            "meta": meta(&r.from, &r.to),
            "data": { "new_visitors": 0, "returning_visitors": 0, "total": 0, "returning_rate": 0 },
        }));
    }
    // Pengunjung `date < from` yang `distinctId`-nya ada di `visitorsInRange`.
    let mut qb = QueryBuilder::new(r#"SELECT count(DISTINCT "distinctId") FROM "mixpanelEvent""#);
    let before = Filter {
        event: "Page Viewed",
        before: Some(r.from.clone()),
        ..Filter::default()
    };
    push_where(&mut qb, &before);
    qb.push(r#" AND "distinctId" IN (SELECT "distinctId" FROM "mixpanelEvent""#);
    push_where(&mut qb, &in_range);
    qb.push(")");
    let returning: i64 = qb.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "success": true,
        "meta": meta(&r.from, &r.to),
        "data": {
            "new_visitors": total - returning,
            "returning_visitors": returning,
            "total": total,
            "returning_rate": num(ratio(returning, total, 1000.0, 10.0)),
        },
    }))
}

struct Metrics {
    page_views: i64,
    unique_visitors: i64,
    cta_clicks: i64,
    registrations: i64,
    conversion_rate: f64,
}

async fn build_metrics(state: &AppState, from: &str, to: &str) -> ApiResult<Metrics> {
    let views = base("Page Viewed", from, to);
    let page_views = count(state, &views).await?;
    let unique_visitors = distinct_count(state, &views).await?;
    let cta_clicks = count(state, &base("CTA Clicked", from, to)).await?;
    let registrations = count(state, &register(base("Form Submitted Succeeded", from, to))).await?;
    Ok(Metrics {
        page_views,
        unique_visitors,
        cta_clicks,
        registrations,
        conversion_rate: ratio(registrations, unique_visitors, 10000.0, 100.0),
    })
}

fn metrics_json(m: &Metrics) -> Value {
    json!({
        "page_views": m.page_views,
        "unique_visitors": m.unique_visitors,
        "cta_clicks": m.cta_clicks,
        "registrations": m.registrations,
        "conversion_rate": num(m.conversion_rate),
    })
}

/// `pct(current, previous)`.
fn pct_change(current: f64, previous: f64) -> f64 {
    if previous == 0.0 {
        return if current > 0.0 { 100.0 } else { 0.0 };
    }
    js_round(((current - previous) / previous) * 1000.0) / 10.0
}

pub async fn comparison(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    // `toISOString()` pada tanggal tak valid melempar RangeError → 500 di Node.
    let (Some(from_d), Some(to_d)) = (js_ymd(&r.from), js_ymd(&r.to)) else {
        return Err(ApiError::internal(anyhow::anyhow!("Invalid time value")));
    };
    let duration = to_d - from_d;
    let duration_days = duration.num_days();
    let prev_to = from_d - Duration::days(1);
    let prev_from = prev_to - duration;
    let (prev_from_s, prev_to_s) = (iso(prev_from), iso(prev_to));

    let current = build_metrics(state, &r.from, &r.to).await?;
    let previous = build_metrics(state, &prev_from_s, &prev_to_s).await?;

    #[allow(clippy::cast_precision_loss)]
    let c = |a: i64, b: i64| num(pct_change(a as f64, b as f64));
    Ok(json!({
        "success": true,
        "meta": {
            "current": { "from": r.from, "to": r.to, "days": duration_days + 1 },
            "previous": { "from": prev_from_s, "to": prev_to_s, "days": duration_days + 1 },
        },
        "data": {
            "current": metrics_json(&current),
            "previous": metrics_json(&previous),
            "change": {
                "page_views": c(current.page_views, previous.page_views),
                "unique_visitors": c(current.unique_visitors, previous.unique_visitors),
                "cta_clicks": c(current.cta_clicks, previous.cta_clicks),
                "registrations": c(current.registrations, previous.registrations),
                "conversion_rate": num(pct_change(current.conversion_rate, previous.conversion_rate)),
            },
        },
    }))
}

pub async fn registrations(state: &AppState, range: &Range) -> ApiResult<Value> {
    let r = resolve(range);
    let f = register(base("Form Submitted Succeeded", &r.from, &r.to));
    let series = generate_series(&r.from, &r.to, &r.unit);
    let daily_rows = daily(state, &f).await?;
    let by_source = group(state, &f, "utmSource", Some(BREAKDOWN_LIMIT)).await?;
    let by_campaign = group(state, &f, "utmCampaign", Some(BREAKDOWN_LIMIT)).await?;
    let sd = build_time_series(&daily_rows, &series, &r.unit);
    Ok(json!({
        "success": true,
        "meta": { "from": r.from, "to": r.to, "unit": r.unit },
        "data": {
            "series": series_json(&sd),
            "by_source": to_breakdown(&by_source),
            "by_campaign": to_breakdown(&by_campaign),
            "summary": { "total": series_total(&sd) },
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        js_ymd(s).unwrap_or_default()
    }

    #[test]
    fn js_round_matches_math_round() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(-2.6), -3.0);
        assert_eq!(js_round(0.49999999999999994), 0.0);
    }

    #[test]
    fn ymd_parsing_rolls_over_like_v8() {
        assert_eq!(iso(d("2026-02-30")), "2026-03-02");
        assert!(js_ymd("2026-13-01").is_none());
        assert!(js_ymd("26-01-01").is_none());
        assert!(js_ymd("2026-01-32").is_none());
        assert!(js_ymd("").is_none());
    }

    #[test]
    fn series_day_week_month() {
        assert_eq!(
            generate_series("2026-05-30", "2026-06-02", "day"),
            ["2026-05-30", "2026-05-31", "2026-06-01", "2026-06-02"]
        );
        // 2026-05-31 = Minggu → Senin 2026-05-25.
        assert_eq!(
            generate_series("2026-05-31", "2026-06-09", "week"),
            ["2026-05-25", "2026-06-01", "2026-06-08"]
        );
        assert_eq!(
            generate_series("2026-01-15", "2026-03-02", "month"),
            ["2026-01-01", "2026-02-01", "2026-03-01"]
        );
        // unit tak dikenal → harian
        assert_eq!(generate_series("2026-05-30", "2026-05-31", "x").len(), 2);
        assert!(generate_series("bad", "2026-05-31", "day").is_empty());
    }

    #[test]
    fn bucket_and_timeseries() {
        assert_eq!(bucket_date("2026-05-31", "week"), "2026-05-25");
        assert_eq!(bucket_date("2026-05-31", "month"), "2026-05-01");
        assert_eq!(bucket_date("2026-05-31", "day"), "2026-05-31");
        let series = generate_series("2026-05-25", "2026-06-08", "week");
        let rows = vec![
            ("2026-05-26".to_owned(), 2),
            ("2026-05-31".to_owned(), 3),
            ("2026-06-02".to_owned(), 1),
        ];
        let out = build_time_series(&rows, &series, "week");
        assert_eq!(
            out,
            vec![
                ("2026-05-25".to_owned(), 5),
                ("2026-06-01".to_owned(), 1),
                ("2026-06-08".to_owned(), 0)
            ]
        );
    }

    #[test]
    fn breakdown_percent_and_none_label() {
        let rows = vec![(Some("a".to_owned()), 2), (None, 1)];
        let v = to_breakdown(&rows);
        assert_eq!(
            v,
            json!([
                { "label": "a", "value": 2, "percent": 66.7 },
                { "label": "(none)", "value": 1, "percent": 33.3 },
            ])
        );
        assert_eq!(to_breakdown(&[]), json!([]));
    }

    #[test]
    fn pct_change_rules() {
        assert_eq!(pct_change(0.0, 0.0), 0.0);
        assert_eq!(pct_change(5.0, 0.0), 100.0);
        assert_eq!(pct_change(15.0, 10.0), 50.0);
        assert_eq!(pct_change(5.0, 10.0), -50.0);
        assert_eq!(ratio(1, 3, 10000.0, 100.0), 33.33);
        assert_eq!(ratio(1, 0, 10000.0, 100.0), 0.0);
    }

    #[test]
    fn weekday_is_sunday_zero() {
        assert_eq!(js_weekday(d("2026-05-31")), 0);
        assert_eq!(js_weekday(d("2026-06-01")), 1);
    }
}

#[cfg(test)]
mod iso_tests {
    use super::*;

    #[test]
    fn iso_matches_js_to_iso_string_slice() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap_or_default();
        assert_eq!(iso(d(2026, 11, 17)), "2026-11-17");
        assert_eq!(iso(d(-5947, 9, 1)), "-005947-09");
        assert_eq!(iso(d(10000, 1, 1)), "+010000-01");
    }
}
