//! Port `src/overtime/calendar/work-pattern.service.ts` bagian yang dipakai `PATCH /companies/:uuid`
//! dan `GET /companies/:uuid`: `view`, `validate`, `schedule`, `cancelScheduled`, `realignScheduled`.
//! Penulisan `companyWorkPattern` lewat `AuditHandle` (Node: delegate Prisma → middleware audit).

use axum::http::StatusCode;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_overtime::calendar::{
    NORMAL_SEC_FIVE_DAY, NORMAL_SEC_SHORT_DAY, NORMAL_SEC_SIX_DAY, is_candidate, pattern_of,
    pick_winner, resolve_pattern_with_from,
};
use mz_overtime::hr::common::clock::load_pattern_rows;
use mz_overtime::math::next_week_start;
use mz_overtime::zone::local_date_key;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

const SEC_PER_HOUR: i64 = 3600;

/// Pola ternormalisasi (`DayInfoPattern`) — tipe bersama `mz_overtime::calendar::DayPattern`.
pub use mz_overtime::calendar::DayPattern as Pattern;

/// Hasil `validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternOp {
    Set(Pattern),
    Cancel,
}

/// `LockedCompanyClock`.
#[derive(Debug, Clone)]
pub struct Clock {
    pub id: i32,
    pub work_week_start_day: i32,
    pub time_zone: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub work_days: i32,
    pub off_days: Vec<i32>,
    pub short_day: Option<i32>,
    pub effective_from: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scheduled {
    pub work_days: i32,
    pub off_days: Vec<i32>,
    pub short_day: Option<i32>,
    pub effective_from: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalHours {
    pub workday: i64,
    pub short_day: Option<i64>,
    pub weekly: i64,
}

/// Blok `workPattern` pada `GET /companies/:uuid`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkPatternView {
    pub current: Resolved,
    pub scheduled: Option<Scheduled>,
    pub next_effective_date: String,
    pub normal_hours: NormalHours,
}

/// `overtimeError(422, 'WORK_PATTERN_INVALID', message, { field })`.
fn invalid(field: &str, message: &str) -> ApiError {
    let mut body = Map::new();
    body.insert("code".into(), json!("WORK_PATTERN_INVALID"));
    body.insert("message".into(), json!(message));
    body.insert("details".into(), json!({ "field": field }));
    ApiError::body(StatusCode::UNPROCESSABLE_ENTITY, Value::Object(body))
}

fn iso_day(v: &Value) -> Option<i32> {
    let n = v.as_f64()?;
    (n.fract() == 0.0 && (1.0..=7.0).contains(&n)).then_some(n as i32)
}

/// Aturan lintas-field R2 (`validatePattern`).
fn validate_pattern(input: &Value) -> ApiResult<Pattern> {
    let work_days = input.get("workDays").and_then(Value::as_f64);
    let work_days = match work_days {
        Some(5.0) => 5,
        Some(6.0) => 6,
        _ => return Err(invalid("workDays", "Jumlah hari kerja harus 5 atau 6.")),
    };
    let off_msg = "Hari libur harus berupa angka 1 (Senin) sampai 7 (Minggu).";
    let off_days: Vec<i32> = match input.get("offDays").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .map(iso_day)
            .collect::<Option<Vec<i32>>>()
            .ok_or_else(|| invalid("offDays", off_msg))?,
        None => return Err(invalid("offDays", off_msg)),
    };
    let mut uniq = off_days.clone();
    uniq.sort_unstable();
    uniq.dedup();
    if uniq.len() != off_days.len() {
        return Err(invalid("offDays", "Hari libur tidak boleh berulang."));
    }
    let expected = if work_days == 5 { 2 } else { 1 };
    if off_days.len() != expected {
        return Err(invalid(
            "offDays",
            &format!("Pola {work_days} hari kerja harus punya tepat {expected} hari libur."),
        ));
    }

    let short_raw = input.get("shortDay").filter(|v| !v.is_null());
    let short_day = if work_days == 5 {
        if short_raw.is_some() {
            return Err(invalid(
                "shortDay",
                "Pola 5 hari kerja tidak memakai hari pendek.",
            ));
        }
        None
    } else {
        let Some(raw) = short_raw else {
            return Err(invalid(
                "shortDay",
                "Pola 6 hari kerja wajib menentukan hari pendek.",
            ));
        };
        let Some(day) = iso_day(raw) else {
            return Err(invalid(
                "shortDay",
                "Hari pendek harus berupa angka 1 (Senin) sampai 7 (Minggu).",
            ));
        };
        if off_days.contains(&day) {
            return Err(invalid(
                "shortDay",
                "Hari pendek tidak boleh jatuh pada hari libur.",
            ));
        }
        Some(day)
    };
    Ok(Pattern {
        work_days,
        off_days: uniq,
        short_day,
    })
}

/// `validate(input)`: `None` = tidak ada operasi pola. Keduanya terisi (bukan null/undefined) → `conflict`.
pub fn validate(
    work_pattern: Option<&Value>,
    cancel_scheduled: Option<&Value>,
) -> ApiResult<Option<PatternOp>> {
    let pattern = work_pattern.filter(|v| !v.is_null());
    let cancel = cancel_scheduled.filter(|v| !v.is_null());
    if pattern.is_some() && cancel.is_some() {
        return Err(invalid(
            "conflict",
            "Tidak bisa menyimpan pola kerja baru sekaligus membatalkan pola terjadwal.",
        ));
    }
    if let Some(p) = pattern {
        return validate_pattern(p).map(|p| Some(PatternOp::Set(p)));
    }
    Ok((cancel == Some(&Value::Bool(true))).then_some(PatternOp::Cancel))
}

fn normal_hours_of(p: &Pattern) -> NormalHours {
    if p.work_days == 6 {
        let workday = NORMAL_SEC_SIX_DAY / SEC_PER_HOUR;
        let short_day = NORMAL_SEC_SHORT_DAY / SEC_PER_HOUR;
        NormalHours {
            workday,
            short_day: Some(short_day),
            weekly: workday * i64::from(p.work_days - 1) + short_day,
        }
    } else {
        let workday = NORMAL_SEC_FIVE_DAY / SEC_PER_HOUR;
        NormalHours {
            workday,
            short_day: None,
            weekly: workday * i64::from(p.work_days),
        }
    }
}

fn ms_of(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

fn nws(local_today: NaiveDate, week_start: i32) -> ApiResult<NaiveDate> {
    next_week_start(local_today, week_start).map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))
}

/// Blok `workPattern` (`view`). Tanpa lock: tampilan saja.
pub async fn view(state: &AppState, company_id: i32) -> ApiResult<WorkPatternView> {
    let company: Option<(i32, String)> =
        sqlx::query_as(r#"SELECT "workweekStartDay", "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((week_start, time_zone)) = company else {
        return Err(ApiError::not_found("Company not found"));
    };
    let now = now_naive();
    let now_ms = ms_of(now);
    let today = local_date_key(now_ms, Some(&time_zone));
    // Tanpa batas atas: versi terjadwal (> hari ini) juga dibutuhkan.
    let mut conn = state.db.acquire().await?;
    let far = NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or(NaiveDate::MAX);
    let rows = load_pattern_rows(&mut conn, company_id, far).await?;

    let (current, from) = resolve_pattern_with_from(&rows, today, now_ms);
    let next = pick_winner(
        rows.iter()
            .filter(|r| r.effective_from > today && is_candidate(r, now_ms)),
    );
    let normal_hours = normal_hours_of(&current);
    Ok(WorkPatternView {
        current: Resolved {
            work_days: current.work_days,
            off_days: current.off_days,
            short_day: current.short_day,
            effective_from: from.map(|d| d.to_string()),
        },
        scheduled: next.map(|r| {
            let p = pattern_of(r);
            Scheduled {
                work_days: p.work_days,
                off_days: p.off_days,
                short_day: p.short_day,
                effective_from: r.effective_from.to_string(),
            }
        }),
        next_effective_date: nws(today, week_start)?.to_string(),
        normal_hours,
    })
}

/// `schedule` / `cancelScheduled` (R2 langkah 1–3). Pemanggil WAJIB sudah memegang
/// `SELECT … FOR UPDATE` atas baris `company` di transaksi yang sama (T7).
pub async fn schedule(
    audit: &AuditHandle,
    tx: &mut PgConnection,
    company: &Clock,
    input: &PatternOp,
    actor_id: Option<i32>,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let local_today = local_date_key(ms_of(now), company.time_zone.as_deref());
    let effective_from = nws(local_today, company.work_week_start_day)?;

    // Langkah 2: versi yang belum berlaku boleh digantikan; versi berlaku tak pernah disentuh.
    let company_id = company.id;
    let pre = audit
        .pre_read_many(
            Model::CompanyWorkPattern,
            Lookup::filter(move |qb| {
                qb.push(r#""companyId" = "#).push_bind(company_id);
                qb.push(r#" AND "supersededAt" IS NULL AND "effectiveFrom" > "#)
                    .push_bind(local_today);
            }),
        )
        .await;
    let affected = sqlx::query(
        r#"UPDATE "companyWorkPattern" SET "supersededAt" = $2
           WHERE "companyId" = $1 AND "supersededAt" IS NULL AND "effectiveFrom" > $3"#,
    )
    .bind(company_id)
    .bind(now)
    .bind(local_today)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::CompanyWorkPattern,
            pre,
            affected,
            &Row::new().with("supersededAt", json!(JsDate(now))),
            json!({
                "companyId": company_id,
                "supersededAt": null,
                "effectiveFrom": { "gt": format!("{local_today}T00:00:00.000Z") }
            }),
        )
        .await;

    let PatternOp::Set(pattern) = input else {
        return Ok(());
    };

    // Langkah 3: sama dengan pola yang berlaku sehari sebelumnya → berhenti (= cancel).
    let day_before = effective_from - Duration::days(1);
    let rows = load_pattern_rows(&mut *tx, company_id, day_before).await?;
    // `LATEST_STATE`: hanya baris yang belum digantikan, apa pun createdAt-nya.
    let (before, _) = resolve_pattern_with_from(&rows, day_before, i64::MAX);
    if before == *pattern {
        return Ok(());
    }

    // createdAt = now (bukan default DB) agar persis bertemu supersededAt versi lama untuk asOf.
    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "companyWorkPattern"
             ("companyId", "workDays", "offDays", "shortDay", "effectiveFrom", "createdAt", "createdById")
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
    )
    .bind(company_id)
    .bind(i16::try_from(pattern.work_days).unwrap_or(5))
    .bind(&pattern.off_days)
    .bind(pattern.short_day)
    .bind(effective_from)
    .bind(now)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?;
    // Tanpa `select`: middleware audit butuh `companyId` dari hasil create.
    if let Some(row) = audit
        .result_row(&mut *tx, Model::CompanyWorkPattern, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::CompanyWorkPattern, &row).await;
    }
    Ok(())
}

/// `realignScheduled`: setelah `workweekStartDay` berubah, pindahkan versi terjadwal ke
/// `nextWeekStart` yang baru. Tanpa jadwal, atau jadwal sudah di tanggal itu → tidak menulis.
pub async fn realign_scheduled(
    audit: &AuditHandle,
    tx: &mut PgConnection,
    company: &Clock,
    actor_id: Option<i32>,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let local_today = local_date_key(ms_of(now), company.time_zone.as_deref());
    let far = NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or(NaiveDate::MAX);
    let all = load_pattern_rows(&mut *tx, company.id, far).await?;
    let pending = pick_winner(
        all.iter()
            .filter(|r| r.superseded_at_ms.is_none() && r.effective_from > local_today),
    );
    let Some(scheduled) = pending else {
        return Ok(());
    };
    if scheduled.effective_from == nws(local_today, company.work_week_start_day)? {
        return Ok(());
    }
    let op = PatternOp::Set(pattern_of(scheduled));
    schedule(audit, tx, company, &op, actor_id, now).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(j: Value) -> Option<Value> {
        Some(j)
    }

    fn field_of(err: &ApiError) -> String {
        match err {
            ApiError::Body { body, .. } => body
                .pointer("/details/field")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => String::new(),
        }
    }

    #[test]
    fn validate_matches_node_rules() {
        let ok = validate(v(json!({"workDays":5,"offDays":[7,6]})).as_ref(), None);
        assert_eq!(
            ok.ok().flatten(),
            Some(PatternOp::Set(Pattern {
                work_days: 5,
                off_days: vec![6, 7],
                short_day: None
            }))
        );
        let six = validate(
            v(json!({"workDays":6,"offDays":[7],"shortDay":6})).as_ref(),
            None,
        );
        assert!(matches!(six, Ok(Some(PatternOp::Set(_)))));
        assert_eq!(
            validate(None, v(json!(true)).as_ref()).ok().flatten(),
            Some(PatternOp::Cancel)
        );
        assert_eq!(
            validate(None, v(json!(false)).as_ref()).ok().flatten(),
            None
        );

        let cases = [
            (json!({"workDays":4,"offDays":[7]}), "workDays"),
            (json!({"workDays":5,"offDays":[7]}), "offDays"),
            (json!({"workDays":5,"offDays":[7,7]}), "offDays"),
            (
                json!({"workDays":5,"offDays":[6,7],"shortDay":1}),
                "shortDay",
            ),
            (json!({"workDays":6,"offDays":[7]}), "shortDay"),
            (json!({"workDays":6,"offDays":[7],"shortDay":7}), "shortDay"),
            (json!({"workDays":6,"offDays":[7],"shortDay":9}), "shortDay"),
        ];
        for (input, field) in cases {
            match validate(Some(&input), None) {
                Err(e) => assert_eq!(field_of(&e), field, "{input}"),
                Ok(_) => panic!("harus 422: {input}"),
            }
        }
        let both = validate(
            v(json!({"workDays":5,"offDays":[6,7]})).as_ref(),
            v(json!(true)).as_ref(),
        );
        assert!(matches!(both, Err(ref e) if field_of(e) == "conflict"));
        // null dianggap tidak dikirim.
        assert_eq!(
            validate(Some(&Value::Null), Some(&Value::Null))
                .ok()
                .flatten(),
            None
        );
    }

    #[test]
    fn normal_hours() {
        let five = normal_hours_of(&Pattern::default_pattern());
        assert_eq!((five.workday, five.short_day, five.weekly), (8, None, 40));
        let six = normal_hours_of(&Pattern {
            work_days: 6,
            off_days: vec![7],
            short_day: Some(6),
        });
        assert_eq!((six.workday, six.short_day, six.weekly), (7, Some(5), 40));
    }
}
