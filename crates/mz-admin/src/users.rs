//! Controller `admin/users` — dukungan user untuk operator (specs/007 contracts/m4).
//! Node: `src/admin/users/admin-users.controller.ts` + `admin-users.service.ts`.
//!
//! Gerbang tiap route: `JwtAdminAuthGuard` ([`Operator`]) → `OperatorRolesGuard` (`op.require`) →
//! pipe (`ParseUUIDPipe` lalu `ADMIN_VALIDATION_PIPE` untuk query/body). POST tanpa `@HttpCode` → 201.
//! Tulisan ke `users`/`company` melewati [`AuditHandle`]; jejak operator ke `operatorAuditLog` ditulis
//! di dalam transaksi yang sama dengan perubahannya.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_account::division_membership::remove_users_from_divisions_in_tx;
use mz_account::manual_seat_limit::assert_manual_seat_available;
use mz_account::user_ops::{revoke_all_user_tokens_audited, send_email_forgot_password};
use mz_account::username_conflict::is_username_conflict;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::db::advisory_xact_lock;
use mz_core::time::{JsDate, now_naive};
use mz_core::user_status::{STATUS_ACTIVE, STATUS_DELETED, STATUS_DISABLED, STATUS_INVITED};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::dto::{
    CHANGE_EMAIL, OPTIONAL_REASON, REASON, RawBody, RawQuery, UserSearch, opt_text, run_body, text,
    validate_user_search,
};

/// Panjang `company.register_email` (VARCHAR(50)).
const REGISTER_EMAIL_MAX: usize = 50;
const RECENT_ACTIONS_LIMIT: i64 = 20;

fn err(status: StatusCode, code: &str, message: &str) -> ApiError {
    ApiError::coded(status, code, message)
}

fn not_found() -> ApiError {
    err(StatusCode::NOT_FOUND, "USER_NOT_FOUND", "User not found")
}

fn email_in_use() -> ApiError {
    err(
        StatusCode::CONFLICT,
        "EMAIL_IN_USE",
        "Email is already used by another account",
    )
}

/// `statusLabel`: `ACTIVE` | `INVITED` | `DISABLED` | `DELETED`, selain itu angkanya.
pub fn status_label(status: i32) -> String {
    match status {
        STATUS_ACTIVE => "ACTIVE".to_owned(),
        STATUS_INVITED => "INVITED".to_owned(),
        STATUS_DISABLED => "DISABLED".to_owned(),
        STATUS_DELETED => "DELETED".to_owned(),
        other => other.to_string(),
    }
}

fn status_number(label: &str) -> Option<i32> {
    match label {
        "ACTIVE" => Some(STATUS_ACTIVE),
        "INVITED" => Some(STATUS_INVITED),
        "DISABLED" => Some(STATUS_DISABLED),
        "DELETED" => Some(STATUS_DELETED),
        _ => None,
    }
}

/// `effectiveRoleCodeOf`: `isSystem ? code : (baseRole ?? code)`.
fn effective_role(code: &str, is_system: bool, base_role: Option<&str>) -> String {
    if is_system {
        code.to_owned()
    } else {
        base_role.unwrap_or(code).to_owned()
    }
}

// ── Pembacaan (`userSelect` + `present`) ────────────────────────────────────

type BaseRow = (i32, String, String, String, i32, Option<JsDate>, bool);

struct Membership {
    company_id: i32,
    company_uuid: String,
    company_name: String,
    company_status: i32,
    owner_user_id: Option<i32>,
    status: i32,
    is_default: bool,
}

/// `(companyId, role)`; `role` = `null` bila penugasan tanpa relasi `accessLevel`.
type RoleRow = (i32, Option<String>);

/// Baris user siap dipresentasikan (`present`) + penanda tautan Google.
struct UserView {
    base: BaseRow,
    memberships: Vec<Membership>,
    roles: Vec<RoleRow>,
}

impl UserView {
    fn present(&self) -> Value {
        let (id, uuid, name, username, status, verified, _) = &self.base;
        let memberships: Vec<Value> = self
            .memberships
            .iter()
            .map(|m| {
                let role = self
                    .roles
                    .iter()
                    .find(|(company_id, _)| *company_id == m.company_id)
                    .and_then(|(_, r)| r.clone());
                json!({
                    "company": {
                        "uuid": m.company_uuid,
                        "name": m.company_name,
                        "status": status_label(m.company_status),
                    },
                    "role": role,
                    "membershipStatus": status_label(m.status),
                    "isDefault": m.is_default,
                    "isOwner": m.owner_user_id == Some(*id),
                })
            })
            .collect();
        json!({
            "uuid": uuid,
            "name": name,
            "email": username,
            "status": status_label(*status),
            "emailVerifiedAt": verified,
            "memberships": memberships,
        })
    }
}

const BASE_COLUMNS: &str = r#"u.id, u.uuid::text, u.name, u.username, u.status, u."emailVerifiedAt", u."googleId" IS NOT NULL"#;

/// Muat relasi (`userCompany` + company, `userAccessLevel` aktif + role) untuk `bases`.
async fn hydrate(state: &AppState, bases: Vec<BaseRow>) -> Result<Vec<UserView>, sqlx::Error> {
    let ids: Vec<i32> = bases.iter().map(|b| b.0).collect();
    type MemberRow = (i32, i32, String, String, i32, Option<i32>, i32, bool);
    let members: Vec<MemberRow> = sqlx::query_as(
        r#"SELECT uc."userId", c.id, c.uuid::text, c.name, c.status, c."ownerUserId", uc.status, uc."isDefault"
             FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
            WHERE uc."userId" = ANY($1)
            ORDER BY uc."userId", uc.id"#,
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    type AccessRow = (i32, i32, Option<String>, Option<bool>, Option<String>);
    let access: Vec<AccessRow> = sqlx::query_as(
        r#"SELECT ual."userId", ual."companyId", al.code, al."isSystem", al."baseRole"::text
             FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
            WHERE ual."userId" = ANY($1) AND ual.status = 1
            ORDER BY ual."userId", ual.id"#,
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;

    let mut members_by_user: HashMap<i32, Vec<Membership>> = HashMap::new();
    for (
        user_id,
        company_id,
        company_uuid,
        company_name,
        company_status,
        owner,
        status,
        is_default,
    ) in members
    {
        members_by_user
            .entry(user_id)
            .or_default()
            .push(Membership {
                company_id,
                company_uuid,
                company_name,
                company_status,
                owner_user_id: owner,
                status,
                is_default,
            });
    }
    let mut roles_by_user: HashMap<i32, Vec<RoleRow>> = HashMap::new();
    for (user_id, company_id, code, is_system, base_role) in access {
        let role =
            code.map(|c| effective_role(&c, is_system.unwrap_or(false), base_role.as_deref()));
        roles_by_user
            .entry(user_id)
            .or_default()
            .push((company_id, role));
    }
    Ok(bases
        .into_iter()
        .map(|base| UserView {
            memberships: members_by_user.remove(&base.0).unwrap_or_default(),
            roles: roles_by_user.remove(&base.0).unwrap_or_default(),
            base,
        })
        .collect())
}

fn push_filter(qb: &mut QueryBuilder<'_, Postgres>, search: Option<&str>, status: Option<i32>) {
    let mut first = true;
    let mut sep = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(if first { " WHERE " } else { " AND " });
        first = false;
    };
    if let Some(s) = search {
        sep(qb);
        let pattern = format!("%{s}%");
        qb.push("(u.username ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR u.name ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(st) = status {
        sep(qb);
        qb.push("u.status = ").push_bind(st);
    }
}

/// Node: src/admin/users/admin-users.controller.ts:50 (`search`) → `AdminUsersService.search`.
pub async fn search(
    State(state): State<AppState>,
    op: Operator,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    op.require(&[
        OperatorRole::Owner,
        OperatorRole::Finance,
        OperatorRole::Support,
    ])?;
    let UserSearch {
        search,
        status,
        page_number,
        items_per_page,
    } = validate_user_search(&q)?;
    let per_page = items_per_page.min(50.0);
    let skip = (page_number - 1.0) * per_page;
    if skip >= 9.0e18 {
        // Prisma gagal memuat `skip` (tidak ditangkap di Node → 500).
        return Err(ApiError::internal(anyhow::anyhow!("skip out of range")));
    }
    #[allow(clippy::cast_possible_truncation)]
    let (skip, take, page_out, per_out) = (
        skip as i64,
        per_page as i64,
        page_number as i64,
        per_page as i64,
    );
    let status_n = status.as_deref().and_then(status_number);

    let mut rows_q = QueryBuilder::<Postgres>::new(format!("SELECT {BASE_COLUMNS} FROM users u"));
    push_filter(&mut rows_q, search.as_deref(), status_n);
    // Terbaru di atas, supaya user yang baru daftar langsung terlihat.
    rows_q
        .push(" ORDER BY u.id DESC OFFSET ")
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(take);
    let mut count_q = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM users u");
    push_filter(&mut count_q, search.as_deref(), status_n);

    let (total, bases) = tokio::try_join!(
        count_q.build_query_scalar::<i64>().fetch_one(&state.db),
        rows_q.build_query_as::<BaseRow>().fetch_all(&state.db),
    )?;
    let views = hydrate(&state, bases).await?;
    Ok(Json(json!({
        "success": true,
        "data": {
            "result": views.iter().map(UserView::present).collect::<Vec<_>>(),
            "pageNumber": page_out,
            "itemsPerPage": per_out,
            "total": total,
        },
    })))
}

/// Node: src/admin/users/admin-users.controller.ts:57 (`detail`) → `AdminUsersService.detail`.
pub async fn detail(
    State(state): State<AppState>,
    op: Operator,
    Path(raw_uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    op.require(&[
        OperatorRole::Owner,
        OperatorRole::Finance,
        OperatorRole::Support,
    ])?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let base: Option<BaseRow> = sqlx::query_as(&format!(
        "SELECT {BASE_COLUMNS} FROM users u WHERE u.uuid = $1"
    ))
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(base) = base else {
        return Err(not_found());
    };
    let has_google = base.6;
    let view = hydrate(&state, vec![base])
        .await?
        .into_iter()
        .next()
        .ok_or_else(not_found)?;
    type ActionRow = (String, String, Option<String>, JsDate, Option<i32>, String);
    let actions: Vec<ActionRow> = sqlx::query_as(
        r#"SELECT l.uuid::text, l.action, l.reason, l."createdAt", l."companyId", o.name
             FROM "operatorAuditLog" l JOIN operators o ON o.id = l."operatorId"
            WHERE l."targetUuid" = $1 ORDER BY l.id DESC LIMIT $2"#,
    )
    .bind(uuid)
    .bind(RECENT_ACTIONS_LIMIT)
    .fetch_all(&state.db)
    .await?;
    let recent: Vec<Value> = actions
        .into_iter()
        .map(
            |(uuid, action, reason, created_at, company_id, operator_name)| {
                json!({
                    "uuid": uuid,
                    "action": action,
                    "reason": reason,
                    "createdAt": created_at,
                    "companyId": company_id,
                    "operator": { "name": operator_name },
                })
            },
        )
        .collect();
    let mut data = view.present();
    if let Value::Object(m) = &mut data {
        m.insert("hasGoogleLink".into(), Value::Bool(has_google));
        m.insert("recentOperatorActions".into(), Value::Array(recent));
    }
    Ok(Json(json!({ "success": true, "data": data })))
}

// ── Tulis ───────────────────────────────────────────────────────────────────

struct TargetUser {
    id: i32,
    username: String,
    status: i32,
    has_google: bool,
}

async fn require_user(state: &AppState, uuid: uuid::Uuid) -> ApiResult<TargetUser> {
    let row: Option<(i32, String, i32, bool)> = sqlx::query_as(
        r#"SELECT id, username, status, "googleId" IS NOT NULL FROM users WHERE uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    row.map(|(id, username, status, has_google)| TargetUser {
        id,
        username,
        status,
        has_google,
    })
    .ok_or_else(not_found)
}

/// `AdminCompanyCacheInvalidator.afterUserChange`: cache permission, revokasi token, dan `user-me-<id>`
/// dibuang SESUDAH commit.
async fn after_user_change(
    state: &AppState,
    user_id: i32,
    tokens: bool,
    permissions: bool,
    user_me: bool,
) {
    if permissions {
        state.invalidate_user_access(user_id);
    }
    if tokens {
        state.invalidate_token_revocation(user_id);
    }
    if user_me {
        mz_core::invalidation::forget_user_me(state, user_id).await;
    }
}

struct AuditArgs<'a> {
    action: &'a str,
    before: Option<Value>,
    after: Option<Value>,
    reason: Option<&'a str>,
}

/// `recordOperatorAction(tx, { targetType: 'user', targetUuid, ... })`.
async fn operator_audit(
    conn: &mut PgConnection,
    ctx: &AuditCtx,
    op: &Operator,
    target: uuid::Uuid,
    a: AuditArgs<'_>,
) -> sqlx::Result<()> {
    record(
        conn,
        ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: a.action,
            target_type: "user",
            target_uuid: Some(target),
            before: a.before.as_ref(),
            after: a.after.as_ref(),
            reason: a.reason,
            ..Default::default()
        },
    )
    .await
}

fn created(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(v))
}

const WRITE_ROLES: [OperatorRole; 2] = [OperatorRole::Owner, OperatorRole::Support];

/// Node: src/admin/users/admin-users.controller.ts:65 (`sendResetPassword`).
pub async fn send_reset_password(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[OPTIONAL_REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = opt_text(&dto, "reason");
    let user = require_user(&state, uuid).await?;
    if user.status != STATUS_ACTIVE && user.status != STATUS_INVITED {
        return Err(err(
            StatusCode::CONFLICT,
            "USER_NOT_ACTIVE",
            "User is disabled or deleted",
        ));
    }
    // Audit dulu (commit), baru kirim: email tak bisa ditarik kembali.
    let mut tx = state.db.begin().await?;
    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.send_reset_password",
            before: Some(json!({ "status": status_label(user.status) })),
            after: None,
            reason,
        },
    )
    .await?;
    tx.commit().await?;
    // Nilai `username` tersimpan apa adanya (pencocokan persis).
    send_email_forgot_password(&state, &user.username).await?;
    Ok(created(
        json!({ "success": true, "data": { "uuid": raw_uuid } }),
    ))
}

/// Node: src/admin/users/admin-users.controller.ts:77 (`verifyEmail`). Hanya dari INVITED.
pub async fn verify_email(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = text(&dto, "reason");
    let user = require_user(&state, uuid).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    // Hanya INVITED: DISABLED/DELETED tak boleh diaktifkan ulang lewat jalur ini.
    let user_id = user.id;
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(user_id)
                    .push(" AND status = ")
                    .push_bind(STATUS_INVITED);
            }),
        )
        .await;
    let affected = sqlx::query(
        r#"UPDATE users SET status = $3, "emailVerifiedAt" = $4 WHERE id = $1 AND status = $2"#,
    )
    .bind(user.id)
    .bind(STATUS_INVITED)
    .bind(STATUS_ACTIVE)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new()
                .with("status", STATUS_ACTIVE)
                .with("emailVerifiedAt", json!(JsDate(now))),
            json!({ "id": user.id, "status": STATUS_INVITED }),
        )
        .await;
    if affected == 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "NOT_PENDING_VERIFICATION",
            "User is not pending email verification",
        ));
    }
    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.verify_email",
            before: Some(json!({ "status": "INVITED" })),
            after: Some(json!({ "status": "ACTIVE", "emailVerifiedAt": JsDate(now) })),
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    after_user_change(&state, user.id, false, true, true).await;
    Ok(created(
        json!({ "success": true, "data": { "uuid": raw_uuid, "status": "ACTIVE" } }),
    ))
}

/// Node: src/admin/users/admin-users.controller.ts:89 (`changeEmail`). PATCH → 200; sesi lama dicabut.
pub async fn change_email(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[CHANGE_EMAIL, REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    // DTO sudah men-trim + lowercase `email`; service melakukannya sekali lagi.
    let new_email = text(&dto, "email").trim().to_lowercase();
    let reason = text(&dto, "reason");
    let user = require_user(&state, uuid).await?;
    let mut warnings: Vec<&str> = Vec::new();

    let mut tx = state.db.begin().await?;
    // Satu pemenang untuk dua operator yang memakai email sama (TOCTOU).
    advisory_xact_lock(&mut tx, &format!("user-email:{new_email}")).await?;
    let current: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(current) = current else {
        return Err(not_found());
    };
    if current.to_lowercase() == new_email {
        return Err(err(
            StatusCode::CONFLICT,
            "SAME_EMAIL",
            "The new email is the same as the current one",
        ));
    }
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE LOWER(username) = LOWER($1) AND id <> $2 LIMIT 1",
    )
    .bind(&new_email)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?;
    if taken.is_some() {
        return Err(email_in_use());
    }

    // `tx.users.update({ username })` + audit `users`.
    let pre = audit.pre_read(Model::Users, Lookup::Id(user.id)).await;
    sqlx::query("UPDATE users SET username = $2 WHERE id = $1")
        .bind(user.id)
        .bind(&new_email)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_username_conflict(&e) {
                email_in_use()
            } else {
                e.into()
            }
        })?;
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Users, Lookup::Id(user.id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    revoke_all_user_tokens_audited(&mut tx, &audit, user.id).await?;

    let owned: Vec<i32> = sqlx::query_scalar(r#"SELECT id FROM company WHERE "ownerUserId" = $1"#)
        .bind(user.id)
        .fetch_all(&mut *tx)
        .await?;
    if !owned.is_empty() {
        if new_email.encode_utf16().count() <= REGISTER_EMAIL_MAX {
            let pre = audit
                .pre_read_many(Model::Company, Lookup::Ids(owned.clone()))
                .await;
            let affected = sqlx::query("UPDATE company SET register_email = $2 WHERE id = ANY($1)")
                .bind(&owned)
                .bind(&new_email)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            audit
                .record_update_many(
                    Model::Company,
                    pre,
                    affected,
                    &Row::new().with("register_email", new_email.as_str()),
                    json!({ "id": { "in": owned } }),
                )
                .await;
        } else {
            warnings.push("OWNER_EMAIL_TOO_LONG_FOR_COMPANY_RECORD");
        }
    }
    if user.has_google {
        warnings.push("GOOGLE_SIGNIN_NEW_ADDRESS");
    }

    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.change_email",
            before: Some(json!({ "email": current })),
            after: Some(json!({ "email": new_email })),
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    after_user_change(&state, user.id, true, true, true).await;
    Ok(Json(json!({
        "success": true,
        "data": { "uuid": raw_uuid, "email": new_email, "warnings": warnings },
    })))
}

/// Node: src/admin/users/admin-users.controller.ts:106 (`disable`).
pub async fn disable(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = text(&dto, "reason");
    let user = require_user(&state, uuid).await?;

    let mut tx = state.db.begin().await?;
    let user_id = user.id;
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(user_id)
                    .push(" AND status IN (")
                    .push_bind(STATUS_ACTIVE)
                    .push(", ")
                    .push_bind(STATUS_INVITED)
                    .push(")");
            }),
        )
        .await;
    let affected = sqlx::query("UPDATE users SET status = $2 WHERE id = $1 AND status IN ($3, $4)")
        .bind(user.id)
        .bind(STATUS_DISABLED)
        .bind(STATUS_ACTIVE)
        .bind(STATUS_INVITED)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new().with("status", STATUS_DISABLED),
            json!({ "id": user.id, "status": { "in": [STATUS_ACTIVE, STATUS_INVITED] } }),
        )
        .await;
    if affected == 0 {
        let fresh: Option<i32> = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?;
        return Err(match fresh {
            None => not_found(),
            Some(STATUS_DELETED) => err(StatusCode::CONFLICT, "USER_DELETED", "User is deleted"),
            Some(_) => err(
                StatusCode::CONFLICT,
                "ALREADY_DISABLED",
                "User is already disabled",
            ),
        });
    }
    revoke_all_user_tokens_audited(&mut tx, &audit, user.id).await?;
    // specs/006 R11: keluar dari divisi saat tidak aktif (enable TIDAK memulihkan).
    remove_users_from_divisions_in_tx(&mut tx, &audit, &[user.id], None).await?;
    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.disable",
            before: Some(json!({ "status": status_label(user.status) })),
            after: Some(json!({ "status": "DISABLED" })),
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    after_user_change(&state, user.id, true, true, true).await;
    Ok(created(
        json!({ "success": true, "data": { "uuid": raw_uuid, "status": "DISABLED" } }),
    ))
}

/// Node: src/admin/users/admin-users.controller.ts:118 (`enable`).
pub async fn enable(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = text(&dto, "reason");
    let user = require_user(&state, uuid).await?;

    let mut tx = state.db.begin().await?;
    // Batas kursi MANUAL per company tempat user menjadi anggota (R-18). Urut menaik supaya dua
    // enable bersamaan tak saling deadlock di lock `seats:`.
    let companies: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "companyId" FROM "userCompany"
            WHERE "userId" = $1 AND status IN (1, 2) ORDER BY "companyId" ASC"#,
    )
    .bind(user.id)
    .fetch_all(&mut *tx)
    .await?;
    for company_id in companies {
        assert_manual_seat_available(&mut tx, company_id, 1, Some(user.id)).await?;
    }
    let user_id = user.id;
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(user_id)
                    .push(" AND status = ")
                    .push_bind(STATUS_DISABLED);
            }),
        )
        .await;
    let affected = sqlx::query("UPDATE users SET status = $2 WHERE id = $1 AND status = $3")
        .bind(user.id)
        .bind(STATUS_ACTIVE)
        .bind(STATUS_DISABLED)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new().with("status", STATUS_ACTIVE),
            json!({ "id": user.id, "status": STATUS_DISABLED }),
        )
        .await;
    if affected == 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "NOT_DISABLED",
            "User is not disabled",
        ));
    }
    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.enable",
            before: Some(json!({ "status": "DISABLED" })),
            after: Some(json!({ "status": "ACTIVE" })),
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    after_user_change(&state, user.id, true, true, true).await;
    Ok(created(
        json!({ "success": true, "data": { "uuid": raw_uuid, "status": "ACTIVE" } }),
    ))
}

/// Node: src/admin/users/admin-users.controller.ts:130 (`revokeSessions`).
pub async fn revoke_sessions(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&WRITE_ROLES)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = text(&dto, "reason");
    let user = require_user(&state, uuid).await?;

    let mut tx = state.db.begin().await?;
    revoke_all_user_tokens_audited(&mut tx, &audit, user.id).await?;
    sqlx::query(r#"UPDATE "userConnected" SET status = 0 WHERE "userId" = $1 AND status = 1"#)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    operator_audit(
        &mut tx,
        &ctx,
        &op,
        uuid,
        AuditArgs {
            action: "user.revoke_sessions",
            before: None,
            after: None,
            reason: Some(reason),
        },
    )
    .await?;
    tx.commit().await?;
    after_user_change(&state, user.id, true, false, true).await;
    Ok(created(
        json!({ "success": true, "data": { "uuid": raw_uuid } }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_round_trip() {
        for (n, l) in [
            (1, "ACTIVE"),
            (2, "INVITED"),
            (3, "DISABLED"),
            (0, "DELETED"),
        ] {
            assert_eq!(status_label(n), l);
            assert_eq!(status_number(l), Some(n));
        }
        assert_eq!(status_label(9), "9");
        assert_eq!(status_number("x"), None);
    }

    #[test]
    fn effective_role_follows_is_system() {
        assert_eq!(effective_role("ADMIN", true, None), "ADMIN");
        assert_eq!(effective_role("c1_x", false, Some("MANAGER")), "MANAGER");
        assert_eq!(effective_role("c1_x", false, None), "c1_x");
    }
}
