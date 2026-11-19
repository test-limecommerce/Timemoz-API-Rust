//! `src/leave-types/**`: 7 route `/leave-types*` (`leave-types.controller.ts` +
//! `leave-types.service.ts`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::enums::LeaveBalanceSource;
use mz_leave::policy::apply_sick_leave_from_leave_type;
use serde::Serialize;
use sqlx::{FromRow, PgExecutor, Postgres, QueryBuilder};

use crate::common::{SUCCESS, Success, company_of};
use crate::dto::{CreateLeaveTypeDto, LeaveTypeInput, LeaveTypeQuery, UpdateLeaveTypeDto};
use crate::validate::{Tri, node_500, parse_id};

/// Model `companyLeaveType` (semua kolom skalar, urutan `schema.prisma`).
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct LeaveTypeRow {
    pub id: i32,
    pub company_id: i32,
    pub name: String,
    pub code: String,
    pub deduct_balance: bool,
    pub balance_source: LeaveBalanceSource,
    pub requires_document: bool,
    pub annual_quota: Option<i32>,
    pub is_active: bool,
    pub is_default: bool,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `companyLeaveType` (enum dibaca `::text`); juga dipakai `RETURNING`.
pub const TYPE_COLUMNS: &str = r#"id, "companyId", name, code, "deductBalance",
    "balanceSource"::text AS "balanceSource", "requiresDocument", "annualQuota", "isActive",
    "isDefault", "createdAt", "updatedAt""#;

/// Satu entri `DEFAULT_LEAVE_TYPES` (`leave-types.service.ts:12`); semua `isDefault: true`.
struct DefaultType {
    code: &'static str,
    name: &'static str,
    deduct_balance: bool,
    balance_source: &'static str,
    requires_document: bool,
    annual_quota: Option<i32>,
}

const fn default_type(
    code: &'static str,
    name: &'static str,
    deduct_balance: bool,
    balance_source: &'static str,
    requires_document: bool,
    annual_quota: Option<i32>,
) -> DefaultType {
    DefaultType {
        code,
        name,
        deduct_balance,
        balance_source,
        requires_document,
        annual_quota,
    }
}

const DEFAULT_LEAVE_TYPES: [DefaultType; 6] = [
    default_type("ANNUAL_LEAVE", "Annual Leave", true, "ANNUAL", false, None),
    default_type("SICK_LEAVE", "Sick Leave", true, "SICK", true, None),
    default_type(
        "MARRIAGE_LEAVE",
        "Marriage Leave",
        false,
        "ANNUAL",
        true,
        Some(3),
    ),
    default_type(
        "MATERNITY_LEAVE",
        "Maternity Leave",
        false,
        "ANNUAL",
        true,
        None,
    ),
    default_type(
        "BEREAVEMENT_LEAVE",
        "Bereavement Leave",
        false,
        "ANNUAL",
        false,
        None,
    ),
    default_type("UNPAID_LEAVE", "Unpaid Leave", false, "ANNUAL", false, None),
];

/// `toCode(name)`: `toUpperCase`, runtun non `[A-Z0-9]` → `_`, `_` di tepi dibuang.
pub fn to_code(name: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for c in name.to_uppercase().chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            // Pemisah di awal (`^_+`) dan akhir (`_+$`) dibuang; yang di tengah jadi satu `_`.
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(c);
        } else {
            pending_sep = true;
        }
    }
    out
}

fn not_found() -> ApiError {
    ApiError::not_found("Jenis cuti tidak ditemukan")
}

async fn fetch(
    exec: impl PgExecutor<'_>,
    id: i32,
    company_id: i32,
) -> sqlx::Result<Option<LeaveTypeRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {TYPE_COLUMNS} FROM "companyLeaveType" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(id)
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

async fn code_taken(exec: impl PgExecutor<'_>, company_id: i32, code: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "companyLeaveType" WHERE "companyId" = $1 AND code = $2)"#,
    )
    .bind(company_id)
    .bind(code)
    .fetch_one(exec)
    .await
}

/// `GET /leave-types`. Node: `leave-types.controller.ts:84` → `service.list`.
pub async fn list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<LeaveTypeQuery>,
) -> ApiResult<Json<Vec<LeaveTypeRow>>> {
    let company_id = company_of(&state, &ctx).await?;
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT {TYPE_COLUMNS} FROM "companyLeaveType" WHERE "companyId" = "#
    ));
    qb.push_bind(company_id);
    if let Some(active) = query.filter() {
        qb.push(r#" AND "isActive" = "#).push_bind(active);
    }
    qb.push(r#" ORDER BY "isDefault" DESC, name ASC, id ASC"#);
    Ok(Json(qb.build_query_as().fetch_all(&state.db).await?))
}

/// `GET /leave-types/:id`. Node: `leave-types.controller.ts:215` → `service.findOne`.
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<LeaveTypeRow>> {
    find_one_inner(&state, &ctx, &id).await
}

pub async fn find_one_inner(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
) -> ApiResult<Json<LeaveTypeRow>> {
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    fetch(&state.db, id, company_id)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

/// `POST /leave-types`. Node: `leave-types.controller.ts:134` → `service.create`.
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateLeaveTypeDto>,
) -> ApiResult<impl IntoResponse> {
    let input = dto.0.typed()?;
    let company_id = company_of(&state, &ctx).await?;
    let Tri::Val(name) = input.name else {
        return Err(node_500("name"));
    };
    // `dto.code ?? this.toCode(dto.name)`.
    let code = input.code.val().cloned().unwrap_or_else(|| to_code(&name));
    if code_taken(&state.db, company_id, &code).await? {
        return Err(ApiError::bad_request(format!(
            "Kode '{code}' sudah digunakan"
        )));
    }
    let row: LeaveTypeRow = sqlx::query_as(&format!(
        r#"INSERT INTO "companyLeaveType"
             ("companyId", name, code, "deductBalance", "balanceSource", "requiresDocument",
              "annualQuota", "isActive", "isDefault")
           VALUES ($1, $2, $3, $4, $5::"LeaveBalanceSource", $6, $7, $8, false)
           RETURNING {TYPE_COLUMNS}"#
    ))
    .bind(company_id)
    .bind(&name)
    .bind(&code)
    .bind(input.deduct_balance.val().copied().unwrap_or(true))
    .bind(
        input
            .balance_source
            .val()
            .copied()
            .unwrap_or(LeaveBalanceSource::ANNUAL)
            .as_str(),
    )
    .bind(input.requires_document.val().copied().unwrap_or(false))
    .bind(input.annual_quota.val().copied())
    .bind(input.is_active.val().copied().unwrap_or(true))
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `POST /leave-types/seed` (idempotent). Node: `leave-types.controller.ts:174` → `seedDefaults`.
pub async fn seed(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<impl IntoResponse> {
    let company_id = company_of(&state, &ctx).await?;
    // `createMany({ data, skipDuplicates: true })` = satu INSERT ... ON CONFLICT DO NOTHING.
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO "companyLeaveType"
             ("companyId", code, name, "deductBalance", "balanceSource", "requiresDocument",
              "annualQuota", "isDefault") "#,
    );
    qb.push_values(DEFAULT_LEAVE_TYPES, |mut b, t| {
        b.push_bind(company_id)
            .push_bind(t.code)
            .push_bind(t.name)
            .push_bind(t.deduct_balance)
            .push_bind(t.balance_source)
            .push_unseparated(r#"::"LeaveBalanceSource""#)
            .push_bind(t.requires_document)
            .push_bind(t.annual_quota)
            .push("true");
    });
    qb.push(r#" ON CONFLICT ("companyId", code) DO NOTHING"#);
    qb.build().execute(&state.db).await?;
    let rows: Vec<LeaveTypeRow> = sqlx::query_as(&format!(
        r#"SELECT {TYPE_COLUMNS} FROM "companyLeaveType"
           WHERE "companyId" = $1 AND "isDefault" = true ORDER BY name ASC, id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(rows)))
}

/// `PUT /leave-types/:id`. Node: `leave-types.controller.ts:251` → `service.update`.
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedJson(dto): ValidatedJson<UpdateLeaveTypeDto>,
) -> ApiResult<Json<LeaveTypeRow>> {
    update_inner(&state, &ctx, &id, dto).await
}

pub async fn update_inner(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
    dto: UpdateLeaveTypeDto,
) -> ApiResult<Json<LeaveTypeRow>> {
    let input = dto.0.typed()?;
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    let Some(leave_type) = fetch(&state.db, id, company_id).await? else {
        return Err(not_found());
    };
    if let Some(code) = input.code.val()
        && !code.is_empty()
        && *code != leave_type.code
        && code_taken(&state.db, company_id, code).await?
    {
        return Err(ApiError::bad_request(format!(
            "Kode '{code}' sudah digunakan"
        )));
    }
    Ok(Json(apply_update(state, company_id, id, &input).await?))
}

/// Transaksi `update` + `applySickLeaveFromLeaveType`.
async fn apply_update(
    state: &AppState,
    company_id: i32,
    id: i32,
    input: &LeaveTypeInput,
) -> ApiResult<LeaveTypeRow> {
    // Kolom non-null yang diisi `null` = galat argumen Prisma (500).
    if matches!(input.name, Tri::Null)
        || matches!(input.code, Tri::Null)
        || matches!(input.deduct_balance, Tri::Null)
        || matches!(input.balance_source, Tri::Null)
        || matches!(input.requires_document, Tri::Null)
        || matches!(input.is_active, Tri::Null)
    {
        return Err(node_500("companyLeaveType.update: null pada kolom wajib"));
    }
    let touches_sick_quota = input.annual_quota.defined()
        || input.balance_source.defined()
        || input.deduct_balance.defined()
        || input.is_active.defined();
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "companyLeaveType" SET "#);
    {
        let mut set = qb.separated(", ");
        if let Some(name) = input.name.val() {
            set.push("name = ").push_bind_unseparated(name.clone());
        }
        if let Some(code) = input.code.val() {
            set.push("code = ").push_bind_unseparated(code.clone());
        }
        if let Some(v) = input.deduct_balance.val() {
            set.push(r#""deductBalance" = "#).push_bind_unseparated(*v);
        }
        if let Some(v) = input.balance_source.val() {
            set.push(r#""balanceSource" = "#)
                .push_bind_unseparated(v.as_str())
                .push_unseparated(r#"::"LeaveBalanceSource""#);
        }
        if let Some(v) = input.requires_document.val() {
            set.push(r#""requiresDocument" = "#)
                .push_bind_unseparated(*v);
        }
        // `annualQuota: null` sah (kolom nullable).
        if let Some(quota) = input.annual_quota.clone().value_or_null() {
            set.push(r#""annualQuota" = "#).push_bind_unseparated(quota);
        }
        if let Some(v) = input.is_active.val() {
            set.push(r#""isActive" = "#).push_bind_unseparated(*v);
        }
        set.push(r#""updatedAt" = "#).push_bind_unseparated(now);
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(format!(" RETURNING {TYPE_COLUMNS}"));
    let updated: LeaveTypeRow = qb.build_query_as().fetch_one(&mut *tx).await?;
    if touches_sick_quota
        && updated.balance_source == LeaveBalanceSource::SICK
        && updated.deduct_balance
        && updated.is_active
    {
        apply_sick_leave_from_leave_type(&mut tx, company_id, updated.annual_quota, now).await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// `DELETE /leave-types/:id`. Node: `leave-types.controller.ts:290` → `service.remove`.
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Success>> {
    remove_inner(&state, &ctx, &id).await
}

pub async fn remove_inner(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
) -> ApiResult<Json<Success>> {
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    if fetch(&state.db, id, company_id).await?.is_none() {
        return Err(not_found());
    }
    let used: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "leaveRequest" WHERE "leaveTypeId" = $1"#)
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    if used > 0 {
        return Err(ApiError::bad_request(format!(
            "Tidak bisa dihapus: {used} pengajuan cuti menggunakan jenis ini. Nonaktifkan saja."
        )));
    }
    sqlx::query(r#"DELETE FROM "companyLeaveType" WHERE id = $1"#)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(SUCCESS))
}

/// `PATCH /leave-types/:id/toggle`. Node: `leave-types.controller.ts:320` → `toggleActive`.
pub async fn toggle(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<LeaveTypeRow>> {
    let company_id = company_of(&state, &ctx).await?;
    let id = parse_id(&id)?;
    let Some(current) = fetch(&state.db, id, company_id).await? else {
        return Err(not_found());
    };
    let row: LeaveTypeRow = sqlx::query_as(&format!(
        r#"UPDATE "companyLeaveType" SET "isActive" = $2, "updatedAt" = $3 WHERE id = $1
           RETURNING {TYPE_COLUMNS}"#
    ))
    .bind(id)
    .bind(!current.is_active)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_generation() {
        assert_eq!(to_code("Cuti Khusus"), "CUTI_KHUSUS");
        assert_eq!(to_code("  a--b  "), "A_B");
        assert_eq!(to_code("Cuti 2026!"), "CUTI_2026");
        assert_eq!(to_code("---"), "");
        assert_eq!(to_code(""), "");
        assert_eq!(to_code("straße"), "STRASSE");
        assert_eq!(to_code("Ünïcode"), "N_CODE");
    }
}
