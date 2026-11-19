//! Bagian bersama controller `admin-company/**`: spesifikasi route operator, body mentah
//! ala `express.json`, urutan guard → pipe, amplop respons, galat ber-`code`, dan
//! `CompanyCacheInvalidator` (`src/admin-company/company-cache-invalidator.service.ts`).

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use fred::interfaces::PubsubInterface;
use mz_audit::AuditHandle;
use mz_core::permissions::Exempt;
use mz_core::{ApiError, ApiResult, AppState, RouteSpec};
use mz_operator::{Operator, OperatorRole};
use serde_json::{Map, Value, json};

/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`; autentikasi lewat
/// extractor [`Operator`] (`JwtAdminAuthGuard`).
pub const SPEC: RouteSpec = RouteSpec::public()
    .exempt(Exempt::SeparateAuth)
    .allow_locked();

pub const ALL_ROLES: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];
pub const OWNER: &[OperatorRole] = &[OperatorRole::Owner];
pub const OWNER_FINANCE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Finance];
pub const OWNER_SUPPORT: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Support];

/// Body seperti `express.json` (strict): bukan content-type JSON / kosong → `{}`; JSON rusak
/// atau primitif → 400. Galat parse DISIMPAN: body-parser Express berjalan sebelum guard
/// Nest, jadi galatnya dilaporkan lebih dulu dari 401 (lihat [`gate`]).
pub struct RawBody(pub Result<Value, ApiError>);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = mz_core::extract::is_json_content_type(req.headers());
        let bytes = match axum::body::Bytes::from_request(req, state).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(Self(Err(mz_core::extract::bytes_rejection(e))));
            }
        };
        Ok(Self(mz_core::extract::parse_express_json(is_json, &bytes)))
    }
}

/// Urutan Nest: body-parser (galat JSON) → `JwtAdminAuthGuard` (401) → `OperatorRolesGuard`
/// (403). `roles = None` = controller tanpa `OperatorRolesGuard` (controller lama).
pub fn gate(
    op: Result<Operator, ApiError>,
    audit: &AuditHandle,
    body: Option<&RawBody>,
    roles: Option<&[OperatorRole]>,
) -> ApiResult<Operator> {
    if let Some(RawBody(Err(e))) = body {
        return Err(clone_err(e));
    }
    let op = op?;
    // `store.operator` juga untuk handle yang tidak berbagi scope dengan extractor.
    audit.set_operator(op.id, &op.name);
    if let Some(r) = roles {
        op.require(r)?;
    }
    Ok(op)
}

fn clone_err(e: &ApiError) -> ApiError {
    match e {
        ApiError::BadJson(m) => ApiError::BadJson(m.clone()),
        ApiError::Message { status, message } => ApiError::message(*status, message.clone()),
        other => ApiError::bad_request(other.to_string()),
    }
}

/// Body yang sudah lolos [`gate`].
pub fn body_value(body: RawBody) -> Value {
    body.0.unwrap_or_else(|_| Value::Object(Map::new()))
}

/// `{ success: true, message: 'OK', data }`.
pub fn ok(data: Value) -> Value {
    json!({ "success": true, "message": "OK", "data": data })
}

/// `{ success: false, message, data: null }` (HTTP 200).
pub fn fail(message: impl Into<String>) -> Value {
    json!({ "success": false, "message": message.into(), "data": Value::Null })
}

/// `new XxxException({ code, message })`.
pub fn coded(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    ApiError::coded(status, code, message)
}

pub fn company_not_found() -> ApiError {
    coded(
        StatusCode::NOT_FOUND,
        "COMPANY_NOT_FOUND",
        "Company not found",
    )
}

/// `ParseUUIDPipe`.
pub fn uuid_pipe(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::uuid_rules::parse_uuid_pipe(raw)
}

/// UUID untuk kolom `@db.Uuid` tanpa pipe: `None` = Prisma akan melempar (format salah).
pub fn db_uuid(raw: &str) -> Option<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).ok()
}

/// Angka JS untuk kolom `Float` (30000 bukan 30000.0).
pub fn num(x: f64) -> Value {
    mz_core::jsfmt::num(x)
}

pub fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

/// `JsDate` opsional → JSON.
pub fn date(v: Option<chrono::NaiveDateTime>) -> Value {
    v.map_or(Value::Null, |d| {
        Value::String(mz_core::time::JsDate(d).to_js_string())
    })
}

pub fn date_of(v: chrono::NaiveDateTime) -> Value {
    Value::String(mz_core::time::JsDate(v).to_js_string())
}

// ── CompanyCacheInvalidator ─────────────────────────────────────────────────

/// `CompanyChangeOptions`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompanyChange {
    pub modules: bool,
    pub divisions: bool,
    pub plan: bool,
    pub user_me: bool,
}

/// `UserChangeOptions`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UserChange {
    pub permissions: bool,
    pub user_me: bool,
    pub tokens: bool,
}

/// Pemilih company untuk `clearUserMeCache(companyWhere)`.
#[derive(Debug, Clone, Copy)]
pub enum CompanyWhere<'a> {
    Id(i32),
    Uuid(&'a uuid::Uuid),
}

/// `afterCompanyChange` — sesudah COMMIT, tidak pernah gagal.
pub async fn after_company_change(state: &AppState, company_id: i32, opts: CompanyChange) {
    if opts.modules {
        mz_core::feature_modules::invalidate_company(state, company_id);
    }
    if opts.divisions {
        // Rust tidak meng-cache gerbang divisi; proses Node diberi tahu lewat protokol.
        publish_division_gate(state, company_id);
    }
    // `opts.plan` (`planEntitlements.clear()`): Rust membaca gerbang paket tiap request
    // (`mz_core::plan`), dan protokol v1 belum punya `kind` untuk cache entitlement Node
    // (usulan core di plan/deviations/mz-admin-company.md).
    if opts.user_me {
        clear_user_me_cache(state, CompanyWhere::Id(company_id)).await;
    }
}

/// `afterUserChange` — sesudah COMMIT.
pub async fn after_user_change(state: &AppState, user_id: i32, opts: UserChange) {
    if opts.permissions {
        state.invalidate_user_access(user_id);
    }
    if opts.tokens {
        state.invalidate_token_revocation(user_id);
    }
    if opts.user_me {
        mz_core::invalidation::forget_user_me(state, user_id).await;
    }
}

/// `clearUserMeCache(companyWhere)`: `user-me-{id}` semua anggota (status apa pun).
pub async fn clear_user_me_cache(state: &AppState, which: CompanyWhere<'_>) {
    let rows: sqlx::Result<Vec<i32>> = match which {
        CompanyWhere::Id(id) => {
            sqlx::query_scalar(r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1"#)
                .bind(id)
                .fetch_all(&state.db)
                .await
        }
        CompanyWhere::Uuid(u) => {
            sqlx::query_scalar(
                r#"SELECT uc."userId" FROM "userCompany" uc
                   JOIN company c ON c.id = uc."companyId" WHERE c.uuid = $1"#,
            )
            .bind(u)
            .fetch_all(&state.db)
            .await
        }
    };
    match rows {
        Ok(ids) => {
            for id in ids {
                mz_core::invalidation::forget_user_me(state, id).await;
            }
        }
        Err(err) => tracing::warn!(error = %err, "Gagal membuang cache user-me company"),
    }
}

/// Pesan protokol `division-gate` (`DivisionGateService.invalidate`, khusus Node).
/// `mz_core::invalidation::Message` belum punya varian ini (usulan core), jadi dikirim
/// mentah dengan bentuk & `src` yang sama. Fire-and-forget.
pub fn publish_division_gate(state: &AppState, company_id: i32) {
    if !state.config.cache_invalidation_enabled() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let payload = division_gate_message(company_id, mz_core::invalidation::instance_id());
    let channel = state.config.cache_invalidation_channel();
    let redis = state.redis.clone();
    handle.spawn(async move {
        let res: Result<i64, _> = redis.next().publish(channel, payload).await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "publish invalidasi division-gate gagal");
        }
    });
}

pub fn division_gate_message(company_id: i32, src: &str) -> String {
    json!({ "v": 1, "src": src, "kind": "division-gate", "companyId": company_id }).to_string()
}

/// `SELECT ... FOR UPDATE` baris company lewat uuid (kolom yang dibutuhkan pemanggil dibaca
/// terpisah).
pub async fn lock_company_by_uuid(
    conn: &mut sqlx::PgConnection,
    uuid: &uuid::Uuid,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar(r#"SELECT id FROM company WHERE uuid = $1 FOR UPDATE"#)
        .bind(uuid)
        .fetch_optional(conn)
        .await
}

/// Galat unik `inviteCode` (`isInviteCodeCollision`).
pub fn is_invite_code_collision(err: &ApiError) -> bool {
    match err {
        ApiError::UniqueViolation(m) => m.contains("inviteCode"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn division_gate_message_matches_protocol() {
        let v: Value =
            serde_json::from_str(&division_gate_message(7, "rust-x")).unwrap_or_default();
        assert_eq!(
            v,
            json!({ "v": 1, "src": "rust-x", "kind": "division-gate", "companyId": 7 })
        );
    }

    #[test]
    fn envelopes() {
        assert_eq!(
            ok(json!(1)).to_string(),
            r#"{"success":true,"message":"OK","data":1}"#
        );
        assert_eq!(
            fail("x").to_string(),
            r#"{"success":false,"message":"x","data":null}"#
        );
    }
}
