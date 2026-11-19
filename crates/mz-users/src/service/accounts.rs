//! `UsersService`: `findAll` (:664), `findOneByUUID` (:1168), `update` (:1389), `remove` (:1525),
//! `deleteAccount` (:1590).

use mz_account::assignable_role::{
    AssignableRoleActor, RoleRef, assert_target_within_rank, require_actor_role,
    resolve_assignable_role,
};
use mz_account::division_membership::remove_users_from_divisions_in_tx;
use mz_account::password::verify_password;
use mz_account::target_user::{
    SharedAccountDto, assert_no_shared_account_change, require_company_member,
};
use mz_account::user_ops::revoke_all_user_tokens_audited;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::find_company_id;
use mz_core::enums::{BaseRole, effective_role_code, sees_whole_company};
use mz_core::jwt::revoke_all_user_tokens;
use mz_core::time::JsDate;
use mz_core::user_status::STATUS_DELETED;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::dto::PaginatorQuery;
use crate::model::{
    ASSIGNMENT_SELECT, AssignmentRow, COMPANY_ADMIN_CODES, PagingNan, UpdateUserInput,
    is_token_revoking_status, resolve_paging, role_code_filter_base_roles, with_effective_role,
};
use mz_account::anti_lockout::{begin_guard, finish_guard, lock_company_row};
use mz_core::invalidation::forget_user_me;

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

// ── GET /users ───────────────────────────────────────────────────────────────

enum Scope {
    /// `seesWholeCompany`: anggota company (opsional dicari `search`), bukan yang dihapus.
    Company {
        company_id: i32,
        search: Option<String>,
    },
    /// Selain itu: hanya diri sendiri.
    Myself { user_id: i32 },
}

const COMPANY_MEMBER: &str =
    r#"EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = u.id AND uc."companyId" = "#;

fn push_scope(qb: &mut QueryBuilder<'_, Postgres>, scope: &Scope) {
    match scope {
        Scope::Myself { user_id } => {
            qb.push("u.id = ").push_bind(*user_id);
        }
        Scope::Company { company_id, search } => {
            // `OR[{username contains & company & status}, {name contains & company & status}]`
            // == `(username ILIKE OR name ILIKE) AND company AND status`.
            if let Some(term) = search {
                let pattern = format!("%{term}%");
                qb.push("(u.username ILIKE ")
                    .push_bind(pattern.clone())
                    .push(" OR u.name ILIKE ")
                    .push_bind(pattern)
                    .push(") AND ");
            }
            qb.push(COMPANY_MEMBER)
                .push_bind(*company_id)
                .push(") AND u.status <> ")
                .push_bind(STATUS_DELETED);
        }
    }
}

fn sort_column_sql(column: &str) -> &'static str {
    match column {
        "id" => "u.id",
        "name" => "u.name",
        "username" => "u.username",
        "updatedAt" => r#"u."updatedAt""#,
        "status" => "u.status",
        _ => r#"u."createdAt""#,
    }
}

#[derive(sqlx::FromRow)]
struct ListRow {
    id: i32,
    username: String,
    name: String,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    #[sqlx(rename = "sentAt")]
    sent_at: Option<JsDate>,
    status: i32,
    uuid: String,
}

/// `UsersService.findAll(query, userId)` -> `{ success, message, data: { total, itemsPerPage, pageNumber, result } }`.
pub async fn find_all(state: &AppState, user_id: i32, query: &PaginatorQuery) -> ApiResult<Value> {
    // Pelaku: penugasan TERBARU (id desc, semua company/status) dan company aktif. Node melempar
    // TypeError (-> 500) kalau salah satunya tidak ada.
    let latest: Option<(Option<String>, Option<bool>, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let company_id = find_company_id(&state.db, user_id).await?;
    let (Some((Some(code), is_system, base_role)), Some(company_id)) = (latest, company_id) else {
        return Err(internal("findAll: actor has no access level or company"));
    };
    let role = effective_role_code(
        &code,
        is_system.unwrap_or(false),
        base_role
            .as_deref()
            .and_then(|b| b.parse::<BaseRole>().ok()),
    );
    let scope = if sees_whole_company(Some(&role)) {
        Scope::Company {
            company_id,
            search: query.search.clone().filter(|s| !s.is_empty()),
        }
    } else {
        Scope::Myself { user_id }
    };

    let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM users u WHERE ");
    push_scope(&mut count_qb, &scope);
    let total: i64 = count_qb.build_query_scalar().fetch_one(&state.db).await?;

    let paging = resolve_paging(
        query.page_number_value(),
        query.items_per_page_value(),
        query.order_by.as_deref(),
        query.order_column.as_deref(),
        total,
    )
    .map_err(|PagingNan| internal("findAll: NaN take/skip"))?;
    let offset = paging
        .offset()
        .ok_or_else(|| internal("findAll: negative skip"))?;

    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT u.id, u.username, u.name, u."createdAt", u."updatedAt", u."sentAt", u.status,
                  u.uuid::text AS "uuid" FROM users u WHERE "#,
    );
    push_scope(&mut qb, &scope);
    let dir = if paging.desc { "DESC" } else { "ASC" };
    qb.push(" ORDER BY ")
        .push(sort_column_sql(paging.order_column))
        .push(" ")
        .push(dir)
        .push(", u.id ")
        .push(dir)
        .push(" LIMIT ")
        .push_bind(paging.items_per_page)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows: Vec<ListRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let assignments: Vec<AssignmentRow> = sqlx::query_as(&format!(
        r#"{ASSIGNMENT_SELECT} WHERE ual."userId" = ANY($1) ORDER BY ual.id"#
    ))
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;

    let result: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mine: Vec<AssignmentRow> = assignments
                .iter()
                .filter(|a| a.user_id == r.id)
                .cloned()
                .collect();
            let mut base = Map::new();
            base.insert("id".into(), json!(r.id));
            base.insert("username".into(), json!(r.username));
            base.insert("name".into(), json!(r.name));
            base.insert("createdAt".into(), json!(r.created_at));
            base.insert("updatedAt".into(), json!(r.updated_at));
            base.insert("sentAt".into(), json!(r.sent_at));
            base.insert("status".into(), json!(r.status));
            base.insert("uuid".into(), json!(r.uuid));
            with_effective_role(base, &mine, company_id)
        })
        .collect();

    Ok(json!({
        "success": true,
        "message": "Success get all data users",
        "data": {
            "total": total,
            "itemsPerPage": paging.items_per_page,
            "pageNumber": paging.page_number,
            "result": result,
        },
    }))
}

// ── GET /users/:uuid ─────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct MembershipSettings {
    #[sqlx(rename = "isAllowEditTime")]
    is_allow_edit_time: bool,
    #[sqlx(rename = "isAllowManualTime")]
    is_allow_manual_time: bool,
    #[sqlx(rename = "isBlur")]
    is_blur: bool,
    #[sqlx(rename = "idleTimeInSecond")]
    idle_time_in_second: i32,
    #[sqlx(rename = "screenshotPerHour")]
    screenshot_per_hour: i32,
    #[sqlx(rename = "isScreenshotActive")]
    is_screenshot_active: bool,
}

/// `UsersService.findOneByUUID(uuid, requesterId)` — `requireCompanyMember` (404 di luar company).
pub async fn find_one_by_uuid(state: &AppState, uuid: &str, requester_id: i32) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, uuid).await?;
    let user = member.user;

    // Seluruh keanggotaan target (semua company), persis seperti `select` Node tanpa `where`.
    let memberships: Vec<MembershipSettings> = sqlx::query_as(
        r#"SELECT "isAllowEditTime", "isAllowManualTime", "isBlur", "idleTimeInSecond",
                  "screenshotPerHour", "isScreenshotActive"
           FROM "userCompany" WHERE "userId" = $1 ORDER BY id"#,
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let assignments: Vec<AssignmentRow> = sqlx::query_as(&format!(
        r#"{ASSIGNMENT_SELECT} WHERE ual."userId" = $1 ORDER BY ual.id"#
    ))
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;

    let mut base = Map::new();
    base.insert("id".into(), json!(user.id));
    base.insert("username".into(), json!(user.username));
    base.insert("name".into(), json!(user.name));
    base.insert("createdAt".into(), json!(user.created_at));
    base.insert("updatedAt".into(), json!(user.updated_at));
    base.insert("sentAt".into(), json!(user.sent_at));
    base.insert("status".into(), json!(user.status));
    base.insert("uuid".into(), json!(user.uuid));
    base.insert(
        "userCompany".into(),
        Value::Array(
            memberships
                .iter()
                .map(|m| {
                    json!({
                        "isAllowEditTime": m.is_allow_edit_time,
                        "isAllowManualTime": m.is_allow_manual_time,
                        "isBlur": m.is_blur,
                        "idleTimeInSecond": m.idle_time_in_second,
                        "screenshotPerHour": m.screenshot_per_hour,
                        "isScreenshotActive": m.is_screenshot_active,
                    })
                })
                .collect(),
        ),
    );
    Ok(json!({
        "success": true,
        "message": format!("Success get data users by uuid {uuid}"),
        "data": with_effective_role(base, &assignments, member.company_id),
    }))
}

// ── PATCH /users/:uuid ───────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct UpdatedUser {
    id: i32,
    username: String,
    name: String,
    status: i32,
    uuid: String,
}

/// `UsersService.update(uuid, dto, requesterId)`.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    input: &UpdateUserInput,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, uuid).await?;
    let (user, company_id) = (member.user, member.company_id);

    // Aturan C: jabatan target SAAT INI tidak boleh di atas pelaku.
    let actor = {
        let mut conn = state.db.acquire().await?;
        let actor = require_actor_role(&mut conn, requester_id, company_id).await?;
        assert_target_within_rank(&mut conn, &actor, user.id, company_id).await?;
        actor
    };
    assert_no_shared_account_change(
        &state.db,
        requester_id,
        &user,
        company_id,
        &SharedAccountDto {
            username: Some(&input.username),
            name: Some(&input.name),
            status: Some(input.status),
        },
    )
    .await?;
    // Aturan B: role BARU tidak boleh di atas pelaku (422).
    let access_level = {
        let mut conn = state.db.acquire().await?;
        resolve_assignable_role(
            &mut conn,
            &AssignableRoleActor::Actor(actor),
            company_id,
            RoleRef {
                code: Some(&input.access_level),
                uuid: None,
            },
        )
        .await?
    };

    // §9.6/§9.8: cek + tulis satu transaksi ber-lock baris company. Rollback (mis. penurunan admin
    // terakhir ditolak) tidak boleh meninggalkan baris audit: `discardAudit()` di jalur galat.
    let mut tx = state.db.begin().await?;
    let discard = audit.discard_guard();
    let before = begin_guard(&mut tx, company_id).await?;

    // Audit `users.update` (hasil tanpa `select`; `userAccessLevel`/`userCompany` di bawah adalah
    // nested write Prisma yang TIDAK dilihat middleware audit Node, jadi tanpa baris sendiri).
    let pre = audit
        .pre_read(Model::Users, Lookup::Uuid(uuid.to_owned()))
        .await;
    let updated = sqlx::query_as::<_, UpdatedUser>(
        r#"UPDATE users SET username = $2, name = $3, status = $4, "updatedAt" = $5
           WHERE uuid = $1::uuid
           RETURNING id, username, name, status, uuid::text AS "uuid""#,
    )
    .bind(uuid)
    .bind(&input.username)
    .bind(&input.name)
    .bind(input.status)
    .bind(JsDate::now())
    .fetch_one(&mut *tx)
    .await?;
    audit
        .record_update(
            Model::Users,
            pre,
            &Row::new()
                .with("id", updated.id)
                .with("uuid", updated.uuid.as_str())
                .with("username", updated.username.as_str())
                .with("name", updated.name.as_str())
                .with("status", updated.status),
        )
        .await;

    // `companyId` wajib: tanpanya role/setelan user ini di SEMUA company ikut tertimpa (§10.2a).
    sqlx::query(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = $3 WHERE "userId" = $1 AND "companyId" = $2"#,
    )
    .bind(user.id)
    .bind(company_id)
    .bind(access_level.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"UPDATE "userCompany"
           SET "screenshotPerHour" = $3, "idleTimeInSecond" = $4, "isBlur" = $5,
               "isScreenshotActive" = $6, "isAllowManualTime" = false, "isAllowEditTime" = false
           WHERE "userId" = $1 AND "companyId" = $2"#,
    )
    .bind(user.id)
    .bind(company_id)
    .bind(input.screenshot_per_hour)
    .bind(input.idle_time_in_second)
    .bind(input.is_blur)
    .bind(input.is_screenshot_active)
    .execute(&mut *tx)
    .await?;

    if is_token_revoking_status(Some(input.status)) {
        revoke_all_user_tokens(&mut tx, updated.id).await?;
        // specs/006 R11: keluar dari divisi saat tidak aktif (enable TIDAK memulihkan).
        remove_users_from_divisions_in_tx(&mut tx, audit, &[updated.id], None).await?;
    }
    finish_guard(&mut tx, company_id, &before).await?;
    tx.commit().await?;
    discard.disarm();

    // Jabatan baru harus berlaku seketika (bukan setelah TTL 60 dtk), dan jabatan lama yang lebih
    // tinggi tidak boleh hidup lebih lama. SESUDAH commit.
    state.invalidate_user_access(user.id);
    forget_user_me(state, user.id).await;
    state.invalidate_token_revocation(user.id);

    Ok(json!({
        "success": true,
        "message": "update users success",
        "data": {
            "id": updated.id,
            "username": updated.username,
            "name": updated.name,
            "status": updated.status,
            "uuid": updated.uuid,
        },
    }))
}

// ── DELETE /users/:uuid ──────────────────────────────────────────────────────

/// `UsersService.remove(uuid, requesterId)` — soft delete + cabut token.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, uuid).await?;
    let (user, company_id) = (member.user, member.company_id);
    assert_no_shared_account_change(
        &state.db,
        requester_id,
        &user,
        company_id,
        &SharedAccountDto {
            status: Some(STATUS_DELETED),
            ..Default::default()
        },
    )
    .await?;

    // §9.6: menonaktifkan admin terakhir mengunci company sama telaknya dengan menurunkannya.
    let mut tx = state.db.begin().await?;
    let before = begin_guard(&mut tx, company_id).await?;
    let pre = audit
        .pre_read(Model::Users, Lookup::Uuid(uuid.to_owned()))
        .await;
    let deleted_id: i32 =
        sqlx::query_scalar("UPDATE users SET status = $2 WHERE uuid = $1::uuid RETURNING id")
            .bind(uuid)
            .bind(STATUS_DELETED)
            .fetch_one(&mut *tx)
            .await?;
    // `status → 0` = tanda tangan soft delete `employee` → baris `delete` (bisa dipulihkan).
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Users, Lookup::Id(deleted_id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    // Dicabut juga, bukan hanya status: kalau akun diaktifkan lagi, token lama tidak hidup kembali.
    revoke_all_user_tokens_audited(&mut tx, audit, deleted_id).await?;
    // `users.status` dan `userCompany.status` tidak otomatis sinkron.
    deactivate_memberships(&mut tx, audit, deleted_id).await?;
    remove_users_from_divisions_in_tx(&mut tx, audit, &[deleted_id], None).await?;
    finish_guard(&mut tx, company_id, &before).await?;
    tx.commit().await?;

    forget_user_me(state, user.id).await;
    state.invalidate_token_revocation(user.id);

    Ok(json!({
        "success": true,
        "message": "delete users success",
        "data": { "isDeleted": true },
    }))
}

/// `tx.userCompany.updateMany({ where: { userId, status: 1 }, data: { status: 0 } })` + audit-nya
/// (`status → 0` pada pendamping `employee` = baris `delete` ber-`subEntity` `companyMembership`).
async fn deactivate_memberships(
    conn: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    user_id: i32,
) -> ApiResult<()> {
    let pre = audit
        .pre_read_many(
            Model::UserCompany,
            Lookup::filter(move |qb| {
                qb.push(r#""userId" = "#).push_bind(user_id);
                qb.push(" AND status = 1");
            }),
        )
        .await;
    let affected =
        sqlx::query(r#"UPDATE "userCompany" SET status = 0 WHERE "userId" = $1 AND status = 1"#)
            .bind(user_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    audit
        .record_update_many(
            Model::UserCompany,
            pre,
            affected,
            &Row::new().with("status", 0),
            json!({ "userId": user_id, "status": 1 }),
        )
        .await;
    Ok(())
}

// ── POST /users/delete-account ───────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct DeleteAccountUser {
    id: i32,
    uuid: String,
    username: String,
    name: String,
    password: String,
    status: i32,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
}

#[derive(sqlx::FromRow)]
struct AdminRoleRow {
    #[sqlx(rename = "companyId")]
    company_id: i32,
    code: Option<String>,
    #[sqlx(rename = "isSystem")]
    is_system: Option<bool>,
    #[sqlx(rename = "baseRole")]
    base_role: Option<String>,
}

enum DeleteOutcome {
    Failed(&'static str),
    Done { already_deleted: bool },
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

fn deleted_data(user: &DeleteAccountUser, deleted_at: Option<JsDate>, status: i32) -> Value {
    json!({
        "uuid": user.uuid,
        "username": user.username,
        "name": user.name,
        "status": status,
        "isDeleted": true,
        "deletedAt": deleted_at,
    })
}

/// `UsersService.deleteAccount(userId, password)` — self-service soft delete.
pub async fn delete_account(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    password: &str,
) -> ApiResult<Value> {
    let user = sqlx::query_as::<_, DeleteAccountUser>(
        r#"SELECT id, uuid::text AS "uuid", username, name, password, status, "updatedAt"
           FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(user) = user else {
        return Ok(failure("User not found"));
    };

    // Idempotent: klien yang retry tidak perlu menangani galat khusus.
    if user.status == STATUS_DELETED {
        return Ok(json!({
            "success": true,
            "message": "account already deleted",
            "data": deleted_data(&user, user.updated_at, user.status),
        }));
    }

    if !verify_password(password, &user.password).await {
        return Ok(failure("Password is incorrect"));
    }

    let company_id = find_company_id(&state.db, user.id).await?;
    let roles: Vec<AdminRoleRow> = sqlx::query_as(
        r#"SELECT ual."companyId", al.code, al."isSystem", al."baseRole"::text AS "baseRole"
           FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual.status = 1"#,
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let is_company_admin = company_id.is_some_and(|company| {
        roles.iter().any(|r| {
            r.company_id == company
                && r.code.as_deref().is_some_and(|code| {
                    let effective = effective_role_code(
                        code,
                        r.is_system.unwrap_or(false),
                        r.base_role
                            .as_deref()
                            .and_then(|b| b.parse::<BaseRole>().ok()),
                    );
                    COMPANY_ADMIN_CODES.contains(&effective.as_str())
                })
        })
    });

    let deleted_at = JsDate::now();
    // §9.1: transaksi yang rollback tidak meninggalkan baris audit (`discardAudit()`).
    let outcome = match run_delete(
        state,
        audit,
        &user,
        company_id,
        is_company_admin,
        deleted_at,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            audit.discard();
            return Err(err);
        }
    };
    let already_deleted = match outcome {
        DeleteOutcome::Failed(message) => return Ok(failure(message)),
        DeleteOutcome::Done { already_deleted } => already_deleted,
    };

    forget_user_me(state, user.id).await;
    // Hanya bila transaksi benar-benar menulis.
    if !already_deleted {
        state.invalidate_token_revocation(user.id);
    }

    Ok(json!({
        "success": true,
        "message": if already_deleted { "account already deleted" } else { "delete account success" },
        "data": deleted_data(&user, Some(deleted_at), STATUS_DELETED),
    }))
}

async fn run_delete(
    state: &AppState,
    audit: &AuditHandle,
    user: &DeleteAccountUser,
    company_id: Option<i32>,
    is_company_admin: bool,
    deleted_at: JsDate,
) -> ApiResult<DeleteOutcome> {
    // Guard admin terakhir + penurunan status + revoke device harus jadi atau batal bersama.
    let mut tx = state.db.begin().await?;
    // §9.8: dua admin terakhir yang menghapus akunnya bersamaan diserialkan oleh lock ini.
    if let Some(company) = company_id {
        lock_company_row(&mut tx, company).await?;
    }

    let current: Option<i32> = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(current) = current else {
        return Ok(DeleteOutcome::Failed("User not found"));
    };
    if current == STATUS_DELETED {
        return Ok(DeleteOutcome::Done {
            already_deleted: true,
        });
    }

    if let (true, Some(company)) = (is_company_admin, company_id) {
        let other_members: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
               WHERE uc."companyId" = $1 AND uc.status = 1 AND uc."userId" <> $2 AND u.status <> $3"#,
        )
        .bind(company)
        .bind(user.id)
        .bind(STATUS_DELETED)
        .fetch_one(&mut *tx)
        .await?;

        // §10.2: `roleCodeFilter`, bukan `code IN (...)` mentah — admin ber-role custom tetap dihitung.
        let base_roles = role_code_filter_base_roles(&COMPANY_ADMIN_CODES);
        let other_admins: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "userAccessLevel" ual
               JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               JOIN users u ON u.id = ual."userId"
               WHERE ual."companyId" = $1 AND ual.status = 1 AND ual."userId" <> $2
                 AND u.status <> $3
                 AND ((al."isSystem" = true AND al.code = ANY($4))
                      OR (al."isSystem" = false AND al."baseRole"::text = ANY($5)))"#,
        )
        .bind(company)
        .bind(user.id)
        .bind(STATUS_DELETED)
        .bind(COMPANY_ADMIN_CODES.to_vec())
        .bind(base_roles)
        .fetch_one(&mut *tx)
        .await?;

        // Satu-satunya anggota boleh hapus; yang diblokir: masih ada anggota lain tanpa admin.
        if other_members > 0 && other_admins == 0 {
            return Ok(DeleteOutcome::Failed(
                "You are the last admin of this company. Assign another admin before deleting your account.",
            ));
        }
    }

    let pre = audit.pre_read(Model::Users, Lookup::Id(user.id)).await;
    sqlx::query(r#"UPDATE users SET status = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(user.id)
        .bind(STATUS_DELETED)
        .bind(deleted_at)
        .execute(&mut *tx)
        .await?;
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Users, Lookup::Id(user.id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    // Sama seperti admin `remove()`: `userCompany.status` tidak ikut mati otomatis.
    deactivate_memberships(&mut tx, audit, user.id).await?;
    remove_users_from_divisions_in_tx(&mut tx, audit, &[user.id], None).await?;
    // Putus semua device aktif supaya token yang dipegang klien tidak lolos cek device.
    sqlx::query(
        r#"UPDATE "userConnected" SET status = 0, "lastConnectedAt" = $2 WHERE "userId" = $1 AND status = 1"#,
    )
    .bind(user.id)
    .bind(deleted_at)
    .execute(&mut *tx)
    .await?;
    // Token lama tidak boleh hidup lagi bila akun dipulihkan kemudian.
    revoke_all_user_tokens_audited(&mut tx, audit, user.id).await?;
    tx.commit().await?;
    Ok(DeleteOutcome::Done {
        already_deleted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_columns_are_whitelisted_sql() {
        assert_eq!(sort_column_sql("createdAt"), r#"u."createdAt""#);
        assert_eq!(sort_column_sql("name"), "u.name");
        assert_eq!(
            sort_column_sql("password; drop table users"),
            r#"u."createdAt""#
        );
    }

    #[test]
    fn scope_sql_search_and_self() {
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_scope(
            &mut qb,
            &Scope::Company {
                company_id: 7,
                search: Some("bud".into()),
            },
        );
        let sql = qb.sql().to_owned();
        assert!(sql.starts_with("(u.username ILIKE $1 OR u.name ILIKE $2) AND EXISTS"));
        assert!(sql.ends_with("AND u.status <> $4"));

        let mut qb = QueryBuilder::<Postgres>::new("");
        push_scope(&mut qb, &Scope::Myself { user_id: 3 });
        assert_eq!(qb.sql(), "u.id = $1");

        let mut qb = QueryBuilder::<Postgres>::new("");
        push_scope(
            &mut qb,
            &Scope::Company {
                company_id: 7,
                search: None,
            },
        );
        assert!(qb.sql().starts_with("EXISTS"));
    }

    #[test]
    fn deleted_payload_shape() {
        let user = DeleteAccountUser {
            id: 1,
            uuid: "u".into(),
            username: "a@b.co".into(),
            name: "A".into(),
            password: String::new(),
            status: 1,
            updated_at: None,
        };
        let v = deleted_data(&user, None, 0);
        assert_eq!(v["isDeleted"], json!(true));
        assert_eq!(v["status"], json!(0));
        assert_eq!(v["deletedAt"], Value::Null);
        assert_eq!(
            failure("x"),
            json!({"success": false, "message": "x", "data": null})
        );
    }
}
