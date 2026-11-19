//! `src/admin-company/lifecycle/*` — hapus/pulihkan company, putar kode undangan, dan
//! `POST /admin/invite-codes/fill-missing` (4 route).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::common::{
    CompanyChange, OWNER, OWNER_SUPPORT, RawBody, UserChange, after_company_change,
    after_user_change, body_value, coded, company_not_found, gate, is_invite_code_collision, ok,
    uuid_pipe,
};
use crate::plan_limits::{Bind, current_sub, update_company};
use crate::val::{V, array_keys, as_obj, trim_field};

type Op = Result<Operator, ApiError>;

/// Anggota `userCompany.status` 1 (aktif) / 2 (menunggu).
const MEMBER_STATUSES: [i32; 2] = [1, 2];
const INVITE_CODE_MAX_ATTEMPTS: usize = 5;

/// `ReasonDto` di bawah `ADMIN_VALIDATION_PIPE`. Mengembalikan `reason` ter-trim.
pub fn validate_reason_dto(body: &Value) -> ApiResult<String> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["reason"]);
    v.reason(m.get("reason"));
    v.finish()?;
    Ok(m.get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// `DeleteCompanyDto` (`confirmName` dulu, lalu `reason` warisan).
pub fn validate_delete_dto(body: &Value) -> ApiResult<(String, String)> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["confirmName", "reason"]);
    v.length("confirmName", m.get("confirmName"), 1, 255);
    v.is_string("confirmName", m.get("confirmName"));
    v.reason(m.get("reason"));
    v.finish()?;
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok((s("confirmName"), s("reason")))
}

#[derive(sqlx::FromRow)]
struct LockRow {
    id: i32,
    uuid: uuid::Uuid,
    name: String,
    status: i32,
    #[sqlx(rename = "inviteCode")]
    invite_code: Option<String>,
}

/// Kunci baris company (FOR UPDATE); tidak ada = 404 `COMPANY_NOT_FOUND`.
async fn lock(conn: &mut PgConnection, uuid: &uuid::Uuid) -> ApiResult<LockRow> {
    let row: Option<LockRow> = sqlx::query_as(
        r#"SELECT id, uuid, name, status, "inviteCode" FROM company WHERE uuid = $1 FOR UPDATE"#,
    )
    .bind(uuid)
    .fetch_optional(conn)
    .await?;
    row.ok_or_else(company_not_found)
}

/// `deleteTimeoutMs(members)`.
pub fn delete_timeout_ms(members: i64) -> i64 {
    (10_000 + 50 * members).min(60_000)
}

/// Node: src/admin-company/lifecycle/company-lifecycle.controller.ts:39 (`POST /admin-company/:uuid/delete`)
pub async fn h_delete(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let (confirm_name, reason) = validate_delete_dto(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let member_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
           WHERE c.uuid = $1 AND uc.status = ANY($2)"#,
    )
    .bind(uuid)
    .bind(&MEMBER_STATUSES[..])
    .fetch_one(&state.db)
    .await?;

    // Node: jumlah anggota hanya menskalakan timeout transaksi Prisma (`deleteTimeoutMs`);
    // sqlx tidak punya timeout transaksi interaktif.
    let _ = delete_timeout_ms(member_count);
    let mut tx = state.db.begin().await?;
    let company = lock(&mut tx, &uuid).await?;
    if company.status == 0 {
        return Err(coded(
            StatusCode::CONFLICT,
            "ALREADY_DELETED",
            "Company is already deleted",
        ));
    }
    if confirm_name != company.name {
        return Err(coded(
            StatusCode::BAD_REQUEST,
            "CONFIRM_NAME_MISMATCH",
            "confirmName must equal the company name exactly",
        ));
    }
    let current = current_sub(&mut *tx, company.id).await?;
    if current.as_ref().is_some_and(|c| {
        c.billing_source == "PAYPAL"
            && matches!(c.paypal_status.as_deref(), Some("ACTIVE" | "SUSPENDED"))
    }) {
        return Err(coded(
            StatusCode::CONFLICT,
            "PAYPAL_SUBSCRIPTION_ACTIVE",
            "Company has a live PayPal subscription; cancel it before deleting",
        ));
    }

    let mut user_ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = ANY($2) ORDER BY id"#,
    )
    .bind(company.id)
    .bind(&MEMBER_STATUSES[..])
    .fetch_all(&mut *tx)
    .await?;
    let mut seen = std::collections::HashSet::new();
    user_ids.retain(|u| seen.insert(*u));

    update_company(
        &mut tx,
        &audit,
        company.id,
        &[("status".into(), Bind::I32(0))],
    )
    .await?;
    for uid in &user_ids {
        mz_account::user_ops::revoke_all_user_tokens_audited(&mut tx, &audit, *uid).await?;
    }
    let before = json!({ "status": company.status });
    let after = json!({ "status": 0, "revokedUsers": user_ids.len() });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.delete",
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

    after_company_change(
        &state,
        company.id,
        CompanyChange {
            modules: true,
            divisions: true,
            plan: true,
            user_me: false,
        },
    )
    .await;
    for uid in &user_ids {
        after_user_change(
            &state,
            *uid,
            UserChange {
                tokens: true,
                permissions: true,
                user_me: true,
            },
        )
        .await;
    }
    Ok((
        StatusCode::CREATED,
        Json(ok(json!({
            "uuid": raw,
            "status": 0,
            "revokedUsers": user_ids.len(),
        }))),
    ))
}

/// Node: src/admin-company/lifecycle/company-lifecycle.controller.ts:57 (`POST /admin-company/:uuid/restore`)
pub async fn h_restore(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let reason = validate_reason_dto(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let mut tx = state.db.begin().await?;
    let company = lock(&mut tx, &uuid).await?;
    if company.status != 0 {
        return Err(coded(
            StatusCode::CONFLICT,
            "NOT_DELETED",
            "Company is not deleted",
        ));
    }
    update_company(
        &mut tx,
        &audit,
        company.id,
        &[("status".into(), Bind::I32(1))],
    )
    .await?;
    let before = json!({ "status": 0 });
    let after = json!({ "status": 1 });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.restore",
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

    after_company_change(
        &state,
        company.id,
        CompanyChange {
            modules: true,
            divisions: true,
            plan: true,
            user_me: true,
        },
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(ok(json!({ "uuid": raw, "status": 1 }))),
    ))
}

/// Node: src/admin-company/lifecycle/company-lifecycle.controller.ts:69 (`POST /admin-company/:uuid/invite-code/regenerate`)
pub async fn h_regenerate(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_SUPPORT))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let reason = validate_reason_dto(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let mut attempt = 0;
    let code = loop {
        attempt += 1;
        let code = mz_auth::service::invite::generate_invite_code();
        match regenerate_once(&state, &ctx, &audit, &op, &uuid, &code, &reason).await {
            Ok(()) => break code,
            Err(e) if attempt < INVITE_CODE_MAX_ATTEMPTS && is_invite_code_collision(&e) => {}
            Err(e) => return Err(e),
        }
    };
    let frontend = mz_account::util::env_or_undefined(state.config.var("FRONTEND_URL"));
    Ok((
        StatusCode::CREATED,
        Json(ok(json!({
            "inviteCode": code,
            "inviteLink": format!("{frontend}/join/{code}"),
        }))),
    ))
}

async fn regenerate_once(
    state: &AppState,
    ctx: &AuditCtx,
    audit: &AuditHandle,
    op: &Operator,
    uuid: &uuid::Uuid,
    code: &str,
    reason: &str,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    let company = lock(&mut tx, uuid).await?;
    if company.status == 0 {
        return Err(company_not_found());
    }
    let pre = audit.pre_read(Model::Company, Lookup::Id(company.id)).await;
    sqlx::query(r#"UPDATE company SET "inviteCode" = $2 WHERE id = $1"#)
        .bind(company.id)
        .bind(code)
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Company, Lookup::Id(company.id))
        .await
    {
        audit.record_update(Model::Company, pre, &r).await;
    }
    let before = json!({ "inviteCode": company.invite_code });
    let after = json!({ "inviteCode": code });
    record(
        &mut *tx,
        ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.invite_code_regenerate",
            target_type: "company",
            target_uuid: Some(company.uuid),
            company_id: Some(company.id),
            before: Some(&before),
            after: Some(&after),
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Node: src/admin-company/lifecycle/invite-codes-admin.controller.ts:36 (`POST /admin/invite-codes/fill-missing`)
pub async fn h_fill_missing(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER))?;
    let reason = validate_reason_dto(&body_value(body))?;

    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT id FROM company WHERE status <> 0 AND "inviteCode" IS NULL ORDER BY id ASC"#,
    )
    .fetch_all(&state.db)
    .await?;
    let mut filled = 0_i64;
    let mut failed = 0_i64;
    for id in ids {
        match fill_one(&state, &audit, id).await {
            Ok(true) => filled += 1,
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                tracing::warn!(company_id = id, error = %e, "Gagal mengisi kode undangan company");
            }
        }
    }
    let after = json!({ "filled": filled, "failed": failed });
    let mut tx = state.db.begin().await?;
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invite_codes.fill_missing",
            target_type: "company",
            target_uuid: None,
            company_id: None,
            before: None,
            after: Some(&after),
            reason: Some(&reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(ok(after))))
}

/// `company.updateMany({ where: { id, inviteCode: null }, data: { inviteCode } })` + retry.
async fn fill_one(state: &AppState, audit: &AuditHandle, id: i32) -> ApiResult<bool> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let code = mz_auth::service::invite::generate_invite_code();
        let pre = audit
            .pre_read_many(
                Model::Company,
                Lookup::filter(move |qb| {
                    qb.push("id = ")
                        .push_bind(id)
                        .push(r#" AND "inviteCode" IS NULL"#);
                }),
            )
            .await;
        let res = sqlx::query(
            r#"UPDATE company SET "inviteCode" = $2 WHERE id = $1 AND "inviteCode" IS NULL"#,
        )
        .bind(id)
        .bind(&code)
        .execute(&state.db)
        .await;
        match res {
            Ok(r) => {
                let n = r.rows_affected();
                audit
                    .record_update_many(
                        Model::Company,
                        pre,
                        n,
                        &mz_audit::Row::new().with("inviteCode", code.as_str()),
                        json!({ "id": id, "inviteCode": Value::Null }),
                    )
                    .await;
                return Ok(n > 0);
            }
            Err(e) => {
                let e = ApiError::from(e);
                if attempt >= INVITE_CODE_MAX_ATTEMPTS || !is_invite_code_collision(&e) {
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_timeout_scales_and_caps() {
        assert_eq!(delete_timeout_ms(0), 10_000);
        assert_eq!(delete_timeout_ms(100), 15_000);
        assert_eq!(delete_timeout_ms(10_000), 60_000);
    }

    #[test]
    fn delete_dto_order() {
        match validate_delete_dto(&json!({ "x": 1, "reason": "ok reason" })) {
            Err(ApiError::Validation(m)) => assert_eq!(
                m,
                vec![
                    "property x should not exist",
                    "confirmName must be longer than or equal to 1 characters",
                    "confirmName must be a string",
                ]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reason_dto_trims() {
        assert_eq!(
            validate_reason_dto(&json!({ "reason": "  hello world  " })).ok(),
            Some("hello world".to_owned())
        );
        assert!(validate_reason_dto(&json!([1])).is_err());
    }
}
