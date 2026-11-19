//! Port `src/mcp-api/common/*` (`caller-context.ts`, `response-envelope.ts`, `pagination.dto.ts`)
//! plus pembulatan gaya JavaScript yang dipakai semua service.
//!
//! Audit C5 (Nov 2026): company = aturan yang sama dengan seluruh aplikasi
//! (keanggotaan aktif default/tertua); tanpa keanggotaan aktif -> 403, tidak pernah "semua
//! company"; role hanya dari baris `userAccessLevel` company itu; user/project target wajib milik
//! company itu (404 bila bukan, supaya keberadaan uuid di tenant lain tidak terbaca).

use mz_core::{ApiError, ApiResult};
use serde_json::{Map, Value, json};
use sqlx::PgPool;

/// `MCP_MANAGER_ROLES`.
pub const MANAGER_ROLES: [&str; 4] = ["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER"];

/// Hasil `resolveMcpCaller`.
#[derive(Debug, Clone)]
pub struct McpCaller {
    pub user_id: i32,
    pub uuid: String,
    pub name: String,
    pub username: String,
    pub company_id: i32,
    pub company_uuid: String,
    pub company_name: String,
    pub roles: Vec<String>,
    /// Salah satu [`MANAGER_ROLES`] di company ini.
    pub is_manager: bool,
}

impl McpCaller {
    /// `company: { uuid, name }`.
    pub fn company_json(&self) -> Value {
        json!({ "uuid": self.company_uuid, "name": self.company_name })
    }
}

pub fn is_manager_roles(roles: &[String]) -> bool {
    roles.iter().any(|r| MANAGER_ROLES.contains(&r.as_str()))
}

type CallerRow = (String, String, String, i32, String, String);

/// Node: src/mcp-api/common/caller-context.ts:56 (`resolveMcpCaller`).
///
/// `users.findFirst({ id })` + `userCompany` (`nestedUserCompanyScope`: status 1, `isDefault DESC`,
/// `id ASC`, `take 1`) + `company`, lalu `userAccessLevel` (status 1, company itu) ->
/// `effectiveRoleCodesOf`. Tanpa keanggotaan aktif -> 403 `No company found for user`.
pub async fn resolve_mcp_caller(db: &PgPool, user_id: i32) -> ApiResult<McpCaller> {
    let row: Option<CallerRow> = sqlx::query_as(
        r#"SELECT u.uuid::text, u.name, u.username, uc."companyId", c.uuid::text, c.name
             FROM users u
             JOIN "userCompany" uc ON uc."userId" = u.id AND uc.status = 1
             JOIN company c ON c.id = uc."companyId"
            WHERE u.id = $1
            ORDER BY uc."isDefault" DESC, uc.id ASC
            LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let Some((uuid, name, username, company_id, company_uuid, company_name)) = row else {
        return Err(ApiError::forbidden("No company found for user"));
    };
    // Sama dengan `userAccessLevel.findMany({ userId, status: 1, companyId })`.
    let roles = mz_core::roles::role_codes_in_company(db, user_id, company_id).await?;
    let is_manager = is_manager_roles(&roles);
    Ok(McpCaller {
        user_id,
        uuid,
        name,
        username,
        company_id,
        company_uuid,
        company_name,
        roles,
        is_manager,
    })
}

/// Potongan SQL `memberOfCompany`: `users` beralias `{alias}`, company = parameter `{param}`
/// (mis. `$3`). Keanggotaan aktif (status 1).
pub fn member_of_company_sql(alias: &str, param: &str) -> String {
    format!(
        r#"EXISTS (SELECT 1 FROM "userCompany" mc WHERE mc."userId" = {alias}.id AND mc."companyId" = {param} AND mc.status = 1)"#
    )
}

/// Potongan SQL `projectInCompany`: `project` beralias `{alias}`.
pub fn project_in_company_sql(alias: &str, param: &str) -> String {
    format!(
        r#"EXISTS (SELECT 1 FROM "projectCompany" pcx WHERE pcx."projectId" = {alias}.id AND pcx."companyId" = {param})"#
    )
}

/// Node: src/mcp-api/common/caller-context.ts:107 (`findMemberUserId`): 404 `User not found`.
pub async fn find_member_user_id(db: &PgPool, company_id: i32, uuid: &str) -> ApiResult<i32> {
    let id: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT u.id FROM users u WHERE u.uuid = $1::uuid AND {} LIMIT 1",
        member_of_company_sql("u", "$2")
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    id.ok_or_else(|| ApiError::not_found("User not found"))
}

/// Node: src/mcp-api/common/caller-context.ts:123 (`findCompanyProjectId`): 404 `Project not found`.
pub async fn find_company_project_id(db: &PgPool, company_id: i32, uuid: &str) -> ApiResult<i32> {
    let id: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT p.id FROM project p WHERE p.uuid = $1::uuid AND {} LIMIT 1",
        project_in_company_sql("p", "$2")
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    id.ok_or_else(|| ApiError::not_found("Project not found"))
}

/// Node: src/mcp-api/common/response-envelope.ts (`envelope`).
pub fn envelope(data: Value, meta: Option<Value>) -> Value {
    match meta {
        Some(m) => json!({ "data": data, "meta": m }),
        None => json!({ "data": data }),
    }
}

/// `Math.round(x)` JavaScript (setengah dibulatkan ke atas, juga untuk negatif; NaN/Infinity tetap).
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x.abs() >= 4.5e15 {
        x
    } else {
        (x + 0.5).floor()
    }
}

/// `Math.round(x * 100) / 100`.
pub fn round2(x: f64) -> f64 {
    js_round(x * 100.0) / 100.0
}

/// Angka JSON gaya `JSON.stringify` (`5`, bukan `5.0`; NaN/Infinity -> `null`).
pub fn num(x: f64) -> Value {
    mz_core::jsfmt::num(x)
}

/// Milidetik antara dua kolom `DateTime` (`endAt.getTime() - startAt.getTime()`) dibagi 1000.
pub fn secs_between(start_ms: i64, end_ms: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        (end_ms - start_ms) as f64 / 1000.0
    }
}

/// `date.toISOString().substring(0, 10)` dari milidetik epoch.
pub fn iso_date10(ms: i64) -> Option<String> {
    mz_core::time::JsDate::from_millis(ms).map(|d| d.to_js_string().chars().take(10).collect())
}

/// Objek JSON `{}` kosong.
pub fn empty_obj() -> Value {
    Value::Object(Map::new())
}

/// `page`/`perPage` -> `skip` (`(page - 1) * perPage`); luapan Int Prisma dilampaui -> 500.
pub fn skip_of(page: i64, per_page: i64) -> ApiResult<i64> {
    page.saturating_sub(1)
        .checked_mul(per_page)
        .filter(|s| *s <= i64::from(i32::MAX) * 1000)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("skip out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_round_half_up() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(1.4), 1.0);
        assert!(js_round(f64::NAN).is_nan());
        assert_eq!(round2(1.005 * 1.0), 1.0); // 100.49999999999999
        assert_eq!(round2(0.125), 0.13);
    }

    #[test]
    fn manager_roles() {
        assert!(is_manager_roles(&["STAFF".into(), "MANAGER".into()]));
        assert!(!is_manager_roles(&["STAFF".into(), "GUEST".into()]));
        assert!(!is_manager_roles(&[]));
    }

    #[test]
    fn iso_date10_works() {
        assert_eq!(iso_date10(0).as_deref(), Some("1970-01-01"));
    }

    #[test]
    fn skip_math() {
        assert_eq!(skip_of(1, 500).ok(), Some(0));
        assert_eq!(skip_of(3, 10).ok(), Some(20));
        assert!(skip_of(i64::MAX, 500).is_err());
    }

    #[test]
    fn sql_fragments_use_params() {
        assert!(member_of_company_sql("u", "$2").contains("mc.\"companyId\" = $2"));
        assert!(project_in_company_sql("p", "$1").contains("pcx.\"projectId\" = p.id"));
    }
}
