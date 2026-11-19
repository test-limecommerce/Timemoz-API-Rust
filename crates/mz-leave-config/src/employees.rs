//! `leave-policies` bagian karyawan: override jatah (`/employees*`), tier (`/tiers*`), dan
//! penugasan tier (`PUT tiers/:tierId/assign/:userId`, `GET/DELETE tiers/employees/:userId`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mz_core::extract::ValidatedJson;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::models::{LeaveTier, TIER_COLUMNS};
use mz_leave::policy::load_policy;
use serde::Serialize;
use serde_json::json;
use sqlx::{FromRow, PgExecutor, PgPool};

use crate::common::{SUCCESS, Success, UserMini, company_of, null_body};
use crate::dto::{CreateLeaveTierDto, UpdateEmployeeLeavePolicyDto};
use crate::validate::{Tri, db_id, parse_id, parse_int_pipe};

// ── override jatah karyawan ──────────────────────────────────────────────────

/// Model `employeeLeavePolicy`.
#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct EmployeePolicyRow {
    pub id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub annual_leave_days: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

const EMPLOYEE_POLICY_COLUMNS: &str =
    r#"e.id, e."userId", e."companyId", e."annualLeaveDays", e."createdAt", e."updatedAt""#;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmployeePolicyWithUser {
    #[serde(flatten)]
    pub row: EmployeePolicyRow,
    pub user: UserMini,
}

#[derive(FromRow)]
struct EmployeePolicyFlat {
    #[sqlx(flatten)]
    row: EmployeePolicyRow,
    #[sqlx(rename = "uName")]
    name: String,
    #[sqlx(rename = "uUsername")]
    username: String,
}

impl EmployeePolicyFlat {
    fn into_view(self) -> EmployeePolicyWithUser {
        let user = UserMini {
            id: self.row.user_id,
            name: self.name,
            username: self.username,
        };
        EmployeePolicyWithUser {
            row: self.row,
            user,
        }
    }
}

/// `GET /leave-policies/employees`. Node: `leave-policies.controller.ts:337` → `listAllEmployeePolicies`.
pub async fn list_employee_policies(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Vec<EmployeePolicyWithUser>>> {
    let company_id = company_of(&state, &ctx).await?;
    let rows: Vec<EmployeePolicyFlat> = sqlx::query_as(&format!(
        r#"SELECT {EMPLOYEE_POLICY_COLUMNS}, u.name AS "uName", u.username AS "uUsername"
           FROM "employeeLeavePolicy" e JOIN users u ON u.id = e."userId"
           WHERE e."companyId" = $1 ORDER BY u.name ASC, e.id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(EmployeePolicyFlat::into_view)
            .collect(),
    ))
}

/// `{ ...override, isOverride: true }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverrideView {
    #[serde(flatten)]
    inner: EmployeePolicyWithUser,
    is_override: bool,
}

/// `GET /leave-policies/employees/:userId`. Node: `leave-policies.controller.ts:371` → `getEmployeePolicy`.
pub async fn get_employee_policy(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
) -> ApiResult<Response> {
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    let found: Option<EmployeePolicyFlat> = sqlx::query_as(&format!(
        r#"SELECT {EMPLOYEE_POLICY_COLUMNS}, u.name AS "uName", u.username AS "uUsername"
           FROM "employeeLeavePolicy" e JOIN users u ON u.id = e."userId"
           WHERE e."userId" = $1 AND e."companyId" = $2"#
    ))
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some(flat) = found {
        return Ok(Json(OverrideView {
            inner: flat.into_view(),
            is_override: true,
        })
        .into_response());
    }
    let policy = load_policy(&state.db, company_id).await?;
    Ok(Json(json!({
        "userId": user_id,
        "companyId": company_id,
        "annualLeaveDays": policy.map_or(12, |p| p.annual_leave_days),
        "isOverride": false,
    }))
    .into_response())
}

async fn member_exists(
    exec: impl PgExecutor<'_>,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM users u WHERE u.id = $1 AND EXISTS (
             SELECT 1 FROM "userCompany" uc
             WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1))"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_one(exec)
    .await
}

/// `PUT /leave-policies/employees/:userId`. Node: `leave-policies.controller.ts:406` → `upsertEmployeePolicy`.
/// Tidak menyentuh saldo (override baru dibaca saat saldo tahun berikutnya dibuat).
pub async fn upsert_employee_policy(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
    ValidatedJson(dto): ValidatedJson<UpdateEmployeeLeavePolicyDto>,
) -> ApiResult<Json<EmployeePolicyRow>> {
    let days = dto.annual_leave_days()?;
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    if !member_exists(&state.db, user_id, company_id).await? {
        return Err(ApiError::not_found("User not found in this company"));
    }
    let row: EmployeePolicyRow = sqlx::query_as(&format!(
        r#"INSERT INTO "employeeLeavePolicy" AS e ("userId", "companyId", "annualLeaveDays")
           VALUES ($1, $2, $3)
           ON CONFLICT ("userId", "companyId")
             DO UPDATE SET "annualLeaveDays" = EXCLUDED."annualLeaveDays", "updatedAt" = $4
           RETURNING {EMPLOYEE_POLICY_COLUMNS}"#
    ))
    .bind(user_id)
    .bind(company_id)
    .bind(days)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /leave-policies/employees/:userId`. Node: `leave-policies.controller.ts:434` → `deleteEmployeePolicy`.
pub async fn delete_employee_policy(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Success>> {
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    let done = sqlx::query(
        r#"DELETE FROM "employeeLeavePolicy" WHERE "userId" = $1 AND "companyId" = $2"#,
    )
    .bind(user_id)
    .bind(company_id)
    .execute(&state.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found(
            "Employee leave policy override not found",
        ));
    }
    Ok(Json(SUCCESS))
}

// ── tier ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TierCount {
    #[serde(rename = "employeeTiers")]
    pub employee_tiers: i64,
}

/// `leaveTier` + `_count: { employeeTiers }`.
#[derive(Debug, Serialize)]
pub struct TierWithCount {
    #[serde(flatten)]
    pub tier: LeaveTier,
    #[serde(rename = "_count")]
    pub count: TierCount,
}

#[derive(FromRow)]
struct TierCountRow {
    #[sqlx(flatten)]
    tier: LeaveTier,
    #[sqlx(rename = "employeeTiers")]
    employee_tiers: i64,
}

impl TierCountRow {
    fn into_view(self) -> TierWithCount {
        TierWithCount {
            tier: self.tier,
            count: TierCount {
                employee_tiers: self.employee_tiers,
            },
        }
    }
}

const TIER_WITH_COUNT: &str =
    r#"(SELECT COUNT(*) FROM "employeeLeaveTier" e WHERE e."tierId" = t.id) AS "employeeTiers""#;

/// `GET /leave-policies/tiers`. Node: `leave-policies.controller.ts:462` → `listTiers`.
pub async fn list_tiers(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Vec<TierWithCount>>> {
    let company_id = company_of(&state, &ctx).await?;
    let rows: Vec<TierCountRow> = sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS}, {TIER_WITH_COUNT} FROM "leaveTier" t
           WHERE t."companyId" = $1 ORDER BY t."minTenureMonths" ASC, t.id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter().map(TierCountRow::into_view).collect(),
    ))
}

async fn find_tier(
    exec: impl PgExecutor<'_>,
    tier_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<LeaveTier>> {
    sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS} FROM "leaveTier" t WHERE t.id = $1 AND t."companyId" = $2 LIMIT 1"#
    ))
    .bind(tier_id)
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

/// `GET /leave-policies/tiers/:tierId` (`ParseIntPipe`). Node: `leave-policies.controller.ts:528` → `getTier`.
pub async fn get_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(tier_id): Path<String>,
) -> ApiResult<Json<TierWithCount>> {
    // Pipe berjalan sebelum handler (sebelum company).
    let tier_id = parse_int_pipe(&tier_id)?;
    let company_id = company_of(&state, &ctx).await?;
    let tier_id = db_id(tier_id)?;
    let row: Option<TierCountRow> = sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS}, {TIER_WITH_COUNT} FROM "leaveTier" t
           WHERE t.id = $1 AND t."companyId" = $2 LIMIT 1"#
    ))
    .bind(tier_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.map(|r| Json(r.into_view())).ok_or_else(|| {
        ApiError::coded(
            StatusCode::NOT_FOUND,
            "LEAVE_TIER_NOT_FOUND",
            "Leave tier not found",
        )
    })
}

const MAX_MIN_MSG: &str = "maxTenureMonths must be greater than minTenureMonths";

/// `POST /leave-policies/tiers`. Node: `leave-policies.controller.ts:480` → `createTier`.
/// `leavePolicy` dipastikan ada DULU (di luar transaksi, seperti Node), baru validasi tenure.
pub async fn create_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateLeaveTierDto>,
) -> ApiResult<impl IntoResponse> {
    let input = dto.typed()?;
    let company_id = company_of(&state, &ctx).await?;
    // `leavePolicy.upsert({ create: { companyId }, update: {} })`.
    let policy_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "leavePolicy" ("companyId") VALUES ($1)
           ON CONFLICT ("companyId") DO UPDATE SET "companyId" = EXCLUDED."companyId"
           RETURNING id"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    let min = input.min_tenure_months.val().copied().unwrap_or(0);
    if input.max_tenure_months.val().is_some_and(|max| *max <= min) {
        return Err(ApiError::bad_request(MAX_MIN_MSG));
    }
    let tier: LeaveTier = sqlx::query_as(&format!(
        r#"INSERT INTO "leaveTier" AS t
             ("companyId", "policyId", name, "minTenureMonths", "maxTenureMonths", "annualLeaveDays")
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING {TIER_COLUMNS}"#
    ))
    .bind(company_id)
    .bind(policy_id)
    .bind(&input.name)
    .bind(min)
    .bind(input.max_tenure_months.val().copied())
    .bind(input.annual_leave_days)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(tier)))
}

/// `PUT /leave-policies/tiers/:tierId`. Node: `leave-policies.controller.ts:568` → `updateTier`.
pub async fn update_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(tier_id): Path<String>,
    ValidatedJson(dto): ValidatedJson<CreateLeaveTierDto>,
) -> ApiResult<Json<LeaveTier>> {
    let input = dto.typed()?;
    let company_id = company_of(&state, &ctx).await?;
    let tier_id = parse_id(&tier_id)?;
    let Some(tier) = find_tier(&state.db, tier_id, company_id).await? else {
        return Err(ApiError::not_found("Leave tier not found"));
    };
    let min = input
        .min_tenure_months
        .val()
        .copied()
        .unwrap_or(tier.min_tenure_months);
    let max = match &input.max_tenure_months {
        Tri::Absent => tier.max_tenure_months,
        Tri::Null => None,
        Tri::Val(v) => Some(*v),
    };
    if max.is_some_and(|m| m <= min) {
        return Err(ApiError::bad_request(MAX_MIN_MSG));
    }
    let updated: LeaveTier = sqlx::query_as(&format!(
        r#"UPDATE "leaveTier" AS t
           SET name = $2, "minTenureMonths" = $3, "maxTenureMonths" = $4,
               "annualLeaveDays" = $5, "updatedAt" = $6
           WHERE t.id = $1 RETURNING {TIER_COLUMNS}"#
    ))
    .bind(tier_id)
    .bind(&input.name)
    .bind(min)
    .bind(max)
    .bind(input.annual_leave_days)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    Ok(Json(updated))
}

/// `DELETE /leave-policies/tiers/:tierId`. Node: `leave-policies.controller.ts:594` → `deleteTier`.
pub async fn delete_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(tier_id): Path<String>,
) -> ApiResult<Json<Success>> {
    let company_id = company_of(&state, &ctx).await?;
    let tier_id = parse_id(&tier_id)?;
    if find_tier(&state.db, tier_id, company_id).await?.is_none() {
        return Err(ApiError::not_found("Leave tier not found"));
    }
    let assigned: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "employeeLeaveTier" WHERE "tierId" = $1"#)
            .bind(tier_id)
            .fetch_one(&state.db)
            .await?;
    if assigned > 0 {
        return Err(ApiError::bad_request(format!(
            "Cannot delete tier: {assigned} employee(s) are assigned to it"
        )));
    }
    sqlx::query(r#"DELETE FROM "leaveTier" WHERE id = $1"#)
        .bind(tier_id)
        .execute(&state.db)
        .await?;
    Ok(Json(SUCCESS))
}

// ── penugasan tier ───────────────────────────────────────────────────────────

/// Model `employeeLeaveTier` + `tier` + `user`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmployeeTierView {
    pub id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub tier_id: i32,
    pub assigned_at: JsDate,
    pub tier: LeaveTier,
    pub user: UserMini,
}

#[derive(FromRow)]
#[sqlx(rename_all = "camelCase")]
struct EmployeeTierRow {
    id: i32,
    user_id: i32,
    company_id: i32,
    tier_id: i32,
    assigned_at: JsDate,
}

const EMPLOYEE_TIER_COLUMNS: &str =
    r#"e.id, e."userId", e."companyId", e."tierId", e."assignedAt""#;

async fn view_of(pool: &PgPool, row: EmployeeTierRow) -> ApiResult<EmployeeTierView> {
    let tier: LeaveTier = sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS} FROM "leaveTier" t WHERE t.id = $1"#
    ))
    .bind(row.tier_id)
    .fetch_one(pool)
    .await?;
    let user: UserMini = sqlx::query_as("SELECT id, name, username FROM users WHERE id = $1")
        .bind(row.user_id)
        .fetch_one(pool)
        .await?;
    Ok(EmployeeTierView {
        id: row.id,
        user_id: row.user_id,
        company_id: row.company_id,
        tier_id: row.tier_id,
        assigned_at: row.assigned_at,
        tier,
        user,
    })
}

/// `PUT /leave-policies/tiers/:tierId/assign/:userId`. Node: `leave-policies.controller.ts:618` →
/// `assignEmployeeTier`.
pub async fn assign_employee_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((tier_id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<EmployeeTierView>> {
    let company_id = company_of(&state, &ctx).await?;
    // Kedua query jalan lewat `Promise.all`: id tak sah di salah satunya = galat Prisma (500).
    let (tier_id, user_id) = (parse_id(&tier_id)?, parse_id(&user_id)?);
    let (user_ok, tier) = (
        member_exists(&state.db, user_id, company_id).await?,
        find_tier(&state.db, tier_id, company_id).await?,
    );
    if !user_ok {
        return Err(ApiError::not_found("User not found in this company"));
    }
    if tier.is_none() {
        return Err(ApiError::not_found("Leave tier not found"));
    }
    let row: EmployeeTierRow = sqlx::query_as(&format!(
        r#"INSERT INTO "employeeLeaveTier" AS e ("userId", "companyId", "tierId")
           VALUES ($1, $2, $3)
           ON CONFLICT ("userId", "companyId")
             DO UPDATE SET "tierId" = EXCLUDED."tierId", "assignedAt" = $4
           RETURNING {EMPLOYEE_TIER_COLUMNS}"#
    ))
    .bind(user_id)
    .bind(company_id)
    .bind(tier_id)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    Ok(Json(view_of(&state.db, row).await?))
}

async fn find_assignment(
    pool: &PgPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<EmployeeTierRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {EMPLOYEE_TIER_COLUMNS} FROM "employeeLeaveTier" e
           WHERE e."userId" = $1 AND e."companyId" = $2"#
    ))
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// `GET /leave-policies/tiers/employees/:userId`. Node: `leave-policies.controller.ts:497` →
/// `getEmployeeTier` (tanpa penugasan → `null` = body kosong, 200).
pub async fn get_employee_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
) -> ApiResult<Response> {
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    match find_assignment(&state.db, user_id, company_id).await? {
        Some(row) => Ok(Json(view_of(&state.db, row).await?).into_response()),
        None => Ok(null_body()),
    }
}

/// `DELETE /leave-policies/tiers/employees/:userId`. Node: `leave-policies.controller.ts:512` →
/// `removeEmployeeTier`.
pub async fn remove_employee_tier(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Success>> {
    let company_id = company_of(&state, &ctx).await?;
    let user_id = parse_id(&user_id)?;
    let done =
        sqlx::query(r#"DELETE FROM "employeeLeaveTier" WHERE "userId" = $1 AND "companyId" = $2"#)
            .bind(user_id)
            .bind(company_id)
            .execute(&state.db)
            .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found("Employee has no tier assignment"));
    }
    Ok(Json(SUCCESS))
}
