//! `leave-policies` bagian saldo: `GET balances`, `POST balances/initialize[/:userId]`,
//! `PATCH balances/:userId/adjust`, `GET balances/:userId/adjustments`
//! (`leave-policies.service.ts` `getCompanyBalances`, `bulkInitializeBalances`,
//! `initializeBalance`, `adjustBalance`, `getBalanceAdjustments`).

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{Datelike, NaiveDate};
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::jsfmt;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::balance::{
    NewAdjustment, create_balance, find_balance, insert_adjustment, lock_balance,
};
use mz_leave::enums::{HOLIDAY_DEDUCTION_APPLIED, LeaveBalanceChangeSource};
use mz_leave::js::{ser_f64, ser_opt_f64};
use mz_leave::models::{BALANCE_COLUMNS, LeaveBalance};
use mz_leave::policy::{load_entitlement_inputs, load_policy, require_time_zone};
use mz_leave::view::{BalanceView, BalanceViewCtx, load_scheduled_holidays, to_balance_view_with};
use serde::Serialize;
use sqlx::{FromRow, Postgres, QueryBuilder};

use crate::common::{UserMini, company_of};
use crate::dto::{AdjustLeaveBalanceDto, BalancesQuery, YearQuery};
use crate::validate::{node_500, parse_id};

/// `new Date().getFullYear()` (produksi `TZ=UTC`).
fn current_year() -> i32 {
    now_naive().year()
}

/// Saldo + `user` (`include: { user: { select: { id, name, username } } }`).
#[derive(Debug, Clone, Serialize)]
pub struct BalanceWithUser {
    #[serde(flatten)]
    pub balance: LeaveBalance,
    pub user: UserMini,
}

#[derive(FromRow)]
struct BalanceFlat {
    #[sqlx(flatten)]
    balance: LeaveBalance,
    #[sqlx(rename = "uName")]
    name: String,
    #[sqlx(rename = "uUsername")]
    username: String,
}

impl BalanceFlat {
    fn into_view(self) -> BalanceWithUser {
        let user = UserMini {
            id: self.balance.user_id,
            name: self.name,
            username: self.username,
        };
        BalanceWithUser {
            balance: self.balance,
            user,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub total: i64,
    pub page: i32,
    pub limit: i32,
    pub total_pages: i64,
}

#[derive(Debug, Serialize)]
pub struct BalancesPage {
    pub data: Vec<BalanceView<BalanceWithUser>>,
    pub meta: Meta,
}

/// `GET /leave-policies/balances`. Node: `leave-policies.controller.ts:652` → `getCompanyBalances`.
pub async fn get_balances(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<BalancesQuery>,
) -> ApiResult<Json<BalancesPage>> {
    let params = query.typed()?;
    let year = params.year.unwrap_or_else(current_year);
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(20);
    let company_id = company_of(&state, &ctx).await?;
    // `skip` dan `take` adalah Int Prisma: di luar Int32 → galat (500).
    let skip = i32::try_from((i64::from(page) - 1) * i64::from(limit))
        .map_err(|_| node_500("skip di luar Int32"))?;

    let pattern = params
        .search
        .as_deref()
        .filter(|s| !s.is_empty())
        // `contains` + `mode: 'insensitive'` = `ILIKE '%…%'` tanpa escape (sama dengan Prisma).
        .map(|s| format!("%{s}%"));
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(
            r#" FROM "leaveBalance" b JOIN users u ON u.id = b."userId" WHERE b."companyId" = "#,
        )
        .push_bind(company_id)
        .push(" AND b.year = ")
        .push_bind(year)
        // Baris saldo bisa milik user yang sudah keluar / pindah company; yang tampil
        // hanya anggota aktif company ini.
        .push(r#" AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = b."userId" AND uc."companyId" = "#)
        .push_bind(company_id)
        .push(" AND uc.status = 1)");
        if let Some(p) = &pattern {
            qb.push(" AND (u.name ILIKE ")
                .push_bind(p.clone())
                .push(" OR u.username ILIKE ")
                .push_bind(p.clone())
                .push(")");
        }
    };

    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    filter(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT {BALANCE_COLUMNS}, u.name AS "uName", u.username AS "uUsername""#
    ));
    filter(&mut qb);
    qb.push(" ORDER BY u.name ASC, b.id ASC LIMIT ")
        .push_bind(i64::from(limit))
        .push(" OFFSET ")
        .push_bind(i64::from(skip));
    let rows: Vec<BalanceFlat> = qb.build_query_as().fetch_all(&state.db).await?;

    // Satu kali baca per halaman untuk policy, override, tier: bukan N+1.
    let mut conn = state.db.acquire().await?;
    let user_ids: Vec<i32> = rows.iter().map(|r| r.balance.user_id).collect();
    let policy = load_policy(&mut *conn, company_id).await?;
    let inputs = load_entitlement_inputs(&mut conn, company_id, &user_ids).await?;
    let time_zone = require_time_zone(&mut *conn, company_id).await?;
    let now = now_naive();

    // Daftar libur terjadwal dihitung sekali per halaman; yang sudah diterapkan dibaca dengan
    // satu query untuk semua user di halaman ini.
    let scheduled = load_scheduled_holidays(&mut conn, company_id, year, &time_zone, now).await?;
    let mut applied_by_user: HashMap<i32, HashSet<i32>> = HashMap::new();
    if !scheduled.is_empty() && !user_ids.is_empty() {
        let holiday_ids: Vec<i32> = scheduled.iter().map(|h| h.holiday_id).collect();
        let applied: Vec<(i32, i32)> = sqlx::query_as(
            r#"SELECT "userId", "holidayId" FROM "holidayLeaveDeduction"
               WHERE "companyId" = $1 AND status = $2 AND "userId" = ANY($3)
                 AND "holidayId" = ANY($4) AND "holidayId" IS NOT NULL"#,
        )
        .bind(company_id)
        .bind(HOLIDAY_DEDUCTION_APPLIED)
        .bind(&user_ids)
        .bind(&holiday_ids)
        .fetch_all(&mut *conn)
        .await?;
        for (user_id, holiday_id) in applied {
            applied_by_user
                .entry(user_id)
                .or_default()
                .insert(holiday_id);
        }
    }

    let data = rows
        .into_iter()
        .map(|row| {
            let pending = match applied_by_user.get(&row.balance.user_id) {
                Some(done) => scheduled
                    .iter()
                    .filter(|h| !done.contains(&h.holiday_id))
                    .count(),
                None => scheduled.len(),
            };
            let balance = row.balance.clone();
            let ctx = BalanceViewCtx {
                policy: policy.as_ref(),
                override_days: inputs.override_of(balance.user_id),
                tier_days: inputs.tier_days_of(balance.user_id),
                time_zone: &time_zone,
                scheduled_holiday_days: pending,
                now,
            };
            to_balance_view_with(row.into_view(), &balance, &ctx)
        })
        .collect();

    Ok(Json(BalancesPage {
        data,
        meta: Meta {
            total,
            page,
            limit,
            total_pages: (total + i64::from(limit) - 1) / i64::from(limit),
        },
    }))
}

/// Hasil `bulkInitializeBalances`.
#[derive(Debug, Serialize)]
pub struct BulkResult {
    pub initialized: u32,
    pub skipped: u32,
}

/// `POST /leave-policies/balances/initialize`. Node: `leave-policies.controller.ts:709` →
/// `bulkInitializeBalances`. Tiap karyawan dibuat di transaksinya sendiri (seperti Node).
pub async fn bulk_initialize(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<YearQuery>,
) -> ApiResult<impl IntoResponse> {
    let year = query.year()?.unwrap_or_else(current_year);
    let company_id = company_of(&state, &ctx).await?;
    let members: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1 ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let mut result = BulkResult {
        initialized: 0,
        skipped: 0,
    };
    for user_id in members {
        if find_balance(&state.db, user_id, company_id, year)
            .await?
            .is_some()
        {
            result.skipped += 1;
            continue;
        }
        // `create_balance` membaca ulang baris yang dibuat bersamaan (P2002), bukan galat.
        create_balance(&state.db, user_id, company_id, year).await?;
        result.initialized += 1;
    }
    Ok((StatusCode::CREATED, Json(result)))
}

/// `POST /leave-policies/balances/initialize/:userId`. Node: `leave-policies.controller.ts:1043` →
/// `initializeBalance`.
pub async fn initialize_balance(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
    ValidatedQuery(query): ValidatedQuery<YearQuery>,
) -> ApiResult<impl IntoResponse> {
    let year = query.year()?.unwrap_or_else(current_year);
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    // Tanpa cek ini admin bisa membuat saldo di company-nya untuk user company lain.
    let member: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    if member.is_none() {
        return Err(ApiError::not_found("User not found in this company"));
    }
    if find_balance(&state.db, user_id, company_id, year)
        .await?
        .is_some()
    {
        return Err(ApiError::bad_request(
            "Balance already initialized for this year",
        ));
    }
    let created = create_balance(&state.db, user_id, company_id, year).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /leave-policies/balances/:userId/adjust`. Node: `leave-policies.controller.ts:773` →
/// `adjustBalance`. Jatah/total diubah + baris riwayat `ADJUSTMENT` dalam satu transaksi.
pub async fn adjust_balance(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
    ValidatedJson(dto): ValidatedJson<AdjustLeaveBalanceDto>,
) -> ApiResult<Json<BalanceWithUser>> {
    let input = dto.typed()?;
    let year = input.year.unwrap_or_else(current_year);
    let requester_id = ctx.require_user_id()?;
    let company_id = company_of(&state, &ctx).await?;
    let target = parse_id(&user_id)?;

    let mut tx = state.db.begin().await?;
    let Some(found) = find_balance(&mut *tx, target, company_id, year).await? else {
        return Err(ApiError::not_found(
            "Saldo cuti karyawan untuk tahun ini belum diinisialisasi",
        ));
    };
    // DEVIASI (lihat plan/deviations/mz-leave-config.md): Node membaca lalu menulis nilai absolut
    // tanpa kunci; di sini baris dikunci dan dibaca ulang supaya potongan/akrual yang berjalan
    // bersamaan tidak tertimpa.
    let balance = lock_balance(&mut tx, found.id).await?;
    let (current, used) = match input.field {
        "annualLeaveTotal" => (balance.annual_leave_total, balance.annual_leave_used),
        "sickLeaveTotal" => (balance.sick_leave_total, balance.sick_leave_used),
        _ => (balance.carry_over_days, balance.carry_over_used),
    };
    let new_value = current + input.delta;
    if new_value < 0.0 {
        return Err(ApiError::bad_request(format!(
            "Nilai {} tidak boleh negatif setelah adjustment",
            input.field
        )));
    }
    if new_value < used {
        return Err(ApiError::bad_request(format!(
            "Tidak dapat mengurangi {} di bawah jumlah yang sudah terpakai ({} hari)",
            input.field,
            jsfmt::num_string(used)
        )));
    }

    // Nama kolom berasal dari daftar tetap di atas (bukan input mentah).
    let updated: LeaveBalance = sqlx::query_as(&format!(
        r#"UPDATE "leaveBalance" AS b SET "{}" = $2, "updatedAt" = $3 WHERE b.id = $1
           RETURNING {BALANCE_COLUMNS}"#,
        input.field
    ))
    .bind(balance.id)
    .bind(new_value)
    .bind(now_naive())
    .fetch_one(&mut *tx)
    .await?;
    insert_adjustment(
        &mut tx,
        &NewAdjustment {
            balance_id: balance.id,
            user_id: target,
            company_id,
            adjusted_by_id: Some(requester_id),
            field: input.field,
            delta: input.delta,
            reason: input.reason,
            source: LeaveBalanceChangeSource::ADJUSTMENT,
            leave_request_id: None,
            dedupe_key: None,
            holiday_id: None,
            intended_delta: None,
        },
    )
    .await?;
    let user: UserMini = sqlx::query_as("SELECT id, name, username FROM users WHERE id = $1")
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(BalanceWithUser {
        balance: updated,
        user,
    }))
}

// ── riwayat ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HolidayMini {
    pub id: i32,
    pub name: String,
    pub date: JsDate,
}

#[derive(Debug, Serialize)]
pub struct LeaveTypeConfigMini {
    pub name: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveRequestMini {
    pub id: i32,
    pub uuid: String,
    pub start_date: JsDate,
    pub end_date: JsDate,
    #[serde(serialize_with = "ser_f64")]
    pub total_days: f64,
    pub leave_type: Option<String>,
    pub leave_type_config: Option<LeaveTypeConfigMini>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustmentView {
    pub id: i32,
    pub balance_id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub adjusted_by_id: Option<i32>,
    pub field: String,
    #[serde(serialize_with = "ser_f64")]
    pub delta: f64,
    pub reason: String,
    pub source: String,
    pub leave_request_id: Option<i32>,
    pub dedupe_key: Option<String>,
    pub holiday_id: Option<i32>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub intended_delta: Option<f64>,
    pub created_at: JsDate,
    pub adjusted_by: Option<UserMini>,
    pub holiday: Option<HolidayMini>,
    pub leave_request: Option<LeaveRequestMini>,
}

#[derive(FromRow)]
struct AdjustmentFlat {
    id: i32,
    #[sqlx(rename = "balanceId")]
    balance_id: i32,
    #[sqlx(rename = "userId")]
    user_id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "adjustedById")]
    adjusted_by_id: Option<i32>,
    field: String,
    delta: f64,
    reason: String,
    source: String,
    #[sqlx(rename = "leaveRequestId")]
    leave_request_id: Option<i32>,
    #[sqlx(rename = "dedupeKey")]
    dedupe_key: Option<String>,
    #[sqlx(rename = "holidayId")]
    holiday_id: Option<i32>,
    #[sqlx(rename = "intendedDelta")]
    intended_delta: Option<f64>,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    ab_name: Option<String>,
    ab_username: Option<String>,
    h_name: Option<String>,
    h_date: Option<NaiveDate>,
    lr_uuid: Option<String>,
    lr_start: Option<NaiveDate>,
    lr_end: Option<NaiveDate>,
    lr_total: Option<f64>,
    lr_type: Option<String>,
    lt_name: Option<String>,
    lt_code: Option<String>,
}

/// Kolom `@db.Date` dikirim Node sebagai `Date` JS → ISO penuh tengah malam UTC.
fn js_date(d: NaiveDate) -> JsDate {
    JsDate(d.and_hms_opt(0, 0, 0).unwrap_or_default())
}

impl AdjustmentFlat {
    fn into_view(self) -> AdjustmentView {
        let adjusted_by = match (self.adjusted_by_id, self.ab_name, self.ab_username) {
            (Some(id), Some(name), Some(username)) => Some(UserMini { id, name, username }),
            _ => None,
        };
        let holiday = match (self.holiday_id, self.h_name, self.h_date) {
            (Some(id), Some(name), Some(date)) => Some(HolidayMini {
                id,
                name,
                date: js_date(date),
            }),
            _ => None,
        };
        let leave_request = match (
            self.leave_request_id,
            self.lr_uuid,
            self.lr_start,
            self.lr_end,
            self.lr_total,
        ) {
            (Some(id), Some(uuid), Some(start), Some(end), Some(total)) => Some(LeaveRequestMini {
                id,
                uuid,
                start_date: js_date(start),
                end_date: js_date(end),
                total_days: total,
                leave_type: self.lr_type,
                leave_type_config: match (self.lt_name, self.lt_code) {
                    (Some(name), Some(code)) => Some(LeaveTypeConfigMini { name, code }),
                    _ => None,
                },
            }),
            _ => None,
        };
        AdjustmentView {
            id: self.id,
            balance_id: self.balance_id,
            user_id: self.user_id,
            company_id: self.company_id,
            adjusted_by_id: self.adjusted_by_id,
            field: self.field,
            delta: self.delta,
            reason: self.reason,
            source: self.source,
            leave_request_id: self.leave_request_id,
            dedupe_key: self.dedupe_key,
            holiday_id: self.holiday_id,
            intended_delta: self.intended_delta,
            created_at: self.created_at,
            adjusted_by,
            holiday,
            leave_request,
        }
    }
}

/// `GET /leave-policies/balances/:userId/adjustments`. Node: `leave-policies.controller.ts:1005` →
/// `getBalanceAdjustments`.
pub async fn get_adjustments(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
    ValidatedQuery(query): ValidatedQuery<YearQuery>,
) -> ApiResult<Json<Vec<AdjustmentView>>> {
    let year = query.year()?;
    let company_id = company_of(&state, &ctx).await?;
    let target = parse_id(&user_id)?;
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT a.id, a."balanceId", a."userId", a."companyId", a."adjustedById", a.field, a.delta,
                  a.reason, a.source::text AS source, a."leaveRequestId", a."dedupeKey",
                  a."holidayId", a."intendedDelta", a."createdAt",
                  ab.name AS ab_name, ab.username AS ab_username,
                  h.name AS h_name, h.date AS h_date,
                  lr.uuid::text AS lr_uuid, lr."startDate" AS lr_start, lr."endDate" AS lr_end,
                  lr."totalDays" AS lr_total, lr."leaveType"::text AS lr_type,
                  lt.name AS lt_name, lt.code AS lt_code
           FROM "leaveBalanceAdjustment" a
           LEFT JOIN users ab ON ab.id = a."adjustedById"
           LEFT JOIN "companyHoliday" h ON h.id = a."holidayId"
           LEFT JOIN "leaveRequest" lr ON lr.id = a."leaveRequestId"
           LEFT JOIN "companyLeaveType" lt ON lt.id = lr."leaveTypeId""#,
    );
    // `if (year)`: tahun 0 tidak memfilter.
    if let Some(y) = year.filter(|y| *y != 0) {
        qb.push(r#" JOIN "leaveBalance" b ON b.id = a."balanceId" AND b.year = "#)
            .push_bind(y);
    }
    qb.push(r#" WHERE a."userId" = "#)
        .push_bind(target)
        .push(r#" AND a."companyId" = "#)
        .push_bind(company_id)
        .push(r#" ORDER BY a."createdAt" DESC, a.id DESC"#);
    let rows: Vec<AdjustmentFlat> = qb.build_query_as().fetch_all(&state.db).await?;
    Ok(Json(
        rows.into_iter().map(AdjustmentFlat::into_view).collect(),
    ))
}
