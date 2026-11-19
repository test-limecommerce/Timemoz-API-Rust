//! Laporan lembur (specs/004 T092, contracts/overtime-report.md, research R14).
//! Node: `src/overtime/overtime-report.service.ts`.
//!
//! Hanya baca: tanpa transaksi, tanpa cek "enabled" — laporan tetap terbuka saat fitur
//! nonaktif (FR-036). Jumlah query tetap berapa pun jumlah karyawan (SC-007); semua
//! hitungan per hari di memori, O(karyawan x hari).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use axum::http::StatusCode;
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use mz_core::company::require_company_id;
use mz_core::enums::BaseRole;
use mz_core::jsfmt::collate_id;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::json;
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::dto::{DAY_TYPE_VALUES, ReportQuery, STATUS_VALUES};
use super::entity::{
    EmployeeRef, OvertimeReport, Period, ReportDay, ReportEmployee, ReportFilters, ReportMeta,
    WeeklyExcessEmployee, WeeklyExcessMonth, WeeklyExcessReport, WeeklyExcessWeek,
};
use crate::calendar::{DayType, day_info_for, load_batch};
use crate::hr::common::clock::company_clock;
use crate::hr::common::constants::{
    ACTIVE, DEFAULT_RETRO_WINDOW_DAYS, LEGAL_NOTE, WEEKLY_EXCESS_LABEL,
};
use crate::hr::common::eligibility::{
    EmployeeRole, OvertimeEmployeeRow, is_eligible_overtime_employee,
};
use crate::hr::common::enums::OvertimeCancelReason;
use crate::hr::common::errors::overtime_error;
use crate::hr::common::mapper::{date_key_of, live_day_of};
use crate::math::week_range_of;
use crate::worked::worked_sec;
use crate::zone::{date_key, ms_of, parse_date_key, zone_or_utc};

/// Rentang maksimal dihitung INKLUSIF: `from`..`to` paling banyak 92 tanggal, cukup untuk
/// satu kuartal terpanjang (Jul-Sep = 92 hari).
pub const REPORT_MAX_RANGE_DAYS: i64 = 92;

/// Rentang laporan yang sudah dicek.
#[derive(Debug, Clone, Copy)]
pub struct ReportRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

fn invalid_range(message: &str, details: serde_json::Value) -> ApiError {
    overtime_error(
        StatusCode::BAD_REQUEST,
        "OVERTIME_INVALID_RANGE",
        message,
        Some(details),
    )
}

/// Lapis kedua setelah DTO: tanggal valid, `from <= to`, maks 92 tanggal inklusif -> 400.
pub fn assert_report_range(q: &ReportQuery) -> ApiResult<ReportRange> {
    let (Some(from), Some(to)) = (parse_date_key(&q.from), parse_date_key(&q.to)) else {
        return Err(invalid_range(
            "from dan to harus tanggal YYYY-MM-DD yang valid.",
            json!({ "from": q.from, "to": q.to }),
        ));
    };
    if from > to {
        return Err(invalid_range(
            "Tanggal awal tidak boleh setelah tanggal akhir.",
            json!({ "from": q.from, "to": q.to }),
        ));
    }
    let days = (to - from).num_days() + 1;
    if days > REPORT_MAX_RANGE_DAYS {
        return Err(invalid_range(
            &format!("Rentang laporan maksimal {REPORT_MAX_RANGE_DAYS} hari."),
            json!({ "from": q.from, "to": q.to, "maxDays": REPORT_MAX_RANGE_DAYS }),
        ));
    }
    Ok(ReportRange { from, to })
}

/// Filter karyawan (UUID); daftar kosong = tanpa filter.
fn user_ids_of(q: &ReportQuery) -> Option<Vec<Uuid>> {
    let ids: Vec<Uuid> = q
        .user_ids
        .iter()
        .flatten()
        .filter(|id| !id.is_empty())
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect();
    (!ids.is_empty()).then_some(ids)
}

fn known_statuses(q: &ReportQuery) -> Vec<String> {
    q.status
        .iter()
        .flatten()
        .filter(|s| STATUS_VALUES.contains(&s.as_str()))
        .cloned()
        .collect()
}

fn known_day_type(q: &ReportQuery) -> Option<&str> {
    q.day_type
        .as_deref()
        .filter(|d| DAY_TYPE_VALUES.contains(d))
}

const fn day_type_name(t: DayType) -> &'static str {
    match t {
        DayType::Workday => "WORKDAY",
        DayType::Holiday => "HOLIDAY",
    }
}

fn to_minutes(sec: i64) -> i64 {
    sec.div_euclid(60)
}

fn to_minutes_opt(sec: Option<i32>) -> Option<i64> {
    sec.map(|s| to_minutes(i64::from(s)))
}

fn by_name_then_id(a: &EmployeeRef, b: &EmployeeRef) -> Ordering {
    collate_id(&a.name, &b.name).then_with(|| a.id.cmp(&b.id))
}

/// Keterangan baris: alasan tolak, atau label alasan batal.
fn note_of(row: &DailyRow) -> Option<String> {
    match row.status.as_str() {
        "REJECTED" => row.reject_reason.clone().filter(|r| !r.is_empty()),
        "CANCELLED" => row
            .cancel_reason
            .as_deref()
            .and_then(|r| OvertimeCancelReason::from_str(r).ok())
            .map(|r| r.label().to_owned()),
        _ => None,
    }
}

/// Baris final: `finalizedAt` hanya pernah diisi pada APPROVED/COMPLETED, jadi
/// REJECTED/CANCELLED dianggap final juga (menyimpang dari teks kontrak "finalizedAt
/// terisi", supaya baris tertutup tidak membuat `notFinal` menyala selamanya).
fn is_final_row(row: &DailyRow) -> bool {
    row.finalized || matches!(row.status.as_str(), "REJECTED" | "CANCELLED")
}

#[derive(sqlx::FromRow)]
struct DailyRow {
    uuid: String,
    user_id: i32,
    date: NaiveDate,
    status: String,
    day_type: Option<String>,
    requested_sec: i32,
    worked_sec: Option<i32>,
    overtime_sec: Option<i32>,
    exceeds_limit: bool,
    exceeds_daily_limit: bool,
    exceeds_weekly_limit: bool,
    daily_limit_sec: Option<i32>,
    finalized: bool,
    kind: String,
    reject_reason: Option<String>,
    cancel_reason: Option<String>,
    user_uuid: String,
    user_name: String,
    user_username: String,
    requested_by_name: String,
}

/// Rincian harian: SATU query pengajuan dengan `date` di [from, to] (FR-042) beserta nama
/// karyawan. Total hanya dari COMPLETED.
pub async fn daily(
    conn: &mut PgConnection,
    company_id: i32,
    q: &ReportQuery,
    now: NaiveDateTime,
) -> ApiResult<OvertimeReport> {
    let range = assert_report_range(q)?;
    let user_ids = user_ids_of(q);
    let statuses = known_statuses(q);
    let day_type_filter = known_day_type(q);

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"SELECT r.uuid::text AS uuid, r."userId" AS user_id, r.date, r.status::text AS status,
                  r."dayType"::text AS day_type, r."requestedSec" AS requested_sec,
                  r."workedSec" AS worked_sec, r."overtimeSec" AS overtime_sec,
                  r."exceedsLimit" AS exceeds_limit, r."exceedsDailyLimit" AS exceeds_daily_limit,
                  r."exceedsWeeklyLimit" AS exceeds_weekly_limit, r."dailyLimitSec" AS daily_limit_sec,
                  (r."finalizedAt" IS NOT NULL) AS finalized,
                  r.kind::text AS kind, r."rejectReason" AS reject_reason,
                  r."cancelReason"::text AS cancel_reason,
                  u.uuid::text AS user_uuid, u.name AS user_name, u.username AS user_username,
                  rb.name AS requested_by_name
           FROM "overtimeRequest" r
           JOIN users u ON u.id = r."userId"
           JOIN users rb ON rb.id = r."requestedById"
           WHERE r."companyId" = "#,
    );
    qb.push_bind(company_id)
        .push(" AND r.date >= ")
        .push_bind(range.from)
        .push(" AND r.date <= ")
        .push_bind(range.to);
    if let Some(ids) = user_ids {
        qb.push(" AND u.uuid = ANY(").push_bind(ids).push(")");
    }
    if !statuses.is_empty() {
        qb.push(" AND r.status::text = ANY(")
            .push_bind(statuses)
            .push(")");
    }
    // Snapshot cocok ATAU belum ada snapshot (jenis hari live dihitung di bawah, lalu
    // disaring ulang di memori).
    if let Some(t) = day_type_filter {
        qb.push(r#" AND (r."dayType"::text = "#)
            .push_bind(t.to_owned())
            .push(r#" OR r."dayType" IS NULL)"#);
    }
    if let Some(exceeds) = q.exceeds_limit {
        qb.push(r#" AND r."exceedsLimit" = "#).push_bind(exceeds);
    }
    qb.push(r#" ORDER BY r.date, r."createdAt", r.id"#);
    let fetched: Vec<DailyRow> = qb.build_query_as().fetch_all(&mut *conn).await?;

    // Baris tanpa snapshot (belum/tak pernah disetujui) memakai jenis hari live: SATU batch
    // kalender untuk seluruh periode & karyawan yang punya baris tersebut (tanpa N+1).
    let mut live_user_ids: Vec<i32> = fetched
        .iter()
        .filter(|r| r.day_type.is_none())
        .map(|r| r.user_id)
        .collect();
    live_user_ids.sort_unstable();
    live_user_ids.dedup();
    let batch = if live_user_ids.is_empty() {
        None
    } else {
        Some(
            load_batch(conn, company_id, range.from, range.to, &live_user_ids)
                .await
                .map_err(ApiError::internal)?,
        )
    };
    let as_of_ms = ms_of(now);

    let mut kept: Vec<(DailyRow, Option<&'static str>)> = Vec::new();
    for row in fetched {
        let live = match (&row.day_type, &batch) {
            (Some(_), _) | (None, None) => None,
            (None, Some(b)) => {
                let info =
                    day_info_for(b, row.user_id, row.date, as_of_ms).map_err(ApiError::internal)?;
                Some(day_type_name(live_day_of(&info).day_type))
            }
        };
        let day_type: Option<&str> = row.day_type.as_deref().or(live);
        if day_type_filter.is_some() && day_type != day_type_filter {
            continue;
        }
        let name = day_type.and_then(|t| DAY_TYPE_VALUES.iter().find(|v| **v == t).copied());
        kept.push((row, name));
    }

    let mut groups: Vec<(i32, ReportEmployee)> = Vec::new();
    for (row, day_type) in &kept {
        let idx = match groups.iter().position(|(id, _)| *id == row.user_id) {
            Some(i) => i,
            None => {
                groups.push((
                    row.user_id,
                    ReportEmployee {
                        employee: EmployeeRef {
                            id: row.user_uuid.clone(),
                            name: row.user_name.clone(),
                            email: row.user_username.clone(),
                        },
                        total_overtime_minutes: 0,
                        total_overtime_seconds: 0,
                        not_final: false,
                        days: Vec::new(),
                    },
                ));
                groups.len() - 1
            }
        };
        if let Some((_, entity)) = groups.get_mut(idx) {
            if row.status == "COMPLETED" {
                entity.total_overtime_seconds += i64::from(row.overtime_sec.unwrap_or(0));
            }
            entity.days.push(ReportDay {
                request_id: row.uuid.clone(),
                date: date_key_of(row.date),
                day_type: day_type.map(str::to_owned),
                approved_minutes: to_minutes(i64::from(row.requested_sec)),
                worked_minutes: to_minutes_opt(row.worked_sec),
                overtime_minutes: to_minutes_opt(row.overtime_sec),
                status: row.status.clone(),
                kind: row.kind.clone(),
                requested_by_name: row.requested_by_name.clone(),
                note: note_of(row),
                daily_limit_minutes: to_minutes_opt(row.daily_limit_sec),
                exceeds_limit: row.exceeds_limit,
                exceeds_daily_limit: row.exceeds_daily_limit,
                exceeds_weekly_limit: row.exceeds_weekly_limit,
                is_final: is_final_row(row),
            });
        }
    }
    // Urutan baris sudah (date, createdAt, id) dari SQL.
    let mut employees: Vec<ReportEmployee> = groups
        .into_iter()
        .map(|(_, mut e)| {
            e.total_overtime_minutes = to_minutes(e.total_overtime_seconds);
            e.not_final = e.days.iter().any(|d| !d.is_final);
            e
        })
        .collect();
    employees.sort_by(|a, b| by_name_then_id(&a.employee, &b.employee));

    Ok(OvertimeReport {
        period: Period {
            from: date_key(range.from),
            to: date_key(range.to),
        },
        employees,
        legal_note: LEGAL_NOTE.to_owned(),
    })
}

struct WeekSlot {
    start: NaiveDate,
    end: NaiveDate,
    dates: Vec<NaiveDate>,
}

/// Minggu yang hari TERAKHIR-nya di [from, to] (FR-042), urut naik; bisa kosong.
fn weeks_ending_in(range: ReportRange, workweek_start_day: i32) -> ApiResult<Vec<WeekSlot>> {
    let mut weeks = Vec::new();
    let (mut start, mut end) =
        week_range_of(range.from, workweek_start_day).map_err(ApiError::internal)?;
    while end <= range.to {
        let dates = (0..7).map(|i| start + Duration::days(i)).collect();
        weeks.push(WeekSlot { start, end, dates });
        start += Duration::days(7);
        end += Duration::days(7);
    }
    Ok(weeks)
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: i32,
    uuid: String,
    name: String,
    username: String,
    user_status: i32,
    active_membership: bool,
    code: Option<String>,
    is_system: Option<bool>,
    base_role: Option<BaseRole>,
    al_status: Option<i32>,
}

#[derive(sqlx::FromRow)]
struct ApprovedRow {
    user_id: i32,
    date: NaiveDate,
    status: String,
    overtime_sec: Option<i32>,
    finalized: bool,
}

#[derive(Default)]
struct WeekAcc {
    overtime_sec: i64,
    unfinalized: bool,
}

/// Melebihi jam kerja mingguan (FR-040-FR-043): per minggu company yang berakhir di
/// [from, to], `excess = max(0, worked - normal - overtime COMPLETED)`. Hanya filter
/// `userIds` yang berlaku. Minggu tak saling mengimbangi (FR-041).
///
/// Karyawan = anggota aktif saat ini, ditambah anggota yang keanggotaannya berubah (mis.
/// dinonaktifkan) sejak awal minggu pertama — mereka tetap memuat minggu kelebihannya di
/// periode itu. Yang sudah tak aktif tidak diuji role-nya (penugasan sudah dicabut).
pub async fn weekly_excess(
    conn: &mut PgConnection,
    company_id: i32,
    q: &ReportQuery,
    now: NaiveDateTime,
) -> ApiResult<WeeklyExcessReport> {
    let range = assert_report_range(q)?;
    let user_ids = user_ids_of(q);
    let empty = WeeklyExcessReport {
        months: Vec::new(),
        label: WEEKLY_EXCESS_LABEL.to_owned(),
    };

    let clock = company_clock(&mut *conn, company_id, now).await?;
    let weeks = weeks_ending_in(range, clock.workweek_start_day)?;
    // Rentang penutup: awal minggu pertama s.d. akhir minggu terakhir.
    let (Some(first), Some(last)) = (weeks.first(), weeks.last()) else {
        return Ok(empty);
    };
    let (cover_from, cover_to) = (first.start, last.end);

    let mut mq: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"SELECT u.id, u.uuid::text AS uuid, u.name, u.username, u.status AS user_status,
                  EXISTS (SELECT 1 FROM "userCompany" uc
                          WHERE uc."userId" = u.id AND uc."companyId" = "#,
    );
    mq.push_bind(company_id)
        .push(r#" AND uc.status = "#)
        .push_bind(ACTIVE)
        .push(
            r#") AS active_membership,
                  a.code, a.is_system, a.base_role, a.al_status
           FROM users u
           LEFT JOIN LATERAL (
             SELECT al.code, al."isSystem" AS is_system, al."baseRole"::text AS base_role,
                    al.status AS al_status
             FROM "userAccessLevel" ual
             LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = u.id AND ual."companyId" = "#,
        )
        .push_bind(company_id)
        .push(" AND ual.status = ")
        .push_bind(ACTIVE)
        .push(
            r#"
             ORDER BY ual.id ASC
             LIMIT 1
           ) a ON true
           WHERE ((u.status = "#,
        )
        .push_bind(ACTIVE)
        .push(
            r#" AND EXISTS (SELECT 1 FROM "userCompany" uc
                              WHERE uc."userId" = u.id AND uc."companyId" = "#,
        )
        .push_bind(company_id)
        .push(" AND uc.status = ")
        .push_bind(ACTIVE)
        // Keluar/dinonaktifkan di tengah atau sesudah periode (FR-041).
        .push(
            r#"))
              OR EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = "#,
        )
        .push_bind(company_id)
        .push(r#" AND uc."updatedAt" >= "#)
        .push_bind(cover_from.and_time(chrono::NaiveTime::MIN))
        .push("))");
    if let Some(ids) = &user_ids {
        mq.push(" AND u.uuid = ANY(")
            .push_bind(ids.clone())
            .push(")");
    }
    mq.push(" ORDER BY u.id");
    let member_rows: Vec<MemberRow> = mq.build_query_as().fetch_all(&mut *conn).await?;

    let retro_window_days: Option<i32> = sqlx::query_scalar(
        r#"SELECT "retroWindowDays" FROM "companyOvertimeSetting" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;

    // APPROVED ikut dibaca hanya untuk tanda notFinal; overtime hanya dari COMPLETED.
    let mut rq: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"SELECT r."userId" AS user_id, r.date, r.status::text AS status,
                  r."overtimeSec" AS overtime_sec, (r."finalizedAt" IS NOT NULL) AS finalized
           FROM "overtimeRequest" r
           WHERE r."companyId" = "#,
    );
    rq.push_bind(company_id)
        .push(" AND r.date >= ")
        .push_bind(cover_from)
        .push(" AND r.date <= ")
        .push_bind(cover_to)
        .push(" AND r.status::text IN ('APPROVED', 'COMPLETED')");
    if let Some(ids) = &user_ids {
        rq.push(" AND r.\"userId\" IN (SELECT id FROM users WHERE uuid = ANY(")
            .push_bind(ids.clone())
            .push("))");
    }
    let requests: Vec<ApprovedRow> = rq.build_query_as().fetch_all(&mut *conn).await?;

    let members: Vec<MemberRow> = member_rows
        .into_iter()
        .filter(|m| {
            let still_member = m.user_status == ACTIVE && m.active_membership;
            if !still_member {
                return true;
            }
            let role = match (&m.code, m.is_system, m.al_status) {
                (Some(code), Some(is_system), Some(status)) => Some(EmployeeRole {
                    code: code.clone(),
                    is_system,
                    base_role: m.base_role,
                    status,
                }),
                _ => None,
            };
            let row = OvertimeEmployeeRow {
                id: m.id,
                uuid: m.uuid.clone(),
                name: m.name.clone(),
                username: m.username.clone(),
                role,
            };
            is_eligible_overtime_employee(Some(&row))
        })
        .collect();
    if members.is_empty() {
        return Ok(empty);
    }
    let member_ids: Vec<i32> = members.iter().map(|m| m.id).collect();

    let batch = load_batch(conn, company_id, cover_from, cover_to, &member_ids)
        .await
        .map_err(ApiError::internal)?;
    let worked = worked_sec(
        conn,
        company_id,
        zone_or_utc(Some(&clock.time_zone)),
        &member_ids,
        cover_from,
        cover_to,
    )
    .await
    .map_err(ApiError::internal)?;

    let mut week_index_by_date: HashMap<NaiveDate, usize> = HashMap::new();
    for (i, w) in weeks.iter().enumerate() {
        for d in &w.dates {
            week_index_by_date.insert(*d, i);
        }
    }

    let mut acc_by_user: HashMap<i32, HashMap<usize, WeekAcc>> = HashMap::new();
    for r in &requests {
        let Some(index) = week_index_by_date.get(&r.date) else {
            continue;
        };
        let acc = acc_by_user
            .entry(r.user_id)
            .or_default()
            .entry(*index)
            .or_default();
        if r.status == "COMPLETED" {
            acc.overtime_sec += i64::from(r.overtime_sec.unwrap_or(0));
        }
        // APPROVED/COMPLETED yang belum final: angka minggu ini masih bisa berubah.
        if !r.finalized {
            acc.unfinalized = true;
        }
    }

    // FR-043: jam tracking/manual masih bisa berubah selama masa retro sesudah weekEnd.
    let retro = i64::from(retro_window_days.unwrap_or(DEFAULT_RETRO_WINDOW_DAYS));
    let open_week_ends: HashSet<NaiveDate> = weeks
        .iter()
        .filter(|w| clock.local_today <= w.end + Duration::days(retro))
        .map(|w| w.end)
        .collect();

    let as_of_ms = ms_of(now);
    let mut months: BTreeMap<String, Vec<WeeklyExcessEmployee>> = BTreeMap::new();
    for member in &members {
        let worked_by_date = worked.get(&member.id);
        let per_week = acc_by_user.get(&member.id);
        let mut listed: BTreeMap<String, (WeeklyExcessEmployee, i64)> = BTreeMap::new();

        for (index, week) in weeks.iter().enumerate() {
            let mut worked_sec_total: i64 = 0;
            let mut normal_sec_total: i64 = 0;
            for date in &week.dates {
                worked_sec_total += worked_by_date
                    .and_then(|m| m.get(date))
                    .copied()
                    .unwrap_or(0);
                let info =
                    day_info_for(&batch, member.id, *date, as_of_ms).map_err(ApiError::internal)?;
                normal_sec_total += crate::hr::common::mapper::normal_sec_live(&info);
            }
            let acc = per_week.and_then(|m| m.get(&index));
            let overtime_sec = acc.map_or(0, |a| a.overtime_sec);
            let excess_sec = (worked_sec_total - normal_sec_total - overtime_sec).max(0);
            if excess_sec <= 0 {
                continue;
            }

            let month = date_key(week.end).chars().take(7).collect::<String>();
            let entry = listed.entry(month).or_insert_with(|| {
                (
                    WeeklyExcessEmployee {
                        employee: EmployeeRef {
                            id: member.uuid.clone(),
                            name: member.name.clone(),
                            email: member.username.clone(),
                        },
                        total_excess_minutes: 0,
                        weeks: Vec::new(),
                    },
                    0,
                )
            });
            entry.0.weeks.push(WeeklyExcessWeek {
                week_start: date_key(week.start),
                week_end: date_key(week.end),
                worked_minutes: to_minutes(worked_sec_total),
                normal_minutes: to_minutes(normal_sec_total),
                overtime_minutes: to_minutes(overtime_sec),
                excess_minutes: to_minutes(excess_sec),
                not_final: acc.is_some_and(|a| a.unfinalized) || open_week_ends.contains(&week.end),
            });
            entry.1 += excess_sec;
        }

        for (month, (mut entry, excess_sec)) in listed {
            entry.total_excess_minutes = to_minutes(excess_sec);
            months.entry(month).or_default().push(entry);
        }
    }

    let result = months
        .into_iter()
        .map(|(month, mut employees)| {
            employees.sort_by(|a, b| by_name_then_id(&a.employee, &b.employee));
            WeeklyExcessMonth { month, employees }
        })
        .collect();
    Ok(WeeklyExcessReport {
        months: result,
        label: WEEKLY_EXCESS_LABEL.to_owned(),
    })
}

fn now_naive() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// `GET /v1/overtime/reports` untuk pemanggil `user_id`.
pub async fn report(state: &AppState, user_id: i32, q: &ReportQuery) -> ApiResult<OvertimeReport> {
    assert_report_range(q)?;
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    daily(&mut conn, company_id, q, now_naive()).await
}

/// `GET /v1/overtime/reports/weekly-excess` untuk pemanggil `user_id`.
pub async fn weekly(
    state: &AppState,
    user_id: i32,
    q: &ReportQuery,
) -> ApiResult<WeeklyExcessReport> {
    assert_report_range(q)?;
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    weekly_excess(&mut conn, company_id, q, now_naive()).await
}

/// Data ekspor: company diselesaikan SEKALI lalu dipakai kedua bagian.
pub async fn export_data(
    state: &AppState,
    user_id: i32,
    q: &ReportQuery,
) -> ApiResult<(OvertimeReport, WeeklyExcessReport, ReportMeta)> {
    assert_report_range(q)?;
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let now = now_naive();
    let daily_report = daily(&mut conn, company_id, q, now).await?;
    let weekly_report = weekly_excess(&mut conn, company_id, q, now).await?;
    let company: Option<(String, String, i32)> =
        sqlx::query_as(r#"SELECT name, "timeZone", "workweekStartDay" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    let generated_by: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?;
    let (company_name, time_zone, workweek_start_day) =
        company.unwrap_or_else(|| (String::new(), "UTC".to_owned(), 1));
    let meta = ReportMeta {
        company_name,
        time_zone,
        generated_by: generated_by.unwrap_or_default(),
        generated_at: Utc::now(),
        workweek_start_day,
        filters: ReportFilters {
            status: known_statuses(q),
            day_type: known_day_type(q).map(str::to_owned),
            exceeds_limit: q.exceeds_limit,
            employee_count: q
                .user_ids
                .as_ref()
                .map_or(0, |ids| ids.iter().filter(|id| !id.is_empty()).count()),
        },
    };
    Ok((daily_report, weekly_report, meta))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn q(from: &str, to: &str) -> ReportQuery {
        ReportQuery {
            from: from.into(),
            to: to.into(),
            ..ReportQuery::default()
        }
    }

    fn code_of(e: ApiError) -> (u16, String, String) {
        match e {
            ApiError::Body { status, body } => (
                status.as_u16(),
                body["code"].as_str().unwrap_or_default().to_owned(),
                body["message"].as_str().unwrap_or_default().to_owned(),
            ),
            other => panic!("bukan body galat: {other:?}"),
        }
    }

    #[test]
    fn range_accepts_inclusive_92_days() {
        assert!(assert_report_range(&q("2026-07-01", "2026-09-30")).is_ok());
        assert!(assert_report_range(&q("2026-10-01", "2026-10-01")).is_ok());
    }

    #[test]
    fn range_rejects_reversed_and_too_long() {
        let (status, code, msg) =
            code_of(assert_report_range(&q("2026-10-31", "2026-10-01")).unwrap_err());
        assert_eq!((status, code.as_str()), (400, "OVERTIME_INVALID_RANGE"));
        assert_eq!(msg, "Tanggal awal tidak boleh setelah tanggal akhir.");
        let (_, _, msg) = code_of(assert_report_range(&q("2026-01-01", "2026-12-31")).unwrap_err());
        assert_eq!(msg, "Rentang laporan maksimal 92 hari.");
        // 93 tanggal inklusif.
        assert!(assert_report_range(&q("2026-07-01", "2026-10-01")).is_err());
    }

    #[test]
    fn weeks_end_inside_range() {
        // Okt 2026: Senin 28 Sep .. Minggu 4 Okt, dst. Periode 1-31 Okt memuat minggu yang
        // berakhir 4, 11, 18, 25 Okt (minggu berakhir 1 Nov di luar).
        let range = ReportRange {
            from: parse_date_key("2026-10-01").unwrap(),
            to: parse_date_key("2026-10-31").unwrap(),
        };
        let weeks = weeks_ending_in(range, 1).unwrap();
        let ends: Vec<String> = weeks.iter().map(|w| date_key(w.end)).collect();
        assert_eq!(
            ends,
            ["2026-10-04", "2026-10-11", "2026-10-18", "2026-10-25"]
        );
        assert_eq!(date_key(weeks[0].start), "2026-09-28");
        assert_eq!(weeks[0].dates.len(), 7);
        // Periode lebih pendek dari satu minggu: kosong.
        let short = ReportRange {
            from: parse_date_key("2026-10-05").unwrap(),
            to: parse_date_key("2026-10-07").unwrap(),
        };
        assert!(weeks_ending_in(short, 1).unwrap().is_empty());
    }

    #[test]
    fn name_ordering_is_case_insensitive_then_id() {
        let e = |name: &str, id: &str| EmployeeRef {
            id: id.into(),
            name: name.into(),
            email: String::new(),
        };
        assert_eq!(
            by_name_then_id(&e("ani", "1"), &e("Budi", "2")),
            Ordering::Less
        );
        assert_eq!(
            by_name_then_id(&e("Ani", "2"), &e("Ani", "1")),
            Ordering::Greater
        );
        assert_eq!(
            by_name_then_id(&e("ani", "1"), &e("Ani", "1")),
            Ordering::Less
        );
    }

    #[test]
    fn minutes_floor() {
        assert_eq!(to_minutes(119), 1);
        assert_eq!(to_minutes(0), 0);
        assert_eq!(to_minutes_opt(None), None);
        assert_eq!(to_minutes_opt(Some(7200)), Some(120));
    }
}
