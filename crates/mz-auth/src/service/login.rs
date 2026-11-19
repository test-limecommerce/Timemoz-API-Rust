//! `AuthService.login` (`auth.service.ts:801`).

use mz_account::error_codes::{
    ACCOUNT_DEACTIVATED, ACCOUNT_NOT_ACTIVE, INVALID_CREDENTIALS, NO_ACTIVE_COMPANY,
    SUBSCRIPTION_INACTIVE,
};
use mz_account::password::{INVALID_CREDENTIALS_MESSAGE, compare_password_safely};
use mz_account::role_summary::{ROLE_SUMMARY_COLUMNS, RoleSummaryRow, role_summary_of};
use mz_core::company::USER_COMPANY_SCOPE_SQL;
use mz_core::enums::effective_role_code;
use mz_core::user_status::{STATUS_DELETED, STATUS_DISABLED, STATUS_INVITED};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};

use crate::analytics::spawn_app_installed;
use crate::login_attempts::{self, LOGIN_LOCKED_MESSAGE, Scope};
use crate::rowjson::{row_to_map, row_to_value};
use crate::service::session::{activate_device_session, sign_user_token};

#[derive(sqlx::FromRow)]
struct LoginUser {
    id: i32,
    password: String,
    username: String,
    name: String,
    status: i32,
    uuid: String,
}

fn fail(code: &str, message: &str) -> Value {
    json!({ "success": false, "code": code, "message": message })
}

/// Kolom `company` yang dipilih login web (`companySelect`, tanpa `profilePictureFullPath`).
const COMPANY_SELECT: &str = r#"c.id, c.uuid, c.name, c.telp, c.address, c."websiteURL", c.register_name,
    c.register_email, c.register_roles, c.register_total_employees, c."timeZone", c.currency,
    c."workweekStartDay", c."dailyReportSentAt", c."isAlllowDeleteScreenshot", c."fixedHourNotification",
    c."isAgencyMode", c."isTrial", c."maxUsers", c."maxProject", c."useIdleDetection", c."useCheatDetection",
    c."useReports", c."useTimesheetApproval", c."useActivityLevel", c."useIntegrations",
    c."screenshotResolution", c."screenshotRetentionDays", c."reportExportsPerMonth",
    c."reportExportsUsedThisMonth", c."reportExportsResetAt", c."dataHistoryMonths",
    c."budgetAlertDeliveryChannel", c."clientProofExportAllowed", c."createdAt", c."updatedAt", c.status,
    c."actualProfilePictureName", c."hasCreateOrgnanization", c."inviteCode", c."dontChargeInvite",
    c."clickupApiToken", c."clickupWorkspaceId""#;

/// Peran user di company (`userAccessLevel` terbaru, tanpa filter status — sama dengan Node).
pub async fn role_of(
    state: &AppState,
    user_id: i32,
    company_id: i32,
) -> ApiResult<(String, Option<Value>)> {
    let sql = format!(
        r#"SELECT {ROLE_SUMMARY_COLUMNS} FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2
           ORDER BY ual.id DESC LIMIT 1"#
    );
    let row = sqlx::query_as::<_, RoleSummaryRow>(&sql)
        .bind(user_id)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
    // `effectiveRoleCodeOfRow(roles) || 'USER'`
    let access_level = row
        .as_ref()
        .map(|r| effective_role_code(&r.code, r.is_system, r.base_role))
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "USER".into());
    let role = serde_json::to_value(role_summary_of(row.as_ref())).map_err(ApiError::internal)?;
    Ok((access_level, (!role.is_null()).then_some(role)))
}

/// `{...userCompany, company: {...company, ...}, allowTakeScreenshot, maxUsers, ...}`.
fn company_block(mut user_company: Map<String, Value>, company: Option<Value>) -> Value {
    let mut company = match company {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for (k, v) in [
        ("maxUsers", json!(0)),
        ("maxProject", json!(999)),
        ("maxScreenshotPerUser", json!(9_999_999)),
    ] {
        company.insert(k.into(), v);
    }
    let allow = user_company
        .get("isScreenshotActive")
        .cloned()
        .unwrap_or(Value::Null);
    user_company.insert("company".into(), Value::Object(company));
    user_company.insert("allowTakeScreenshot".into(), allow);
    for (k, v) in [
        ("maxUsers", json!(0)),
        ("maxProject", json!(999)),
        ("maxScreenshotPerUser", json!(9_999_999)),
    ] {
        user_company.insert(k.into(), v);
    }
    Value::Object(user_company)
}

pub struct LoginInput<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub mac_address: Option<&'a str>,
    pub os: Option<&'a str>,
    pub platform: Option<&'a str>,
}

/// `AuthService.login(username, password, macAddress, os, platform)`.
pub async fn login(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    input: LoginInput<'_>,
) -> ApiResult<Value> {
    let LoginInput {
        username,
        password,
        platform,
        ..
    } = input;
    // `if (macAddress)` / `if (os)`: string kosong = tidak dikirim.
    let mac = input.mac_address.filter(|m| !m.is_empty());
    let os = input.os.filter(|o| !o.is_empty());

    // 0. Jatah percobaan per username, SEBELUM password dibandingkan.
    if !login_attempts::consume(state, Scope::User, username).await {
        return Ok(json!({ "success": false, "message": LOGIN_LOCKED_MESSAGE }));
    }

    // 1-2. Password dulu, baru status (akun tak ada = waktu & jawaban sama).
    let user = sqlx::query_as::<_, LoginUser>(
        r#"SELECT id, password, username, name, status, uuid::text AS "uuid" FROM users WHERE username = $1"#,
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?;
    let valid =
        compare_password_safely(state, password, user.as_ref().map(|u| u.password.as_str())).await;
    let Some(user) = user.filter(|_| valid) else {
        return Ok(fail(INVALID_CREDENTIALS, INVALID_CREDENTIALS_MESSAGE));
    };
    login_attempts::reset(state, Scope::User, username).await;

    // 3. Status.
    if user.status == STATUS_DISABLED || user.status == STATUS_DELETED {
        return Ok(fail(
            ACCOUNT_DEACTIVATED,
            "Your account has been deactivated.",
        ));
    }
    if user.status == STATUS_INVITED {
        return Ok(fail(
            ACCOUNT_NOT_ACTIVE,
            "Please activate your account via email.",
        ));
    }

    // 4. Langganan company (hanya desktop yang mengirim macAddress).
    let mut company_for_analytics = None;
    if mac.is_some() {
        let sql = format!(
            r#"SELECT uc."companyId",
                      (SELECT s.status::text FROM subscriptions s WHERE s."companyId" = uc."companyId"
                       ORDER BY s.id DESC LIMIT 1) AS status
               FROM "userCompany" uc WHERE {USER_COMPANY_SCOPE_SQL} LIMIT 1"#
        );
        let row: Option<(i32, Option<String>)> = sqlx::query_as(&sql)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?;
        company_for_analytics = row.as_ref().map(|r| r.0);
        if matches!(
            row.and_then(|r| r.1).as_deref(),
            Some("INACTIVE" | "SUSPENDED")
        ) {
            return Ok(fail(
                SUBSCRIPTION_INACTIVE,
                "Your company subscription is inactive or suspended. Please contact support.",
            ));
        }
    }

    // 5. Satu device desktop aktif.
    let mut sid = None;
    if let Some(mac) = mac {
        tracing::warn!(
            "User {} logging in from MAC address {mac} with os: {}",
            user.id,
            os.unwrap_or("undefined")
        );
        sid = Some(activate_device_session(state, user.id, mac, input.os).await?);
    }

    // 6. Token — `sid` hanya untuk login desktop.
    let access_token = sign_user_token(state, user.id, sid.map(Value::String))?;

    spawn_app_installed(
        state,
        audit,
        user.id,
        company_for_analytics,
        "password",
        platform,
        input.mac_address,
        input.os,
    );

    let mut data = Map::new();
    data.insert("id".into(), json!(user.id));
    data.insert("username".into(), json!(user.username));
    data.insert("name".into(), json!(user.name));
    data.insert("status".into(), json!(user.status));
    data.insert("uuid".into(), json!(user.uuid));

    // 7. Desktop: data minimal.
    if mac.is_some() && os.is_some() {
        data.insert("accessToken".into(), json!(access_token));
        return Ok(json!({ "success": true, "message": "Login Success", "data": data }));
    }

    // 8. Web: company & role.
    let sql =
        format!(r#"SELECT uc.* FROM "userCompany" uc WHERE {USER_COMPANY_SCOPE_SQL} LIMIT 1"#);
    let Some(uc_row) = sqlx::query(&sql)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(fail(
            NO_ACTIVE_COMPANY,
            "User has no active company assigned.",
        ));
    };
    let user_company = row_to_map(&uc_row)?;
    let company_id = user_company
        .get("companyId")
        .and_then(Value::as_i64)
        .and_then(|c| i32::try_from(c).ok())
        .unwrap_or(0);

    let company_sql = format!("SELECT {COMPANY_SELECT} FROM company c WHERE c.id = $1");
    let company = match sqlx::query(&company_sql)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?
    {
        Some(row) => {
            let mut map = row_to_map(&row)?;
            let trials = sqlx::query(r#"SELECT * FROM trials WHERE "companyId" = $1 ORDER BY id"#)
                .bind(company_id)
                .fetch_all(&state.db)
                .await?
                .iter()
                .map(row_to_value)
                .collect::<Result<Vec<_>, _>>()?;
            map.insert("trial".into(), Value::Array(trials));
            Some(Value::Object(map))
        }
        None => None,
    };
    let has_create_organization = company
        .as_ref()
        .and_then(|c| c.get("hasCreateOrgnanization"))
        .cloned()
        // `company.hasCreateOrgnanization` dengan `company` null = TypeError di Node.
        .ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!("login: company {company_id} not found"))
        })?;

    let (access_level, role) = role_of(state, user.id, company_id).await?;

    data.insert("accessLevel".into(), json!(access_level));
    data.insert("role".into(), role.unwrap_or(Value::Null));
    data.insert("hasCreateOrganization".into(), has_create_organization);
    data.insert("company".into(), company_block(user_company, company));
    data.insert("accessToken".into(), json!(access_token));
    Ok(json!({ "success": true, "message": "Login Success", "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn company_block_overrides_limits() {
        let uc = json!({"id": 1, "companyId": 9, "isScreenshotActive": true});
        let block = company_block(
            uc.as_object().cloned().unwrap_or_default(),
            Some(json!({"id": 9, "maxUsers": 50, "trial": []})),
        );
        assert_eq!(
            block
                .pointer("/allowTakeScreenshot")
                .cloned()
                .unwrap_or_default(),
            json!(true)
        );
        assert_eq!(
            block.pointer("/maxUsers").cloned().unwrap_or_default(),
            json!(0)
        );
        assert_eq!(
            block.pointer("/maxProject").cloned().unwrap_or_default(),
            json!(999)
        );
        assert_eq!(
            block
                .pointer("/company/maxUsers")
                .cloned()
                .unwrap_or_default(),
            json!(0)
        );
        assert_eq!(
            block
                .pointer("/company/maxScreenshotPerUser")
                .cloned()
                .unwrap_or_default(),
            json!(9_999_999)
        );
        assert_eq!(
            block.pointer("/company/trial").cloned().unwrap_or_default(),
            json!([])
        );
    }

    #[test]
    fn failure_shape() {
        assert_eq!(
            fail(INVALID_CREDENTIALS, INVALID_CREDENTIALS_MESSAGE),
            json!({"success":false,"code":"INVALID_CREDENTIALS","message":"Invalid username or password"})
        );
    }
}
