//! Controller `admin` — akun operator Timemoz: login & lupa password (pra-login), daftar/detail/CRUD
//! operator. Node: `src/admin/admin.controller.ts` + `src/admin/admin.service.ts`.
//!
//! Semua POST tanpa `@HttpCode` → 201. Semua handler berwenang lewat extractor [`Operator`]
//! (`JwtAdminAuthGuard`); `OperatorRolesGuard` dipanggil di awal handler (sebelum validasi).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_account::password::{INVALID_CREDENTIALS_MESSAGE, compare_password_safely, hash_password};
use mz_account::util::{encode_uri_component, env_or_undefined};
use mz_auth::dto::{ForgotPasswordDto, UsernameDto, text as dto_text};
use mz_auth::login_attempts::{LOGIN_LOCKED_MESSAGE, Scope, consume, reset};
use mz_core::db::{advisory_xact_lock, is_unique_violation};
use mz_core::extract::ValidatedJson;
use mz_core::jwt::sign;
use mz_core::mail::{Mail, send};
use mz_core::time::{JsDate, now_naive};
use mz_core::user_status::RESET_TOKEN_USED;
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{
    CREATE_ADMIN, LOGIN, Paginator, REASON, RawBody, RawQuery, UPDATE_ADMIN, opt_text, run_body,
    text, validate_paginator,
};

/// Kolom `OPERATOR_PUBLIC_SELECT` (tanpa `password`).
const PUBLIC_COLUMNS: &str =
    r#"id, uuid::text AS uuid, username, name, role::text AS role, status"#;
const STATUS_ACTIVE: i32 = 1;

/// Baris `operators` tanpa `password` (urutan kunci JSON = urutan `select` Node).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OperatorRow {
    pub id: i32,
    pub uuid: String,
    pub username: String,
    pub name: String,
    pub role: String,
    pub status: i32,
}

impl OperatorRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "uuid": self.uuid,
            "username": self.username,
            "name": self.name,
            "role": self.role,
            "status": self.status,
        })
    }
}

fn created(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(v))
}

fn username_taken() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "USERNAME_TAKEN",
        "Username is already in use",
    )
}

fn cannot_remove_self() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "CANNOT_REMOVE_SELF",
        "You cannot deactivate your own account",
    )
}

fn not_found(uuid: &str) -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "OPERATOR_NOT_FOUND",
        format!("Admin with uuid {uuid} not found"),
    )
}

/// `isUniqueViolation` (P2002) → `USERNAME_TAKEN`.
fn map_unique(e: sqlx::Error) -> ApiError {
    if is_unique_violation(&e) {
        username_taken()
    } else {
        e.into()
    }
}

// ── Pra-login ───────────────────────────────────────────────────────────────

/// Node: src/admin/admin.controller.ts:97 (`login`) → `AdminService.login` (:67).
pub async fn login(
    State(state): State<AppState>,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = run_body(&body, &[LOGIN], false)?;
    let (username, password) = (text(&dto, "username"), text(&dto, "password"));

    // Jatah percobaan per username, dicatat sebelum bcrypt (H3). Bucket terpisah dari login user.
    if !consume(&state, Scope::Operator, username).await {
        return Ok(created(
            json!({ "success": false, "message": LOGIN_LOCKED_MESSAGE }),
        ));
    }

    type LoginRow = (i32, String, String, String, i32, String, String);
    let admin: Option<LoginRow> = sqlx::query_as(
        r#"SELECT id, password, username, name, status, role::text, uuid::text
             FROM operators WHERE username = $1"#,
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?;

    // Password dicek dulu (biaya waktu sama untuk operator tak terdaftar); status nonaktif hanya
    // diberitahukan ke pemilik password.
    let valid =
        compare_password_safely(&state, password, admin.as_ref().map(|a| a.1.as_str())).await;
    let Some((id, _, admin_username, name, status, role, uuid)) = admin.filter(|_| valid) else {
        return Ok(created(
            json!({ "success": false, "message": INVALID_CREDENTIALS_MESSAGE }),
        ));
    };
    reset(&state, Scope::Operator, username).await;

    if status == 0 {
        return Ok(created(
            json!({ "success": false, "message": "Admin account is inactive" }),
        ));
    }

    let mut payload = Map::new();
    payload.insert("username".into(), Value::String(admin_username.clone()));
    payload.insert("sub".into(), Value::from(id));
    let token = sign(&state, payload)?;
    Ok(created(json!({
        "success": true,
        "message": "Admin login successful",
        "data": {
            "token": token,
            "id": id,
            "uuid": uuid,
            "username": admin_username,
            "name": name,
            "role": role,
        },
    })))
}

/// Node: src/admin/admin.controller.ts:105 (`forgotPassword`) → `AdminService.forgotPassword` (:107).
pub async fn forgot_password(
    State(state): State<AppState>,
    ValidatedJson(dto): ValidatedJson<ForgotPasswordDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let failure = json!({ "success": false, "message": "Invalid or expired token" });
    let (forgot_uuid, password, username) = (
        dto_text(&dto.forgot_password_uuid),
        dto_text(&dto.password),
        dto_text(&dto.username),
    );
    let operator_id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM operators WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username)
            .fetch_optional(&state.db)
            .await?;
    let Some(operator_id) = operator_id else {
        return Ok(created(failure));
    };
    // Hash dihitung SEBELUM transaksi (bcrypt lambat).
    let hashed = hash_password(&state, password).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    // Klaim atomik: satu UPDATE bersyarat (dua request bersamaan tak bisa sama-sama memakai token).
    let claim = sqlx::query(
        r#"UPDATE "operatorForgotPassword" SET status = $4, "updatedAt" = $3
            WHERE uuid = $1::uuid AND "operatorId" = $2 AND status = 0 AND "expiredAt" > $3"#,
    )
    .bind(forgot_uuid)
    .bind(operator_id)
    .bind(now)
    .bind(RESET_TOKEN_USED)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claim != 1 {
        tx.rollback().await?;
        return Ok(created(failure));
    }
    sqlx::query("UPDATE operators SET password = $2 WHERE id = $1")
        .bind(operator_id)
        .bind(&hashed)
        .execute(&mut *tx)
        .await?;
    // Token lain milik operator yang sama yang masih berlaku ikut dicabut.
    sqlx::query(
        r#"UPDATE "operatorForgotPassword" SET status = $3, "updatedAt" = $2
            WHERE "operatorId" = $1 AND status IN (0, 1)"#,
    )
    .bind(operator_id)
    .bind(now)
    .bind(RESET_TOKEN_USED)
    .execute(&mut *tx)
    .await?;
    type ResetRow = (i32, String, String, JsDate, Option<JsDate>, i32, String);
    let data: Option<ResetRow> = sqlx::query_as(
        r#"SELECT id, username, name, "createdAt", "updatedAt", status, uuid::text
             FROM operators WHERE id = $1"#,
    )
    .bind(operator_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let data = data.map(
        |(id, username, name, created_at, updated_at, status, uuid)| {
            json!({
                "id": id,
                "username": username,
                "name": name,
                "createdAt": created_at,
                "updatedAt": updated_at,
                "status": status,
                "uuid": uuid,
            })
        },
    );
    Ok(created(json!({
        "success": true,
        "message": "Reset password success",
        "data": data,
    })))
}

/// Node: src/admin/admin.controller.ts:112 (`sendEmailForgotPassword`) →
/// `AdminService.sendEmailForgotPassword` (:171). Respons identik untuk semua hasil (bukan oracle
/// enumerasi username).
pub async fn forgot_password_generate(
    State(state): State<AppState>,
    ValidatedJson(dto): ValidatedJson<UsernameDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let generic = json!({
        "success": true,
        "message": "If the email is registered, a password reset link has been sent.",
    });
    let username = dto_text(&dto.username);
    let found: Option<(i32, String)> =
        sqlx::query_as("SELECT id, name FROM operators WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username)
            .fetch_optional(&state.db)
            .await?;
    let Some((operator_id, name)) = found else {
        return Ok(created(generic));
    };

    // `parseInt(process.env.EXPIRED_AT)`; NaN di Node berakhir 500 di Prisma.
    let minutes: i64 = state
        .config
        .var("EXPIRED_AT")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("EXPIRED_AT is not a valid integer")))?;
    let now = now_naive();
    let expired_at = now + chrono::Duration::minutes(minutes);

    // Token yang belum dikirim (status 1) dan belum kedaluwarsa: tidak membuat baru.
    let pending: Option<Option<JsDate>> = sqlx::query_scalar(
        r#"SELECT "expiredAt" FROM "operatorForgotPassword"
            WHERE "operatorId" = $1 AND status = $2 ORDER BY id LIMIT 1"#,
    )
    .bind(operator_id)
    .bind(STATUS_ACTIVE)
    .fetch_optional(&state.db)
    .await?;
    if let Some(Some(expiry)) = pending
        && expiry.0 > now_naive()
    {
        return Ok(created(generic));
    }

    // Node memakai satu objek Date untuk `createdAt` dan `expiredAt` (dimutasi `setMinutes`), jadi
    // `createdAt` = `expiredAt`; di sini `createdAt` = sekarang (lihat deviasi).
    let token: String = sqlx::query_scalar(
        r#"INSERT INTO "operatorForgotPassword" ("operatorId", "createdAt", status, "expiredAt")
           VALUES ($1, $2, $3, $4) RETURNING uuid::text"#,
    )
    .bind(operator_id)
    .bind(now)
    .bind(STATUS_ACTIVE)
    .bind(expired_at)
    .fetch_one(&state.db)
    .await?;

    // Event 'forgot.operator.password' → listener → MailService (fire-and-forget di Node).
    let (state, username) = (state.clone(), username.to_owned());
    mz_core::bg::spawn(async move {
        send_operator_reset_mail(&state, &username, &name, &token).await;
    });
    Ok(created(generic))
}

/// `MailService.sendEmailForgotPasswordOperator(user, token)`: kirim hanya bila token masih
/// berstatus 1; lalu tandai `sentAt = now, status = 0` (bisa dipakai). Galat hanya dilog.
async fn send_operator_reset_mail(state: &AppState, username: &str, name: &str, token: &str) {
    let url = format!(
        "{}/forgot-password-operator?token={token}&username={}",
        env_or_undefined(state.config.var("HOST_FE")),
        encode_uri_component(username)
    );
    let found: Result<Option<i32>, sqlx::Error> = sqlx::query_scalar(
        r#"SELECT id FROM "operatorForgotPassword" WHERE uuid = $1::uuid AND status = 1 LIMIT 1"#,
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await;
    match found {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(err) => {
            tracing::error!(error = %err, "sendEmailForgotPasswordOperator: query gagal");
            return;
        }
    }
    if let Err(err) = sqlx::query(
        r#"UPDATE "operatorForgotPassword" SET "sentAt" = $2, status = 0 WHERE uuid = $1::uuid"#,
    )
    .bind(token)
    .bind(now_naive())
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %err, "sendEmailForgotPasswordOperator: update gagal");
        return;
    }
    let _ = send(
        state,
        Mail {
            to_email: username,
            to_name: name,
            subject: "Reset Password",
            text: None,
            template: "reset-password",
            context: json!({ "name": name, "url": url }),
            category: "Reset Password",
            label: "send email forgot password operator",
        },
    )
    .await;
}

// ── Daftar & detail ─────────────────────────────────────────────────────────

/// `parseInt(String(n), 10)` untuk angka JS (`n` bulat dari `parseInt`): `>= 1e21` ditulis JS dalam
/// notasi eksponen sehingga yang terbaca hanya angka depan mantisa.
fn js_parse_int_of_number(n: f64) -> f64 {
    if n.abs() < 1e21 {
        return n.trunc();
    }
    let s = format!("{n:e}");
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.as_str()),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let v: f64 = digits.parse().unwrap_or(0.0);
    if neg { -v } else { v }
}

fn failure_list() -> Value {
    json!({ "success": false, "message": "Error retrieving admins", "data": null })
}

/// Node: src/admin/admin.controller.ts:123 (`findAll`) → `AdminService.findAll` (:261).
/// Seluruh galat (termasuk DB) ditangkap Node → `{ success: false, ... }`.
pub async fn find_all(
    State(state): State<AppState>,
    _op: Operator,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    let Paginator {
        page_number,
        items_per_page,
        search,
    } = validate_paginator(&q)?;
    let page = page_number
        .map(js_parse_int_of_number)
        .filter(|n| *n != 0.0)
        .unwrap_or(1.0)
        .max(1.0);
    let per_page = items_per_page
        .map(js_parse_int_of_number)
        .filter(|n| *n != 0.0)
        .unwrap_or(10.0)
        .clamp(1.0, 100.0);
    let skip = (page - 1.0) * per_page;
    if skip >= 9.0e18 {
        // Prisma gagal memuat `skip` di i64 → ditangkap Node.
        return Ok(Json(failure_list()));
    }
    #[allow(clippy::cast_possible_truncation)]
    let (skip, take) = (skip as i64, per_page as i64);
    let pattern = search.filter(|s| !s.is_empty()).map(|s| format!("%{s}%"));

    let result = list_operators(&state, pattern.as_deref(), skip, take).await;
    let (rows, total) = match result {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "GET /admin gagal");
            return Ok(Json(failure_list()));
        }
    };
    #[allow(clippy::cast_possible_truncation)]
    let page_out = page as i64;
    Ok(Json(json!({
        "success": true,
        "data": {
            "total": total,
            "pageNumber": page_out,
            "itemsPerPage": take,
            "results": rows.iter().map(OperatorRow::to_json).collect::<Vec<_>>(),
        },
    })))
}

async fn list_operators(
    state: &AppState,
    pattern: Option<&str>,
    skip: i64,
    take: i64,
) -> Result<(Vec<OperatorRow>, i64), sqlx::Error> {
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        if let Some(p) = pattern {
            qb.push(" WHERE (username ILIKE ")
                .push_bind(p.to_owned())
                .push(" OR name ILIKE ")
                .push_bind(p.to_owned())
                .push(")");
        }
    };
    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT {PUBLIC_COLUMNS} FROM operators"));
    filter(&mut qb);
    qb.push(" ORDER BY id DESC OFFSET ")
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(take);
    let rows = qb
        .build_query_as::<OperatorRow>()
        .fetch_all(&state.db)
        .await?;
    let mut cq = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM operators");
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok((rows, total))
}

/// Node: src/admin/admin.controller.ts:131 (`findOne`) → `AdminService.findByUuid` (:329).
/// `uuid` tak valid → Prisma melempar → ditangkap → `{ success: false }`; tak ada → `data: {}`.
pub async fn find_one(
    State(state): State<AppState>,
    _op: Operator,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    find_one_by(&state, &uuid).await
}

/// `GET /admin/create`: Nest mencocokkan `GET :uuid` (route statis `POST create` tidak
/// menghalangi method lain).
pub async fn find_one_create(
    State(state): State<AppState>,
    _op: Operator,
) -> ApiResult<Json<Value>> {
    find_one_by(&state, "create").await
}

async fn find_one_by(state: &AppState, uuid: &str) -> ApiResult<Json<Value>> {
    let sql = format!("SELECT {PUBLIC_COLUMNS} FROM operators WHERE uuid = $1::uuid LIMIT 1");
    match sqlx::query_as::<_, OperatorRow>(&sql)
        .bind(uuid)
        .fetch_optional(&state.db)
        .await
    {
        Ok(row) => Ok(Json(json!({
            "success": true,
            "data": row.map_or_else(|| json!({}), |r| r.to_json()),
        }))),
        Err(err) => {
            tracing::warn!(error = %err, "GET /admin/:uuid gagal");
            Ok(Json(json!({
                "success": false,
                "message": "Error retrieving admin data",
                "data": null,
            })))
        }
    }
}

// ── Tulis (OWNER) ───────────────────────────────────────────────────────────

/// Node: src/admin/admin.controller.ts:141 (`createAdmin`) → `AdminService.create` (:342).
pub async fn create(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(&[OperatorRole::Owner])?;
    let dto = run_body(&body, &[CREATE_ADMIN], true)?;
    let role = opt_text(&dto, "role").unwrap_or("SUPPORT");
    // Hash dihitung sebelum transaksi dibuka (CPU-bound).
    let hashed = hash_password(&state, text(&dto, "password")).await?;

    let mut tx = state.db.begin().await?;
    let sql = format!(
        r#"INSERT INTO operators (username, name, password, role, status)
           VALUES ($1, $2, $3, $4::"OperatorRole", 1) RETURNING {PUBLIC_COLUMNS}"#
    );
    let row = sqlx::query_as::<_, OperatorRow>(&sql)
        .bind(text(&dto, "username"))
        .bind(text(&dto, "name"))
        .bind(&hashed)
        .bind(role)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_unique)?;
    let after = json!({
        "username": row.username,
        "name": row.name,
        "role": row.role,
        "status": row.status,
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "operator.create",
            target_type: "operator",
            target_uuid: uuid::Uuid::parse_str(&row.uuid).ok(),
            after: Some(&after),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(created(json!({
        "success": true,
        "message": "Admin created successfully",
        "data": row.to_json(),
    })))
}

/// `assertAnotherActiveOwner`.
async fn assert_another_active_owner(
    conn: &mut sqlx::PgConnection,
    target_id: i32,
) -> ApiResult<()> {
    let others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operators WHERE role = 'OWNER' AND status = 1 AND id <> $1",
    )
    .bind(target_id)
    .fetch_one(&mut *conn)
    .await?;
    if others == 0 {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "LAST_OWNER",
            "At least one active OWNER must remain",
        ));
    }
    Ok(())
}

/// `revokeMcpTokens`: cabut kredensial MCP admin milik operator.
async fn revoke_mcp_tokens(conn: &mut sqlx::PgConnection, operator_id: i32) -> sqlx::Result<()> {
    let now = now_naive();
    for table in [
        "mcpAdminAccessToken",
        "mcpAdminOAuthAccessToken",
        "mcpAdminOAuthRefreshToken",
    ] {
        sqlx::query(&format!(
            r#"UPDATE "{table}" SET "revokedAt" = $2 WHERE "operatorId" = $1 AND "revokedAt" IS NULL"#
        ))
        .bind(operator_id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn find_target(
    conn: &mut sqlx::PgConnection,
    uuid: uuid::Uuid,
) -> sqlx::Result<Option<OperatorRow>> {
    let sql = format!("SELECT {PUBLIC_COLUMNS} FROM operators WHERE uuid = $1");
    sqlx::query_as::<_, OperatorRow>(&sql)
        .bind(uuid)
        .fetch_optional(&mut *conn)
        .await
}

/// Node: src/admin/admin.controller.ts:153 (`updateAdmin`) → `AdminService.update` (:381).
pub async fn update(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(&[OperatorRole::Owner])?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[UPDATE_ADMIN], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let new_username = opt_text(&dto, "username");
    let new_name = opt_text(&dto, "name");
    let new_role = opt_text(&dto, "role");
    // `@IsIn([0, 1])` sudah lolos: hanya 0 atau 1.
    let new_status = dto
        .get("status")
        .and_then(Value::as_f64)
        .map(|n| i32::from(n == 1.0));
    let reason = opt_text(&dto, "reason");
    let wants_role_or_status = dto.get("role").is_some_and(|v| !v.is_null())
        || dto.get("status").is_some_and(|v| !v.is_null());

    let mut tx = state.db.begin().await?;
    // Lock dulu sebelum membaca, supaya hitungan OWNER aktif tidak basi (R-4).
    if wants_role_or_status {
        advisory_xact_lock(&mut tx, "operators:roles").await?;
    }
    let Some(target) = find_target(&mut tx, uuid).await? else {
        return Err(not_found(&raw_uuid));
    };

    // Hanya kolom yang benar-benar berubah yang ditulis.
    let username = new_username.filter(|v| *v != target.username);
    let name = new_name.filter(|v| *v != target.name);
    let role = new_role.filter(|v| *v != target.role);
    let status = new_status.filter(|v| *v != target.status);

    let role_or_status_changed = role.is_some() || status.is_some();
    if role_or_status_changed && reason.is_none_or(str::is_empty) {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "REASON_REQUIRED",
            "reason is required when role or status changes",
        ));
    }

    let deactivating = status == Some(0);
    let leaving_owner = target.role == "OWNER"
        && target.status == 1
        && (deactivating || role.is_some_and(|r| r != "OWNER"));
    if deactivating && target.id == op.id {
        return Err(cannot_remove_self());
    }
    if leaving_owner {
        assert_another_active_owner(&mut tx, target.id).await?;
    }

    if username.is_none() && name.is_none() && role.is_none() && status.is_none() {
        return Ok(Json(json!({
            "success": true,
            "message": "Admin data updated successfully",
            "data": target.to_json(),
        })));
    }

    let mut qb = QueryBuilder::<Postgres>::new("UPDATE operators SET ");
    if let Some(v) = username {
        qb.push("username = ").push_bind(v.to_owned()).push(", ");
    }
    if let Some(v) = name {
        qb.push("name = ").push_bind(v.to_owned()).push(", ");
    }
    if let Some(v) = role {
        qb.push("role = ")
            .push_bind(v.to_owned())
            .push("::\"OperatorRole\", ");
    }
    if let Some(v) = status {
        qb.push("status = ").push_bind(v).push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(target.id)
        .push(format!(" RETURNING {PUBLIC_COLUMNS}"));
    let row = qb
        .build_query_as::<OperatorRow>()
        .fetch_one(&mut *tx)
        .await
        .map_err(map_unique)?;
    if deactivating {
        revoke_mcp_tokens(&mut tx, target.id).await?;
    }

    let mut before = Map::new();
    let mut after = Map::new();
    if username.is_some() {
        before.insert("username".into(), json!(target.username));
        after.insert("username".into(), json!(row.username));
    }
    if name.is_some() {
        before.insert("name".into(), json!(target.name));
        after.insert("name".into(), json!(row.name));
    }
    if role.is_some() {
        before.insert("role".into(), json!(target.role));
        after.insert("role".into(), json!(row.role));
    }
    if status.is_some() {
        before.insert("status".into(), json!(target.status));
        after.insert("status".into(), json!(row.status));
    }
    let (before, after) = (Value::Object(before), Value::Object(after));
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "operator.update",
            target_type: "operator",
            target_uuid: Some(uuid),
            before: Some(&before),
            after: Some(&after),
            reason,
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "success": true,
        "message": "Admin data updated successfully",
        "data": row.to_json(),
    })))
}

/// Node: src/admin/admin.controller.ts:171 (`deleteAdmin`) → `AdminService.remove` (:497).
/// Soft delete: `status = 0` + cabut token MCP; baris tidak pernah dihapus.
pub async fn remove(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(raw_uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(&[OperatorRole::Owner])?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = run_body(&body, &[REASON], true)?;
    let uuid = parse_uuid_pipe(&raw_uuid)?;
    let reason = text(&dto, "reason");

    let mut tx = state.db.begin().await?;
    advisory_xact_lock(&mut tx, "operators:roles").await?;
    let Some(target) = find_target(&mut tx, uuid).await? else {
        return Err(not_found(&raw_uuid));
    };
    if target.id == op.id {
        return Err(cannot_remove_self());
    }
    if target.role == "OWNER" && target.status == 1 {
        assert_another_active_owner(&mut tx, target.id).await?;
    }
    let sql = format!(
        r#"UPDATE operators SET status = 0, "updatedAt" = $2 WHERE id = $1 RETURNING {PUBLIC_COLUMNS}"#
    );
    let row = sqlx::query_as::<_, OperatorRow>(&sql)
        .bind(target.id)
        .bind(now_naive())
        .fetch_one(&mut *tx)
        .await?;
    revoke_mcp_tokens(&mut tx, target.id).await?;
    let before = json!({ "status": target.status, "role": target.role });
    let after = json!({ "status": row.status, "role": row.role });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "operator.deactivate",
            target_type: "operator",
            target_uuid: Some(uuid),
            before: Some(&before),
            after: Some(&after),
            reason: Some(reason),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "success": true,
        "message": "Admin deactivated successfully",
        "data": row.to_json(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_of_large_numbers_follows_js_string_form() {
        assert!((js_parse_int_of_number(5.0) - 5.0).abs() < f64::EPSILON);
        assert!((js_parse_int_of_number(1e21) - 1.0).abs() < f64::EPSILON);
        assert!((js_parse_int_of_number(2.5e22) - 2.0).abs() < f64::EPSILON);
        assert!((js_parse_int_of_number(-3.0) + 3.0).abs() < f64::EPSILON);
    }
}
