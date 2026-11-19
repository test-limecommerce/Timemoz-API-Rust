//! `src/admin-company/ownership/*` — `POST /admin-company/:uuid/transfer-owner`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_account::assignable_role::{AssignableRoleActor, RoleRef, resolve_assignable_role};
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::enums::{BaseRole, effective_role_code};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::common::{
    CompanyChange, OWNER, RawBody, UserChange, after_company_change, after_user_change, body_value,
    coded, company_not_found, gate, ok, uuid_pipe,
};
use crate::plan_limits::{Bind, update_company};
use crate::val::{V, array_keys, as_obj, trim_field};

type Op = Result<Operator, ApiError>;

/// `company.register_email` = VARCHAR(50).
pub const REGISTER_EMAIL_MAX: usize = 50;

/// `TransferOwnerDto` (`newOwnerUserUuid` lalu `reason` warisan).
pub fn validate(body: &Value) -> ApiResult<(String, String)> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["newOwnerUserUuid", "reason"]);
    v.is_uuid("newOwnerUserUuid", m.get("newOwnerUserUuid"));
    v.reason(m.get("reason"));
    v.finish()?;
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok((s("newOwnerUserUuid"), s("reason")))
}

#[derive(sqlx::FromRow)]
struct Assignment {
    id: i32,
    #[sqlx(rename = "accessLevelId")]
    access_level_id: i32,
    code: Option<String>,
    #[sqlx(rename = "isSystem")]
    is_system: Option<bool>,
    #[sqlx(rename = "baseRole")]
    base_role: Option<BaseRole>,
}

impl Assignment {
    /// `effectiveRoleCodeOfRow`.
    fn effective(&self) -> Option<String> {
        self.code
            .as_deref()
            .map(|c| effective_role_code(c, self.is_system.unwrap_or(false), self.base_role))
    }
}

async fn active_assignment(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<Assignment>> {
    sqlx::query_as(
        r#"SELECT ual.id, ual."accessLevelId", al.code, al."isSystem", al."baseRole"::text AS "baseRole"
           FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

/// `findHeuristicOwnerUserId`: anggota dengan SUPER_ADMIN aktif (role sistem) yang baris
/// `userCompany`-nya paling awal.
pub async fn find_heuristic_owner(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar(
        r#"SELECT uc."userId" FROM "userCompany" uc
           WHERE uc."companyId" = $1 AND EXISTS (
             SELECT 1 FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = uc."userId" AND ual."companyId" = $1 AND ual.status = 1
               AND al."isSystem" = true AND al.code = 'SUPER_ADMIN')
           ORDER BY uc."createdAt" ASC, uc.id ASC LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

/// `tx.userAccessLevel.update({ where: { id }, data: { accessLevelId, updatedAt } })` + audit.
async fn set_access_level(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    row_id: i32,
    access_level_id: i32,
) -> ApiResult<()> {
    let pre = audit
        .pre_read(Model::UserAccessLevel, Lookup::Id(row_id))
        .await;
    sqlx::query(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(row_id)
    .bind(access_level_id)
    .bind(now_naive())
    .execute(&mut *conn)
    .await?;
    if let Some(r) = audit
        .result_row(&mut *conn, Model::UserAccessLevel, Lookup::Id(row_id))
        .await
    {
        audit.record_update(Model::UserAccessLevel, pre, &r).await;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct LockRow {
    id: i32,
    uuid: uuid::Uuid,
    status: i32,
    #[sqlx(rename = "ownerUserId")]
    owner_user_id: Option<i32>,
    register_name: Option<String>,
    register_email: Option<String>,
}

/// Node: src/admin-company/ownership/company-ownership.controller.ts:44 (`POST /admin-company/:uuid/transfer-owner`)
pub async fn h_transfer(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), Some(OWNER))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let (new_owner_uuid, reason) = validate(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let mut tx = state.db.begin().await?;
    let company: Option<LockRow> = sqlx::query_as(
        r#"SELECT id, uuid, status, "ownerUserId", register_name, register_email
           FROM company WHERE uuid = $1 FOR UPDATE"#,
    )
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let company = company
        .filter(|c| c.status != 0)
        .ok_or_else(company_not_found)?;

    let new_owner: Option<(i32, uuid::Uuid, String, String, i32)> =
        sqlx::query_as("SELECT id, uuid, name, username, status FROM users WHERE uuid = $1::uuid")
            .bind(&new_owner_uuid)
            .fetch_optional(&mut *tx)
            .await?;
    let membership: Option<i32> = match &new_owner {
        Some((id, _, _, _, 1)) => sqlx::query_scalar(
            r#"SELECT id FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1
                   ORDER BY id LIMIT 1"#,
        )
        .bind(id)
        .bind(company.id)
        .fetch_optional(&mut *tx)
        .await?,
        _ => None,
    };
    let (Some((new_id, new_uuid, new_name, new_username, _)), Some(_)) = (new_owner, membership)
    else {
        return Err(coded(
            StatusCode::CONFLICT,
            "NOT_ACTIVE_MEMBER",
            "New owner must be an active member of this company",
        ));
    };

    let owner_source = if company.owner_user_id.is_some() {
        "ownerUserId"
    } else {
        "heuristic"
    };
    let previous_owner_id = match company.owner_user_id {
        Some(id) => Some(id),
        None => find_heuristic_owner(&mut tx, company.id).await?,
    };
    if previous_owner_id == Some(new_id) {
        return Err(coded(
            StatusCode::CONFLICT,
            "ALREADY_OWNER",
            "This user is already the company owner",
        ));
    }

    let role = |code: &'static str| RoleRef {
        code: Some(code),
        uuid: None,
    };
    let super_admin = resolve_assignable_role(
        &mut tx,
        &AssignableRoleActor::SelfRegistration,
        company.id,
        role("SUPER_ADMIN"),
    )
    .await?;
    let admin = resolve_assignable_role(
        &mut tx,
        &AssignableRoleActor::SelfRegistration,
        company.id,
        role("ADMIN"),
    )
    .await?;

    let new_row = active_assignment(&mut tx, new_id, company.id).await?;
    let new_owner_role_before = new_row.as_ref().and_then(Assignment::effective);
    match &new_row {
        None => {
            let id: i32 = sqlx::query_scalar(
                r#"INSERT INTO "userAccessLevel" ("userId", "companyId", "accessLevelId", status)
                   VALUES ($1, $2, $3, 1) RETURNING id"#,
            )
            .bind(new_id)
            .bind(company.id)
            .bind(super_admin.id)
            .fetch_one(&mut *tx)
            .await?;
            if let Some(r) = audit
                .result_row(&mut *tx, Model::UserAccessLevel, Lookup::Id(id))
                .await
            {
                audit.record_create(Model::UserAccessLevel, &r).await;
            }
        }
        Some(r) if r.access_level_id != super_admin.id => {
            set_access_level(&mut tx, &audit, r.id, super_admin.id).await?;
        }
        Some(_) => {}
    }

    // (id, uuid, roleBefore, roleAfter)
    let mut previous: Option<(i32, uuid::Uuid, Option<String>, Option<String>)> = None;
    if let Some(prev_id) = previous_owner_id {
        let prev_uuid: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT uuid FROM users WHERE id = $1")
                .bind(prev_id)
                .fetch_optional(&mut *tx)
                .await?;
        let prev_row = active_assignment(&mut tx, prev_id, company.id).await?;
        let role_before = prev_row.as_ref().and_then(Assignment::effective);
        let mut role_after = role_before.clone();
        if let Some(r) = &prev_row
            && role_before.as_deref() == Some("SUPER_ADMIN")
        {
            set_access_level(&mut tx, &audit, r.id, admin.id).await?;
            role_after = Some("ADMIN".to_owned());
        }
        if let Some(u) = prev_uuid {
            previous = Some((prev_id, u, role_before, role_after));
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    // `username.length` = unit UTF-16.
    let email_fits = new_username.encode_utf16().count() <= REGISTER_EMAIL_MAX;
    if !email_fits {
        warnings.push(format!(
            "REGISTER_EMAIL_TOO_LONG: new owner email is longer than {REGISTER_EMAIL_MAX} characters; register_email kept unchanged"
        ));
    }
    let mut sets = vec![
        ("ownerUserId".to_owned(), Bind::OptI32(Some(new_id))),
        (
            "register_name".to_owned(),
            Bind::OptStr(Some(new_name.clone())),
        ),
    ];
    if email_fits {
        sets.push((
            "register_email".to_owned(),
            Bind::OptStr(Some(new_username.clone())),
        ));
    }
    update_company(&mut tx, &audit, company.id, &sets).await?;

    let before = json!({
        "ownerUserUuid": previous.as_ref().map(|p| p.1.to_string()),
        "ownerSource": previous_owner_id.map(|_| owner_source),
        "previousOwnerRole": previous.as_ref().and_then(|p| p.2.clone()),
        "newOwnerRole": new_owner_role_before,
        "register_name": company.register_name,
        "register_email": company.register_email,
    });
    let after = json!({
        "ownerUserUuid": new_uuid.to_string(),
        "previousOwnerRole": previous.as_ref().and_then(|p| p.3.clone()),
        "newOwnerRole": "SUPER_ADMIN",
        "register_name": new_name,
        "register_email": if email_fits { Some(new_username.clone()) } else { company.register_email.clone() },
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.transfer_owner",
            target_type: "company",
            target_uuid: Some(company.uuid),
            company_id: Some(company.id),
            before: Some(&before),
            after: Some(&after),
            reason: Some(&reason),
        },
    )
    .await?;
    tx.commit().await?;

    let mut user_ids = vec![new_id];
    if let Some(p) = &previous {
        user_ids.push(p.0);
    }
    for uid in user_ids {
        after_user_change(
            &state,
            uid,
            UserChange {
                permissions: true,
                user_me: true,
                tokens: false,
            },
        )
        .await;
    }
    after_company_change(
        &state,
        company.id,
        CompanyChange {
            user_me: true,
            ..CompanyChange::default()
        },
    )
    .await;

    Ok(Json(ok(json!({
        "previousOwner": previous.as_ref().map_or(Value::Null, |p| json!({ "uuid": p.1.to_string(), "role": p.3 })),
        "newOwner": { "uuid": new_uuid.to_string(), "role": "SUPER_ADMIN" },
        "warnings": warnings,
    }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_messages() {
        match validate(&json!({ "newOwnerUserUuid": "x" })) {
            Err(ApiError::Validation(m)) => assert_eq!(
                m,
                vec![
                    "newOwnerUserUuid must be a UUID",
                    "reason must be longer than or equal to 5 characters",
                    "reason must be a string",
                ]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn effective_role_of_row() {
        let row = |code: Option<&str>, sys, base| Assignment {
            id: 1,
            access_level_id: 1,
            code: code.map(str::to_owned),
            is_system: Some(sys),
            base_role: base,
        };
        assert_eq!(
            row(Some("SUPER_ADMIN"), true, None).effective().as_deref(),
            Some("SUPER_ADMIN")
        );
        assert_eq!(
            row(Some("SALES_LEAD"), false, Some(BaseRole::MANAGER))
                .effective()
                .as_deref(),
            Some("MANAGER")
        );
        assert_eq!(row(None, false, None).effective(), None);
    }
}
