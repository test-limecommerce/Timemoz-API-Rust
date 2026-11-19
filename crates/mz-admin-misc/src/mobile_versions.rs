//! Port `src/admin-global/mobile-versions.*` (`mobile-versions.controller.ts`, `.service.ts`).
//!
//! * `GET /admin/mobile-versions` (`:35`): `OperatorRoles(OWNER, FINANCE, SUPPORT)`.
//! * `PUT /admin/mobile-versions/:type` (`:42`): `OperatorRoles(OWNER, SUPPORT)`, 200.
//!   Audit `mobile_version.set` ditulis SERVICE di dalam transaksi (`recordOperatorAction`),
//!   bukan lewat interceptor (tidak ada `@OperatorAuditAction` di handler ini).
//!
//! Cache: Node tidak menyimpan `latestVersionMobile` di cache (`/settings/latest-version-mobile`
//! membaca DB langsung), jadi tidak ada invalidasi.

use axum::Json;
use axum::extract::{Path, State};
use mz_core::ApiResult;
use mz_core::{AppState, jsfmt};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Value, json};

use crate::common::{
    RawBody, coded_bad_request, field, finish, is_string, reason_errors, whitelist_body,
};

const ROLES_READ: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];
const ROLES_WRITE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Support];

/// `SetMobileVersionDto extends ReasonDto`: reason (dari ReasonDto), version, force.
const SET_KEYS: &[&str] = &["reason", "version", "force"];

/// `SEMVER_REGEX` = `^\d{1,4}\.\d{1,4}\.\d{1,4}$` (`\d` ASCII).
fn is_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `ParseMobileTypePipe`: `ANDROID` | `IOS`.
fn parse_type(raw: &str) -> ApiResult<&'static str> {
    match raw {
        "ANDROID" => Ok("ANDROID"),
        "IOS" => Ok("IOS"),
        _ => Err(coded_bad_request(
            "INVALID_TYPE",
            "type must be ANDROID or IOS",
        )),
    }
}

struct SetInput {
    reason: String,
    version: String,
    force: bool,
}

/// `SetMobileVersionDto` di bawah `ADMIN_VALIDATION_PIPE` (whitelist + forbidNonWhitelisted).
fn validate_set(body: &Value) -> ApiResult<SetInput> {
    let mut e = Vec::new();
    whitelist_body(&mut e, body, SET_KEYS);
    // class-validator mengurutkan metadata properti subclass (version, force) SEBELUM properti
    // `ReasonDto` (paritas adm-misc: Node `version, force, reason`).
    let mut reason_e = Vec::new();
    let reason = reason_errors(&mut reason_e, body);

    // `@Transform(trim) @IsString() @Matches(SEMVER)`: pesan dari bawah → Matches, lalu IsString.
    let raw = field(body, "version");
    let version = match raw {
        Some(Value::String(s)) => Some(jsfmt::trim(s).to_owned()),
        _ => None,
    };
    if !version.as_deref().is_some_and(is_semver) {
        e.push("version must look like 2.5.0".to_owned());
    }
    if !is_string(raw) {
        e.push("version must be a string".to_owned());
    }

    let force = field(body, "force").and_then(Value::as_bool);
    if force.is_none() {
        e.push("force must be a boolean value".to_owned());
    }
    e.extend(reason_e);
    finish(e)?;

    Ok(SetInput {
        reason: reason.unwrap_or_default(),
        version: version.unwrap_or_default(),
        force: force.unwrap_or_default(),
    })
}

/// Node: src/admin-global/mobile-versions.controller.ts:35 (`list`).
pub async fn h_list(State(state): State<AppState>, op: Operator) -> ApiResult<Json<Value>> {
    op.require(ROLES_READ)?;
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        r#"SELECT "type"::text, version, force FROM "latestVersionMobile" ORDER BY "type" ASC"#,
    )
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(t, v, f)| json!({ "type": t, "version": v, "force": f }))
        .collect();
    Ok(Json(json!({ "data": data })))
}

/// Node: src/admin-global/mobile-versions.controller.ts:42 (`set`, 200).
pub async fn h_set(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    Path(platform): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(ROLES_WRITE)?;
    let ty = parse_type(&platform)?;
    let input = validate_set(&body)?;
    let data = set(&state, &op, &audit, ty, &input).await?;
    Ok(Json(json!({ "data": data })))
}

/// `service.set`: upsert pada `type` + audit di transaksi yang sama.
async fn set(
    state: &AppState,
    op: &Operator,
    audit: &AuditCtx,
    ty: &str,
    input: &SetInput,
) -> ApiResult<Value> {
    let mut tx = state.db.begin().await?;
    let before: Option<(String, bool)> = sqlx::query_as(
        r#"SELECT version, force FROM "latestVersionMobile" WHERE "type" = $1::"LatestVersionMobileType""#,
    )
    .bind(ty)
    .fetch_optional(&mut *tx)
    .await?;
    let (version, force): (String, bool) = sqlx::query_as(
        r#"INSERT INTO "latestVersionMobile" ("type", version, force)
           VALUES ($1::"LatestVersionMobileType", $2, $3)
           ON CONFLICT ("type") DO UPDATE SET version = EXCLUDED.version, force = EXCLUDED.force
           RETURNING version, force"#,
    )
    .bind(ty)
    .bind(&input.version)
    .bind(input.force)
    .fetch_one(&mut *tx)
    .await?;

    let before_json = before.map(|(v, f)| json!({ "type": ty, "version": v, "force": f }));
    let after_json = json!({ "type": ty, "version": version, "force": force });
    record(
        &mut *tx,
        audit,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "mobile_version.set",
            target_type: "mobileVersion",
            target_uuid: None,
            company_id: None,
            before: before_json.as_ref(),
            after: Some(&after_json),
            reason: Some(&input.reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "type": ty, "version": version, "force": force }))
}
