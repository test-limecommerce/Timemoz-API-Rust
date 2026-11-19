//! Port `src/quickstart/*`: progres Quickstart per company.
//!
//! Node: `quickstart.controller.ts` (`@PermissionExempt('self-scoped')`, tanpa
//! `@RequireModule` dengan sengaja), `quickstart.service.ts`, `quickstart.steps.ts`.
//!
//! Setiap langkah di-stempel sekali lewat `COALESCE(kolom, NOW())`, jadi stempel tidak
//! pernah mundur. Langkah 7 (`FINISH`) hanya diset oleh `POST /v1/quickstart/finish`.
//! Semua query terikat `companyId` milik pemanggil (`require_company_id`).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::NaiveDateTime;
use mz_core::company::require_company_id;
use mz_core::enums::Role;
use mz_core::gate::{RouteSpec, guard};
use mz_core::permissions::Exempt;
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};

const ALL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
    Role::GUEST,
];
const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `@UseGuards(JwtAuthGuard, RolesGuard)` + `@Roles(...)` semua role; tanpa `@RequireModule`.
const STATUS_SPEC: RouteSpec = RouteSpec::jwt().roles(ALL_ROLES).exempt(Exempt::SelfScoped);
/// `finish`: admin saja.
const FINISH_SPEC: RouteSpec = RouteSpec::jwt().roles(ADMINS).exempt(Exempt::SelfScoped);

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/v1/quickstart",
            guard(state, STATUS_SPEC, get(status_handler)),
        )
        .route(
            "/v1/quickstart/finish",
            guard(state, FINISH_SPEC, post(finish_handler)),
        )
}

/// Kolom stempel di `companyQuickstart`. Nama kolom berasal dari konstanta kode
/// ([`Col::sql`]), tidak pernah dari input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    InviteTeam,
    CompanySetting,
    ManageRegion,
    ManageCustomer,
    ManageStore,
    VisitPlan,
    Finish,
}

impl Col {
    const fn sql(self) -> &'static str {
        match self {
            Col::InviteTeam => r#""inviteTeamAt""#,
            Col::CompanySetting => r#""companySettingAt""#,
            Col::ManageRegion => r#""manageRegionAt""#,
            Col::ManageCustomer => r#""manageCustomerAt""#,
            Col::ManageStore => r#""manageStoreAt""#,
            Col::VisitPlan => r#""visitPlanAt""#,
            Col::Finish => r#""finishAt""#,
        }
    }

    /// `FINISH` tidak pernah dievaluasi; enam langkah lain ya.
    const fn evaluated(self) -> bool {
        !matches!(self, Col::Finish)
    }
}

/// `QUICKSTART_STEPS` (`quickstart.steps.ts`).
struct StepDef {
    key: &'static str,
    order: u8,
    col: Col,
}

const STEPS: [StepDef; 7] = [
    StepDef {
        key: "INVITE_TEAM",
        order: 1,
        col: Col::InviteTeam,
    },
    StepDef {
        key: "COMPANY_SETTING",
        order: 2,
        col: Col::CompanySetting,
    },
    StepDef {
        key: "MANAGE_REGION",
        order: 3,
        col: Col::ManageRegion,
    },
    StepDef {
        key: "MANAGE_CUSTOMER",
        order: 4,
        col: Col::ManageCustomer,
    },
    StepDef {
        key: "MANAGE_STORE",
        order: 5,
        col: Col::ManageStore,
    },
    StepDef {
        key: "VISIT_PLAN",
        order: 6,
        col: Col::VisitPlan,
    },
    StepDef {
        key: "FINISH",
        order: 7,
        col: Col::Finish,
    },
];

/// `Math.round(done / total * 100)`.
pub fn percent(done: usize) -> u32 {
    #[allow(clippy::cast_precision_loss)]
    let ratio = done as f64 / STEPS.len() as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (ratio * 100.0).round() as u32;
    pct
}

/// Baris `companyQuickstart` + `company.moduleSalesTrackerEnabled` (`readRow`).
#[derive(Debug, sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
struct Row {
    invite_team_at: Option<NaiveDateTime>,
    company_setting_at: Option<NaiveDateTime>,
    manage_region_at: Option<NaiveDateTime>,
    manage_customer_at: Option<NaiveDateTime>,
    manage_store_at: Option<NaiveDateTime>,
    visit_plan_at: Option<NaiveDateTime>,
    finish_at: Option<NaiveDateTime>,
    completed_at: Option<NaiveDateTime>,
    module_sales_tracker_enabled: bool,
}

impl Row {
    fn stamp(&self, col: Col) -> Option<NaiveDateTime> {
        match col {
            Col::InviteTeam => self.invite_team_at,
            Col::CompanySetting => self.company_setting_at,
            Col::ManageRegion => self.manage_region_at,
            Col::ManageCustomer => self.manage_customer_at,
            Col::ManageStore => self.manage_store_at,
            Col::VisitPlan => self.visit_plan_at,
            Col::Finish => self.finish_at,
        }
    }
}

/// Hasil enam pengecekan "pernah ada" (`evaluate`).
#[derive(Debug, sqlx::FromRow)]
struct Checks {
    #[sqlx(rename = "INVITE_TEAM")]
    invite_team: bool,
    #[sqlx(rename = "COMPANY_SETTING")]
    company_setting: bool,
    #[sqlx(rename = "MANAGE_REGION")]
    manage_region: bool,
    #[sqlx(rename = "MANAGE_CUSTOMER")]
    manage_customer: bool,
    #[sqlx(rename = "MANAGE_STORE")]
    manage_store: bool,
    #[sqlx(rename = "VISIT_PLAN")]
    visit_plan: bool,
}

impl Checks {
    fn holds(&self, col: Col) -> bool {
        match col {
            Col::InviteTeam => self.invite_team,
            Col::CompanySetting => self.company_setting,
            Col::ManageRegion => self.manage_region,
            Col::ManageCustomer => self.manage_customer,
            Col::ManageStore => self.manage_store,
            Col::VisitPlan => self.visit_plan,
            Col::Finish => false,
        }
    }
}

const CHECKS_SQL: &str = r#"
SELECT
  (SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1) > 1 AS "INVITE_TEAM",
  EXISTS (SELECT 1 FROM "geofence" WHERE "companyId" = $1) AS "COMPANY_SETTING",
  EXISTS (
    SELECT 1 FROM "storeGroupUser" sgu
    JOIN "storeGroup" sg ON sg."id" = sgu."groupId"
    WHERE sg."companyId" = $1
  ) AS "MANAGE_REGION",
  EXISTS (SELECT 1 FROM "customer" WHERE "companyId" = $1) AS "MANAGE_CUSTOMER",
  EXISTS (SELECT 1 FROM "store" WHERE "companyId" = $1) AS "MANAGE_STORE",
  EXISTS (SELECT 1 FROM "storeVisitPlan" WHERE "companyId" = $1) AS "VISIT_PLAN"
FROM "company" c
WHERE c."id" = $1"#;

const FINISH_SQL: &str = r#"
UPDATE "companyQuickstart"
SET "finishAt" = NOW(), "completedAt" = NOW(), "updatedAt" = NOW()
WHERE "companyId" = $1
  AND "finishAt" IS NULL
  AND "inviteTeamAt" IS NOT NULL
  AND "companySettingAt" IS NOT NULL
  AND "manageRegionAt" IS NOT NULL
  AND "manageCustomerAt" IS NOT NULL
  AND "manageStoreAt" IS NOT NULL
  AND "visitPlanAt" IS NOT NULL"#;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepStatus {
    key: &'static str,
    order: u8,
    completed: bool,
    completed_at: Option<JsDate>,
}

/// Bentuk `QuickstartStatus` Node (urutan kunci sama).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    applicable: bool,
    completed: bool,
    completed_at: Option<JsDate>,
    percent: u32,
    steps: Vec<StepStatus>,
}

/// `EXEMPT_STATUS`: company tanpa baris atau dengan Sales Tracker mati.
fn exempt_status() -> Status {
    Status {
        applicable: false,
        completed: true,
        completed_at: None,
        percent: 100,
        steps: Vec::new(),
    }
}

fn to_status(row: &Row) -> Status {
    let steps: Vec<StepStatus> = STEPS
        .iter()
        .map(|s| {
            let at = row.stamp(s.col);
            StepStatus {
                key: s.key,
                order: s.order,
                completed: at.is_some(),
                completed_at: at.map(JsDate),
            }
        })
        .collect();
    let done = steps.iter().filter(|s| s.completed).count();
    Status {
        applicable: true,
        completed: row.completed_at.is_some(),
        completed_at: row.completed_at.map(JsDate),
        percent: percent(done),
        steps,
    }
}

async fn read_row(db: &PgPool, company_id: i32) -> sqlx::Result<Option<Row>> {
    sqlx::query_as(
        r#"SELECT q."inviteTeamAt", q."companySettingAt", q."manageRegionAt",
                  q."manageCustomerAt", q."manageStoreAt", q."visitPlanAt",
                  q."finishAt", q."completedAt", c."moduleSalesTrackerEnabled"
           FROM "companyQuickstart" q
           JOIN "company" c ON c."id" = q."companyId"
           WHERE q."companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await
}

/// Node: `evaluate`. Menjalankan enam pengecekan dan men-stempel HANYA langkah yang baru
/// terpenuhi lewat satu UPDATE. Mengembalikan true kalau ada stempel baru.
async fn evaluate(db: &PgPool, company_id: i32, row: &Row) -> sqlx::Result<bool> {
    if row.completed_at.is_some() {
        return Ok(false);
    }
    let Some(checks) = sqlx::query_as::<_, Checks>(CHECKS_SQL)
        .bind(company_id)
        .fetch_optional(db)
        .await?
    else {
        return Ok(false);
    };

    let to_stamp: Vec<Col> = STEPS
        .iter()
        .map(|s| s.col)
        .filter(|c| c.evaluated() && checks.holds(*c) && row.stamp(*c).is_none())
        .collect();
    if to_stamp.is_empty() {
        return Ok(false);
    }

    // Identifier kolom dari konstanta kode (`Col::sql`); nilai tetap di-bind.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(r#"UPDATE "companyQuickstart" SET "#);
    for (i, col) in to_stamp.iter().enumerate() {
        if i > 0 {
            qb.push(", ");
        }
        qb.push(col.sql())
            .push(" = COALESCE(")
            .push(col.sql())
            .push(", NOW())");
    }
    qb.push(r#", "updatedAt" = NOW() WHERE "companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND "completedAt" IS NULL"#);
    qb.build().execute(db).await?;
    Ok(true)
}

/// Node: `quickstart.service.ts:144` `evaluate(companyId)` tanpa `row` (dipakai cron
/// pengingat): baris dibaca dulu; tidak ada baris / sudah selesai → `false`.
pub async fn evaluate_company(db: &PgPool, company_id: i32) -> sqlx::Result<bool> {
    let Some(row) = read_row(db, company_id).await? else {
        return Ok(false);
    };
    evaluate(db, company_id, &row).await
}

/// Node: `getStatus`.
pub async fn get_status(state: &AppState, company_id: i32) -> ApiResult<Status> {
    let Some(mut row) = read_row(&state.db, company_id).await? else {
        return Ok(exempt_status());
    };
    if !row.module_sales_tracker_enabled {
        return Ok(exempt_status());
    }
    if row.completed_at.is_none()
        && evaluate(&state.db, company_id, &row).await?
        && let Some(fresh) = read_row(&state.db, company_id).await?
    {
        row = fresh;
    }
    Ok(to_status(&row))
}

/// Node: `finish`. Satu UPDATE bersyarat; memanggilnya ulang pada company yang sudah
/// selesai itu idempoten.
pub async fn finish(state: &AppState, company_id: i32) -> ApiResult<Status> {
    let row = match read_row(&state.db, company_id).await? {
        Some(r) if r.module_sales_tracker_enabled => r,
        _ => {
            return Err(ApiError::coded(
                StatusCode::NOT_FOUND,
                "QUICKSTART_NOT_APPLICABLE",
                "Quickstart is not applicable to this company",
            ));
        }
    };
    if row.completed_at.is_some() {
        return Ok(to_status(&row));
    }

    evaluate(&state.db, company_id, &row).await?;
    let updated = sqlx::query(FINISH_SQL)
        .bind(company_id)
        .execute(&state.db)
        .await?
        .rows_affected();

    let row = read_row(&state.db, company_id).await?.unwrap_or(row);
    if updated == 0 && row.completed_at.is_none() {
        let missing: Vec<&str> = STEPS
            .iter()
            .filter(|s| s.col.evaluated() && row.stamp(s.col).is_none())
            .map(|s| s.key)
            .collect();
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "QUICKSTART_STEPS_INCOMPLETE",
                "message": "Quickstart steps are not complete",
                "missing": missing,
            }),
        ));
    }
    Ok(to_status(&row))
}

/// Amplop `{ statusCode, message, data }` (controller Node). `statusCode` di badan selalu
/// 200 walaupun `POST finish` membalas HTTP 201 (bawaan Nest untuk POST).
#[derive(Debug, Serialize)]
pub struct Envelope {
    #[serde(rename = "statusCode")]
    status_code: u16,
    message: &'static str,
    data: Status,
}

/// Node: `quickstart.controller.ts:getStatus`.
async fn status_handler(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let data = get_status(&state, company_id).await?;
    Ok(Json(Envelope {
        status_code: 200,
        message: "OK",
        data,
    }))
}

/// Node: `quickstart.controller.ts:finish`. Tanpa `@HttpCode` → HTTP 201.
async fn finish_handler(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<(StatusCode, Json<Envelope>)> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let data = finish(&state, company_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(Envelope {
            status_code: 200,
            message: "OK",
            data,
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn percent_matches_math_round() {
        let expected = [0, 14, 29, 43, 57, 71, 86, 100];
        for (done, pct) in expected.iter().enumerate() {
            assert_eq!(percent(done), *pct, "{done} dari 7");
        }
    }

    #[test]
    fn steps_match_node_order_and_finish_is_not_evaluated() {
        let keys: Vec<&str> = STEPS.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            [
                "INVITE_TEAM",
                "COMPANY_SETTING",
                "MANAGE_REGION",
                "MANAGE_CUSTOMER",
                "MANAGE_STORE",
                "VISIT_PLAN",
                "FINISH"
            ]
        );
        let evaluated = STEPS.iter().filter(|s| s.col.evaluated()).count();
        assert_eq!(evaluated, 6);
    }

    #[test]
    fn exempt_status_shape() {
        let v = serde_json::to_value(exempt_status()).expect("json");
        assert_eq!(
            v,
            json!({"applicable": false, "completed": true, "completedAt": null, "percent": 100, "steps": []})
        );
    }
}
