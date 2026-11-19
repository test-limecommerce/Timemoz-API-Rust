//! Port `src/admin-jobs/admin-jobs.controller.ts` (`admin/jobs/*`) + `admin-jobs.dto.ts`.
//!
//! `@PermissionExempt('separate-auth')` + `JwtAdminAuthGuard` (extractor [`Operator`]) +
//! `OperatorRolesGuard` (kelas: OWNER/FINANCE/SUPPORT; backup/restore/merge/download: OWNER),
//! `ADMIN_VALIDATION_PIPE`. Audit `admin_job.create` ditulis di transaksi klaim (tanpa
//! `@OperatorAuditAction`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use mz_core::qs::QVal;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorRole};
use serde_json::Value;

use super::service::{self, BackupDto, JOB_STATUSES, JOB_TYPES, ListQuery, MergeDto, RestoreDto};
use crate::validate::{
    Errors, RawBody, RawPath, RawQuery, is_date_only, is_iso8601_strict, present, qval_json,
    reason, trimmed,
};

const ALL: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];
const OWNER: &[OperatorRole] = &[OperatorRole::Owner];
const DATE_RE: &str = r"/^\d{4}-\d{2}-\d{2}$/";
const SUFFIX_RE: &str = "/^[A-Za-z0-9._-]+$/";

fn suffix_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn opt_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

/// `CreateBackupJobDto` (`ReasonDto` + companyUuid, skipHeavy?, redactSecrets?).
pub fn validate_backup(body: &Value) -> Result<BackupDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(
        body,
        &["companyUuid", "skipHeavy", "redactSecrets", "reason"],
    );
    e.uuid("companyUuid", body.get("companyUuid"));
    for k in ["skipHeavy", "redactSecrets"] {
        if present(body.get(k)) {
            e.boolean(k, body.get(k));
        }
    }
    let reason = reason(&mut e, body);
    e.into_result()?;
    Ok(BackupDto {
        company_uuid: body
            .get("companyUuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        skip_heavy: opt_bool(body.get("skipHeavy")),
        redact_secrets: opt_bool(body.get("redactSecrets")),
        reason,
    })
}

/// `CreateRestoreJobDto`.
pub fn validate_restore(body: &Value) -> Result<RestoreDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(
        body,
        &[
            "backupJobUuid",
            "mode",
            "rename",
            "users",
            "usernameSuffix",
            "dryRun",
            "reason",
        ],
    );
    e.uuid("backupJobUuid", body.get("backupJobUuid"));
    e.one_of("mode", body.get("mode"), &["clone"]);
    let rename = trimmed(body.get("rename"));
    if present(rename.as_ref()) {
        e.length("rename", rename.as_ref(), 1, 200);
        e.string("rename", rename.as_ref());
    }
    e.one_of("users", body.get("users"), &["reuse", "insert"]);
    let suffix = body.get("usernameSuffix");
    if present(suffix) {
        e.matches("usernameSuffix", suffix, SUFFIX_RE, suffix_ok);
        e.length("usernameSuffix", suffix, 1, 30);
        e.string("usernameSuffix", suffix);
    }
    e.boolean("dryRun", body.get("dryRun"));
    let reason = reason(&mut e, body);
    e.into_result()?;
    Ok(RestoreDto {
        backup_job_uuid: body
            .get("backupJobUuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        rename: rename.and_then(|v| v.as_str().map(str::to_owned)),
        users: body
            .get("users")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        username_suffix: suffix.and_then(Value::as_str).map(str::to_owned),
        dry_run: opt_bool(body.get("dryRun")).unwrap_or(false),
        reason,
    })
}

/// `CreateMergeJobDto`.
pub fn validate_merge(body: &Value) -> Result<MergeDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(
        body,
        &[
            "targetCompanyUuid",
            "legacyCompanyId",
            "preferLegacy",
            "retention",
            "since",
            "until",
            "dryRun",
            "reason",
        ],
    );
    e.uuid("targetCompanyUuid", body.get("targetCompanyUuid"));
    let legacy = e.int_range(
        "legacyCompanyId",
        body.get("legacyCompanyId"),
        1,
        2_147_483_647,
    );
    if present(body.get("preferLegacy")) {
        e.boolean("preferLegacy", body.get("preferLegacy"));
    }
    e.one_of("retention", body.get("retention"), &["UNLIMITED", "ACCEPT"]);
    for k in ["since", "until"] {
        let v = body.get(k);
        if present(v) {
            if !v.and_then(Value::as_str).is_some_and(is_iso8601_strict) {
                e.push(format!("{k} must be a valid ISO 8601 date string"));
            }
            e.matches(k, v, DATE_RE, is_date_only);
        }
    }
    e.boolean("dryRun", body.get("dryRun"));
    let reason = reason(&mut e, body);
    e.into_result()?;
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    Ok(MergeDto {
        target_company_uuid: s("targetCompanyUuid").unwrap_or_default(),
        legacy_company_id: legacy.unwrap_or_default(),
        prefer_legacy: opt_bool(body.get("preferLegacy")),
        retention: s("retention").unwrap_or_default(),
        since: s("since"),
        until: s("until"),
        dry_run: opt_bool(body.get("dryRun")).unwrap_or(false),
        reason,
    })
}

/// `AdminJobsQueryDto`.
pub fn validate_list(q: &[(String, QVal)]) -> Result<ListQuery, ApiError> {
    let mut e = Errors::default();
    e.unknown_query(
        q,
        &[
            "type",
            "status",
            "companyUuid",
            "pageNumber",
            "itemsPerPage",
        ],
    );
    let get = |k: &str| mz_core::qs::get(q, k);
    let json = |k: &str| get(k).map(qval_json);
    if let Some(v) = json("type") {
        e.one_of("type", Some(&v), JOB_TYPES);
    }
    if let Some(v) = json("status") {
        e.one_of("status", Some(&v), JOB_STATUSES);
    }
    if let Some(v) = json("companyUuid") {
        e.uuid("companyUuid", Some(&v));
    }
    let page = match get("pageNumber") {
        Some(v) => e.query_int_range_min("pageNumber", v, 1),
        None => Some(1),
    };
    let per = match get("itemsPerPage") {
        Some(v) => e.query_int_range("itemsPerPage", Some(v), 1, 100),
        None => Some(25),
    };
    e.into_result()?;
    let s = |k: &str| get(k).and_then(QVal::as_str).map(str::to_owned);
    Ok(ListQuery {
        ty: s("type"),
        status: s("status"),
        company_uuid: s("companyUuid"),
        page_number: page.unwrap_or(1),
        items_per_page: per.unwrap_or(25),
    })
}

impl Errors {
    /// `@Type(() => Number) @IsInt() @Min(min)` (tanpa `@Max`): pesan Min lalu IsInt.
    pub fn query_int_range_min(&mut self, field: &str, v: &QVal, min: i64) -> Option<i64> {
        let num = match v {
            QVal::One(s) => mz_core::jsfmt::number(s),
            _ => f64::NAN,
        };
        let before = self.0.len();
        #[allow(clippy::cast_precision_loss)]
        if num.is_nan() || num < min as f64 {
            self.push(format!("{field} must not be less than {min}"));
        }
        if !(num.is_finite() && num.fract() == 0.0) {
            self.push(format!("{field} must be an integer number"));
        }
        #[allow(clippy::cast_possible_truncation)]
        (self.0.len() == before).then_some(num as i64)
    }
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:63 (`backup`).
pub async fn h_backup(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(OWNER)?;
    let dto = validate_backup(&body)?;
    let res = service::create_backup(&state, &ctx, dto, op.id).await?;
    Ok((StatusCode::ACCEPTED, Json(res)))
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:72 (`restore`).
pub async fn h_restore(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(OWNER)?;
    let dto = validate_restore(&body)?;
    let res = service::create_restore(&state, &ctx, dto, op.id).await?;
    Ok((StatusCode::ACCEPTED, Json(res)))
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:83 (`merge`).
pub async fn h_merge(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(OWNER)?;
    let dto = validate_merge(&body)?;
    let res = service::create_merge(&state, &ctx, dto, op.id).await?;
    Ok((StatusCode::ACCEPTED, Json(res)))
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:94 (`list`).
pub async fn h_list(
    State(state): State<AppState>,
    op: Operator,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    op.require(ALL)?;
    let query = validate_list(&q)?;
    Ok(Json(service::list(&state, &query).await?))
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:100 (`get`).
pub async fn h_get(
    State(state): State<AppState>,
    op: Operator,
    RawPath(raw): RawPath,
) -> ApiResult<Json<Value>> {
    op.require(ALL)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;
    Ok(Json(service::get(&state, uuid).await?))
}

/// `GET /admin/jobs/backup|restore|merge`: di Nest jatuh ke `GET :uuid` (ParseUUIDPipe 400).
pub async fn h_get_static(op: Operator) -> ApiResult<Json<Value>> {
    op.require(ALL)?;
    Err(ApiError::bad_request(
        "Validation failed (uuid is expected)",
    ))
}

/// Node: src/admin-jobs/admin-jobs.controller.ts:106 (`download`).
pub async fn h_download(
    State(state): State<AppState>,
    op: Operator,
    RawPath(raw): RawPath,
) -> ApiResult<Json<Value>> {
    op.require(OWNER)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;
    Ok(Json(service::download(&state, uuid).await?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!("../../tests/golden/validation.json")).unwrap()
    }

    fn errors_of<T>(r: Result<T, ApiError>) -> Option<Vec<String>> {
        match r {
            Ok(_) => None,
            Err(ApiError::Validation(v)) => Some(v),
            Err(other) => Some(vec![format!("OTHER {other}")]),
        }
    }

    fn check_body<T: std::fmt::Debug>(key: &str, f: fn(&Value) -> Result<T, ApiError>) {
        for case in golden()[key].as_array().unwrap() {
            let got = errors_of(f(&case["input"]));
            let want = case.get("errors").map(|e| {
                e.as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            });
            assert_eq!(got, want, "{key} {}", case["input"]);
        }
    }

    /// Pesan & urutan = `ValidationPipe` Node (`tests/golden/validation.json`).
    #[test]
    fn body_identik_dengan_node() {
        check_body("backup", validate_backup);
        check_body("restore", validate_restore);
        check_body("merge", validate_merge);
    }

    #[test]
    fn query_identik_dengan_node() {
        for case in golden()["jobsQuery"].as_array().unwrap() {
            let q = mz_core::qs::parse(case["input"].as_str().unwrap());
            let res = validate_list(&q);
            match case.get("errors") {
                Some(want) => {
                    let want: Vec<String> = want
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| s.as_str().unwrap().to_owned())
                        .collect();
                    assert_eq!(errors_of(res), Some(want), "{}", case["input"]);
                }
                None => {
                    let q = res.unwrap();
                    assert_eq!(q.page_number, case["ok"]["pageNumber"].as_i64().unwrap());
                    assert_eq!(
                        q.items_per_page,
                        case["ok"]["itemsPerPage"].as_i64().unwrap()
                    );
                    assert_eq!(q.ty.as_deref(), case["ok"]["type"].as_str());
                }
            }
        }
    }

    #[test]
    fn restore_transform_trim() {
        let dto = validate_restore(&serde_json::json!({
            "backupJobUuid": "11111111-1111-4111-8111-111111111111", "mode": "clone", "users": "insert",
            "dryRun": false, "reason": "  Uji restore  ", "rename": "  PT Baru  ", "usernameSuffix": ".copy"
        }))
        .unwrap();
        assert_eq!(dto.rename.as_deref(), Some("PT Baru"));
        assert_eq!(dto.reason.as_deref(), Some("Uji restore"));
    }
}
