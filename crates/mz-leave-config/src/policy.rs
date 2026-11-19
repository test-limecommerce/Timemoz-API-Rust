//! `leave-policies` bagian kebijakan perusahaan: `GET/PUT /leave-policies` dan
//! `GET /leave-policies/admins` (`leave-policies.service.ts` `getCompanyPolicy`,
//! `upsertCompanyPolicy`, `listCompanyAdmins`).

use axum::Json;
use axum::extract::State;
use chrono::Datelike;
use mz_core::extract::ValidatedJson;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::enums::{LeaveAccrualMode, LeaveEntitlementType, LeaveResetType, LeaveSickMode};
use mz_leave::jobs::enqueue_policy_switch;
use mz_leave::math::resolve_sick_mode;
use mz_leave::models::{LeavePolicy, LeaveTier, TIER_COLUMNS};
use mz_leave::policy::{load_policy, local_now, propagate_sick_total, require_time_zone};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder};

use crate::common::{UserMini, company_of};
use crate::dto::{PolicyInput, UpdateLeavePolicyDto};
use crate::validate::{Tri, node_500};

/// Peran yang dihitung sebagai "admin" (`LeavePoliciesService.ADMIN_ROLES`).
const ADMIN_CODES: [&str; 4] = ["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER"];
/// `roleCodeFilter(ADMIN_ROLES)` cabang `baseRole` (hanya ADMIN/MANAGER/STAFF punya padanan):
/// `["ADMIN", "MANAGER"]`.
fn admin_base_roles() -> Vec<&'static str> {
    mz_core::roles::role_code_filter_base_roles(&ADMIN_CODES)
}

/// Baris `leavePolicyApprover` + `user` (`include: { user: { select } }`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approver {
    pub id: i32,
    pub policy_id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub created_at: JsDate,
    pub user: UserMini,
}

#[derive(FromRow)]
struct ApproverFlat {
    id: i32,
    #[sqlx(rename = "policyId")]
    policy_id: i32,
    #[sqlx(rename = "userId")]
    user_id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    name: String,
    username: String,
}

/// `leavePolicy` + `leaveTiers` + `approvers`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyView {
    #[serde(flatten)]
    pub policy: LeavePolicy,
    pub leave_tiers: Vec<LeaveTier>,
    pub approvers: Vec<Approver>,
}

/// `findUnique({ where: { companyId }, include: { leaveTiers: orderBy minTenureMonths asc,
/// approvers: include user orderBy user.name asc } })`.
pub async fn load_view(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<PolicyView>> {
    let Some(policy) = load_policy(&mut *conn, company_id).await? else {
        return Ok(None);
    };
    let leave_tiers: Vec<LeaveTier> = sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS} FROM "leaveTier" t WHERE t."policyId" = $1
           ORDER BY t."minTenureMonths" ASC, t.id ASC"#
    ))
    .bind(policy.id)
    .fetch_all(&mut *conn)
    .await?;
    let approvers: Vec<ApproverFlat> = sqlx::query_as(
        r#"SELECT a.id, a."policyId", a."userId", a."companyId", a."createdAt", u.name, u.username
           FROM "leavePolicyApprover" a JOIN users u ON u.id = a."userId"
           WHERE a."policyId" = $1 ORDER BY u.name ASC, a.id ASC"#,
    )
    .bind(policy.id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(PolicyView {
        policy,
        leave_tiers,
        approvers: approvers
            .into_iter()
            .map(|a| Approver {
                id: a.id,
                policy_id: a.policy_id,
                user_id: a.user_id,
                company_id: a.company_id,
                created_at: a.created_at,
                user: UserMini {
                    id: a.user_id,
                    name: a.name,
                    username: a.username,
                },
            })
            .collect(),
    }))
}

/// `GET /leave-policies`. Node: `leave-policies.controller.ts:221` → `getCompanyPolicy`.
pub async fn get_policy(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let company_id = company_of(&state, &ctx).await?;
    let mut conn = state.db.acquire().await?;
    match load_view(&mut conn, company_id).await? {
        Some(view) => Ok(Json(
            serde_json::to_value(view).map_err(ApiError::internal)?,
        )),
        None => Ok(Json(json!({
            "companyId": company_id,
            "annualLeaveDays": 12,
            "sickLeaveDays": null,
            "sickLeaveMode": "DEDUCT_ANNUAL",
            "carryOverEnabled": false,
            "maxCarryOverDays": null,
            "carryOverExpiryMonth": null,
            "resetType": "ANNUAL",
            "entitlementType": "UNIFORM",
            "accrualMode": "UPFRONT",
            "monthlyAccrualDays": null,
            "leaveTiers": [],
            "approvers": [],
        }))),
    }
}

fn is_half_step(v: f64) -> bool {
    (v * 2.0).is_finite() && (v * 2.0).fract() == 0.0
}

/// `PUT /leave-policies`. Node: `leave-policies.controller.ts:272` → `upsertCompanyPolicy`
/// (`leave-policies.service.ts:79`). Efek samping: propagasi jatah cuti sakit ke saldo tahun
/// berjalan, `accruedThroughMonth` saat UPFRONT → MONTHLY, enqueue `leave.policy-switch` SESUDAH
/// commit saat MONTHLY → UPFRONT.
pub async fn upsert_policy(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<UpdateLeavePolicyDto>,
) -> ApiResult<Json<PolicyView>> {
    let input = dto.typed()?;
    let company_id = company_of(&state, &ctx).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    let current = load_policy(&mut *tx, company_id).await?;

    // Validasi nilai gabungan (DB + DTO), bukan DTO saja.
    let merged_mode = input
        .accrual_mode
        .val()
        .copied()
        .or(current.as_ref().map(|c| c.accrual_mode))
        .unwrap_or(LeaveAccrualMode::UPFRONT);
    let merged_monthly: Option<f64> = match &input.monthly_accrual_days {
        Tri::Absent => current.as_ref().and_then(|c| c.monthly_accrual_days),
        Tri::Null => None,
        Tri::Val(v) => Some(*v),
    };
    let merged_annual = input
        .annual_leave_days
        .val()
        .copied()
        .or(current.as_ref().map(|c| c.annual_leave_days))
        .unwrap_or(12);

    if merged_mode == LeaveAccrualMode::MONTHLY {
        if !merged_monthly.is_some_and(|m| m > 0.0 && is_half_step(m)) {
            return Err(ApiError::coded(
                http::StatusCode::BAD_REQUEST,
                "LEAVE_POLICY_MONTHLY_ACCRUAL_REQUIRED",
                "Jumlah cuti per bulan wajib diisi (kelipatan 0,5) untuk mode bulanan",
            ));
        }
        if merged_monthly.is_some_and(|m| m > f64::from(merged_annual)) {
            return Err(ApiError::coded(
                http::StatusCode::BAD_REQUEST,
                "LEAVE_POLICY_MONTHLY_EXCEEDS_MAX",
                "Jumlah per bulan tidak boleh melebihi maksimal per tahun",
            ));
        }
    }
    let previous_mode = current
        .as_ref()
        .map_or(LeaveAccrualMode::UPFRONT, |c| c.accrual_mode);

    // Mode cuti sakit: eksplisit dari DTO; klien lama yang hanya kirim sickLeaveDays diturunkan.
    let previous_sick_mode = resolve_sick_mode(current.as_ref());
    let sick_touched = input.sick_leave_mode.defined() || input.sick_leave_days.defined();
    let sick_mode: LeaveSickMode = match input.sick_leave_mode.val() {
        Some(m) => *m,
        None if input.sick_leave_days.defined() => {
            if input.sick_leave_days.val().is_some() {
                LeaveSickMode::LIMITED
            } else if previous_sick_mode == LeaveSickMode::UNLIMITED {
                LeaveSickMode::UNLIMITED
            } else {
                LeaveSickMode::DEDUCT_ANNUAL
            }
        }
        None if current.is_some() => previous_sick_mode,
        None => LeaveSickMode::UNLIMITED,
    };
    let sick_days: Option<i32> = if sick_mode == LeaveSickMode::LIMITED {
        if input.sick_leave_days.defined() {
            input.sick_leave_days.val().copied()
        } else {
            current.as_ref().and_then(|c| c.sick_leave_days)
        }
    } else {
        None
    };
    if sick_mode == LeaveSickMode::LIMITED && sick_days.is_none() {
        return Err(ApiError::coded(
            http::StatusCode::BAD_REQUEST,
            "LEAVE_POLICY_SICK_DAYS_REQUIRED",
            "Jatah cuti sakit (sickLeaveDays) wajib diisi untuk mode LIMITED",
        ));
    }

    if let Some(ids) = input.approver_ids.as_deref().filter(|ids| !ids.is_empty()) {
        validate_approvers(&mut tx, company_id, ids).await?;
    }

    let policy_id = write_policy(
        &mut tx,
        company_id,
        current.as_ref(),
        &input,
        merged_mode,
        merged_monthly,
        sick_touched.then_some((sick_mode, sick_days)),
        now,
    )
    .await?;

    // Jatah cuti sakit baru berlaku untuk saldo tahun berjalan (baris yang pernah di-adjust
    // manual tidak ditimpa).
    if sick_touched
        && sick_mode == LeaveSickMode::LIMITED
        && (previous_sick_mode != LeaveSickMode::LIMITED
            || current.as_ref().and_then(|c| c.sick_leave_days) != sick_days)
    {
        propagate_sick_total(&mut tx, company_id, sick_days.unwrap_or(0), now).await?;
    }

    // UPFRONT → MONTHLY: bulan yang sudah lewat dianggap sudah dikreditkan (research R4).
    if previous_mode == LeaveAccrualMode::UPFRONT && merged_mode == LeaveAccrualMode::MONTHLY {
        let tz = require_time_zone(&mut *tx, company_id).await?;
        let local = local_now(now, &tz);
        sqlx::query(
            r#"UPDATE "leaveBalance" SET "accruedThroughMonth" = $3, "updatedAt" = $4
               WHERE "companyId" = $1 AND year = $2"#,
        )
        .bind(company_id)
        .bind(local.year())
        .bind(i32::try_from(local.month0()).unwrap_or(0))
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    if let Some(ids) = &input.approver_ids {
        sqlx::query(r#"DELETE FROM "leavePolicyApprover" WHERE "policyId" = $1"#)
            .bind(policy_id)
            .execute(&mut *tx)
            .await?;
        if !ids.is_empty() {
            sqlx::query(
                r#"INSERT INTO "leavePolicyApprover" ("policyId", "userId", "companyId")
                   SELECT $1, uid, $3 FROM UNNEST($2::int[]) AS t(uid)"#,
            )
            .bind(policy_id)
            .bind(ids)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    let saved = load_view(&mut tx, company_id)
        .await?
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("leavePolicy hilang setelah upsert")))?;
    tx.commit().await?;

    // Setelah commit; enqueue tidak pernah melempar (FR-026), tick per jam menutup celah.
    if previous_mode == LeaveAccrualMode::MONTHLY && merged_mode == LeaveAccrualMode::UPFRONT {
        let updated_ms = saved.policy.updated_at.map_or_else(
            || JsDate::now().timestamp_millis(),
            |d| d.timestamp_millis(),
        );
        enqueue_policy_switch(&state, company_id, updated_ms).await;
    }
    Ok(Json(saved))
}

/// Approver harus user aktif dengan peran admin/manager (capability, `roleCodeFilter`) di company.
async fn validate_approvers(
    conn: &mut PgConnection,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<()> {
    let valid: Vec<i32> = sqlx::query_scalar(
        r#"SELECT u.id FROM users u
           WHERE u.id = ANY($1) AND u.status = 1
             AND EXISTS (
               SELECT 1 FROM "userAccessLevel" ual
               JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               WHERE ual."userId" = u.id AND ual."companyId" = $2 AND ual.status = 1
                 AND ((al."isSystem" = true AND al.code = ANY($3))
                      OR (al."isSystem" = false AND al."baseRole"::text = ANY($4))))"#,
    )
    .bind(ids)
    .bind(company_id)
    .bind(ADMIN_CODES.as_slice())
    .bind(admin_base_roles())
    .fetch_all(&mut *conn)
    .await?;
    let invalid: Vec<String> = ids
        .iter()
        .filter(|id| !valid.contains(id))
        .map(ToString::to_string)
        .collect();
    if invalid.is_empty() {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "User(s) [{}] are not a valid SYSTEM_ADMIN/SUPER_ADMIN/ADMIN/MANAGER in this company",
        invalid.join(", ")
    )))
}

/// `leavePolicy.upsert`: buat (`create`) bila belum ada, selain itu `update` hanya field yang
/// dikirim. `sick` = `Some((mode, days))` bila `sickTouched`.
#[allow(clippy::too_many_arguments)]
async fn write_policy(
    conn: &mut PgConnection,
    company_id: i32,
    current: Option<&LeavePolicy>,
    input: &PolicyInput,
    merged_mode: LeaveAccrualMode,
    merged_monthly: Option<f64>,
    sick: Option<(LeaveSickMode, Option<i32>)>,
    now: chrono::NaiveDateTime,
) -> ApiResult<i32> {
    let Some(current) = current else {
        let (sick_mode, sick_days) = sick.unwrap_or((LeaveSickMode::UNLIMITED, None));
        let id: i32 = sqlx::query_scalar(
            r#"INSERT INTO "leavePolicy"
                 ("companyId", "annualLeaveDays", "sickLeaveDays", "sickLeaveMode",
                  "carryOverEnabled", "maxCarryOverDays", "carryOverExpiryMonth", "resetType",
                  "entitlementType", "accrualMode", "monthlyAccrualDays")
               VALUES ($1, $2, $3, $4::"LeaveSickMode", $5, $6, $7, $8::"LeaveResetType",
                       $9::"LeaveEntitlementType", $10::"LeaveAccrualMode", $11)
               RETURNING id"#,
        )
        .bind(company_id)
        .bind(input.annual_leave_days.val().copied().unwrap_or(12))
        .bind(sick_days)
        .bind(sick_mode.as_str())
        .bind(input.carry_over_enabled.val().copied().unwrap_or(false))
        .bind(input.max_carry_over_days.val().copied())
        .bind(input.carry_over_expiry_month.val().copied())
        .bind(
            input
                .reset_type
                .val()
                .copied()
                .unwrap_or(LeaveResetType::ANNUAL)
                .as_str(),
        )
        .bind(
            input
                .entitlement_type
                .val()
                .copied()
                .unwrap_or(LeaveEntitlementType::UNIFORM)
                .as_str(),
        )
        .bind(merged_mode.as_str())
        .bind(merged_monthly)
        .fetch_one(&mut *conn)
        .await?;
        return Ok(id);
    };

    // Kolom non-null yang diisi `null` = galat argumen Prisma (500).
    if matches!(input.annual_leave_days, Tri::Null)
        || matches!(input.carry_over_enabled, Tri::Null)
        || matches!(input.reset_type, Tri::Null)
        || matches!(input.entitlement_type, Tri::Null)
        || matches!(input.accrual_mode, Tri::Null)
    {
        return Err(node_500("leavePolicy.update: null pada kolom wajib"));
    }
    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "leavePolicy" SET "#);
    {
        let mut set = qb.separated(", ");
        if let Some(v) = input.annual_leave_days.val() {
            set.push(r#""annualLeaveDays" = "#)
                .push_bind_unseparated(*v);
        }
        if let Some((mode, days)) = sick {
            set.push(r#""sickLeaveDays" = "#)
                .push_bind_unseparated(days);
            set.push(r#""sickLeaveMode" = "#)
                .push_bind_unseparated(mode.as_str())
                .push_unseparated(r#"::"LeaveSickMode""#);
        }
        if let Some(v) = input.carry_over_enabled.val() {
            set.push(r#""carryOverEnabled" = "#)
                .push_bind_unseparated(*v);
        }
        if let Some(v) = input.max_carry_over_days.clone().value_or_null() {
            set.push(r#""maxCarryOverDays" = "#)
                .push_bind_unseparated(v);
        }
        if let Some(v) = input.carry_over_expiry_month.clone().value_or_null() {
            set.push(r#""carryOverExpiryMonth" = "#)
                .push_bind_unseparated(v);
        }
        if let Some(v) = input.reset_type.val() {
            set.push(r#""resetType" = "#)
                .push_bind_unseparated(v.as_str())
                .push_unseparated(r#"::"LeaveResetType""#);
        }
        if let Some(v) = input.entitlement_type.val() {
            set.push(r#""entitlementType" = "#)
                .push_bind_unseparated(v.as_str())
                .push_unseparated(r#"::"LeaveEntitlementType""#);
        }
        if let Some(v) = input.accrual_mode.val() {
            set.push(r#""accrualMode" = "#)
                .push_bind_unseparated(v.as_str())
                .push_unseparated(r#"::"LeaveAccrualMode""#);
        }
        if input.monthly_accrual_days.defined() {
            set.push(r#""monthlyAccrualDays" = "#)
                .push_bind_unseparated(merged_monthly);
        }
        set.push(r#""updatedAt" = "#).push_bind_unseparated(now);
    }
    qb.push(r#" WHERE id = "#).push_bind(current.id);
    qb.build().execute(&mut *conn).await?;
    Ok(current.id)
}

/// Baris `GET /leave-policies/admins`.
#[derive(Debug, Serialize, FromRow)]
pub struct AdminRow {
    pub id: i32,
    pub name: String,
    pub username: String,
    pub role: Option<String>,
}

/// `GET /leave-policies/admins`. Node: `leave-policies.controller.ts:310` → `listCompanyAdmins`.
pub async fn list_admins(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Vec<AdminRow>>> {
    let company_id = company_of(&state, &ctx).await?;
    // `role`: effectiveRoleCodeOfRow atas penugasan aktif TERBARU (`take: 1 orderBy id desc`),
    // bukan baris yang kebetulan cocok dengan filter.
    let rows: Vec<AdminRow> = sqlx::query_as(
        r#"SELECT u.id, u.name, u.username,
             (SELECT CASE WHEN al."isSystem" THEN al.code ELSE COALESCE(al."baseRole"::text, al.code) END
                FROM "userAccessLevel" x LEFT JOIN "accessLevel" al ON al.id = x."accessLevelId"
               WHERE x."userId" = u.id AND x."companyId" = $1 AND x.status = 1
               ORDER BY x.id DESC LIMIT 1) AS role
           FROM users u
           WHERE u.status = 1
             AND EXISTS (
               SELECT 1 FROM "userAccessLevel" ual
               JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               WHERE ual."userId" = u.id AND ual."companyId" = $1 AND ual.status = 1
                 AND ((al."isSystem" = true AND al.code = ANY($2))
                      OR (al."isSystem" = false AND al."baseRole"::text = ANY($3))))
           ORDER BY u.name ASC, u.id ASC"#,
    )
    .bind(company_id)
    .bind(ADMIN_CODES.as_slice())
    .bind(admin_base_roles())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}
