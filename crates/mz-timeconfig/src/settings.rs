//! Port `src/manual-time-settings` (4 route `ManualTimeSettingsController`, `v1/manual-time-settings`).
//! `manual-time-decision.ts` / `manual-time-quota.ts` dipakai submission di
//! tracking-timelines, bukan route di sini, jadi tidak di-port.

use mz_core::uuid_rules::is_uuid_v4;
use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::State;
use mz_account::assignable_role::{assert_targets_within_rank, require_actor_role};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::company::require_company_id;
use mz_core::db::advisory_xact_lock;
use mz_core::extract::{Validate, ValidatedJson, ValidatedQuery, de};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder};

use crate::util::defined;

const LIMIT_STEP_MINUTES: i64 = 15;
const EXCEPTION_LIST_MAX: usize = 5000;
const EMPLOYEE_UUIDS_MAX: i64 = 5000;

const MANUAL_TIME_LIMIT_REQUIRED: &str = "MANUAL_TIME_LIMIT_REQUIRED";
const MANUAL_TIME_LIMIT_OUT_OF_RANGE: &str = "MANUAL_TIME_LIMIT_OUT_OF_RANGE";
const MANUAL_TIME_EXCEPTION_INVALID: &str = "MANUAL_TIME_EXCEPTION_INVALID";
const MANUAL_TIME_CUSTOM_LIMIT_RANGE: &str = "MANUAL_TIME_CUSTOM_LIMIT_RANGE";
const MANUAL_TIME_EXCEPTIONS_STALE: &str = "MANUAL_TIME_EXCEPTIONS_STALE";

/// `LIMIT_MINUTES_RANGE[period]` → (min, max).
fn limit_range(period: &str) -> (i64, i64) {
    if period == "WEEK" {
        (15, 10080)
    } else {
        (15, 1440)
    }
}

/// `computeExceptionsVersion` (`exceptions-version.ts`).
pub fn exceptions_version(row_ids: &[i32]) -> String {
    let mut sorted = row_ids.to_vec();
    sorted.sort_unstable();
    let joined = sorted
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let digest = Sha256::digest(joined.as_bytes());
    hex::encode(digest).chars().take(16).collect()
}

// ───────────────────────── DTO ─────────────────────────

/// `@IsInt()`.
fn as_int(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_f64()?;
    #[allow(clippy::cast_possible_truncation)]
    (n.is_finite() && n.fract() == 0.0).then_some(n as i64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomLimit {
    pub user_uuid: String,
    pub limit_minutes: i64,
}

/// `PutManualTimeSettingsDto` setelah lolos validasi.
#[derive(Debug, Clone)]
pub struct PutInput {
    pub mode: String,
    pub limit_period: String,
    pub limit_minutes: Option<i64>,
    pub auto_approve_new_employees: bool,
    pub exception_user_uuids: Vec<String>,
    pub custom_limits: Option<Vec<CustomLimit>>,
    pub exceptions_version: String,
}

/// Body `PUT` mentah; divalidasi manual supaya pesan class-validator sama.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct PutBody(pub Map<String, Value>);

fn enum_msg(key: &str, allowed: &[&str]) -> String {
    format!(
        "{key} must be one of the following values: {}",
        allowed.join(", ")
    )
}

impl Validate for PutBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        let enum_ok = |k: &str, allowed: &[&str]| {
            m.get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| allowed.contains(&s))
        };
        if !enum_ok("mode", &["NONE", "ALL", "LIMIT"]) {
            out.push(enum_msg("mode", &["NONE", "ALL", "LIMIT"]));
        }
        if !enum_ok("limitPeriod", &["DAY", "WEEK"]) {
            out.push(enum_msg("limitPeriod", &["DAY", "WEEK"]));
        }
        if let Some(v) = defined(m.get("limitMinutes")) {
            let n = v.as_f64();
            if !n.is_some_and(|n| n <= 10080.0) {
                out.push("limitMinutes must not be greater than 10080".into());
            }
            if !n.is_some_and(|n| n >= 15.0) {
                out.push("limitMinutes must not be less than 15".into());
            }
            if as_int(Some(v)).is_none() {
                out.push("limitMinutes must be an integer number".into());
            }
        }
        if !m
            .get("autoApproveNewEmployees")
            .is_some_and(Value::is_boolean)
        {
            out.push("autoApproveNewEmployees must be a boolean value".into());
        }

        // exceptionUserUuids: IsUUID(each), ArrayUnique, ArrayMaxSize, IsArray (bawah ke atas).
        let list = m.get("exceptionUserUuids").and_then(Value::as_array);
        match list {
            Some(a) => {
                if a.iter().any(|x| !x.as_str().is_some_and(is_uuid_v4)) {
                    out.push("each value in exceptionUserUuids must be a UUID".into());
                }
                let uniq: HashSet<String> = a.iter().map(Value::to_string).collect();
                if uniq.len() != a.len() {
                    out.push("All exceptionUserUuids's elements must be unique".into());
                }
                if a.len() > EXCEPTION_LIST_MAX {
                    out.push("exceptionUserUuids must contain no more than 5000 elements".into());
                }
            }
            None => {
                out.push("each value in exceptionUserUuids must be a UUID".into());
                out.push("All exceptionUserUuids's elements must be unique".into());
                out.push("exceptionUserUuids must contain no more than 5000 elements".into());
                out.push("exceptionUserUuids must be an array".into());
            }
        }

        if let Some(v) = defined(m.get("customLimits")) {
            match v.as_array() {
                Some(a) => {
                    if a.len() > EXCEPTION_LIST_MAX {
                        out.push("customLimits must contain no more than 5000 elements".into());
                    }
                    for (i, item) in a.iter().enumerate() {
                        let Some(o) = item.as_object() else {
                            out.push(format!(
                                "customLimits.{i} nested property must be either object or array"
                            ));
                            continue;
                        };
                        if !o
                            .get("userUuid")
                            .and_then(Value::as_str)
                            .is_some_and(is_uuid_v4)
                        {
                            out.push(format!("customLimits.{i}.userUuid must be a UUID"));
                        }
                        if as_int(o.get("limitMinutes")).is_none() {
                            out.push(format!(
                                "customLimits.{i}.limitMinutes must be an integer number"
                            ));
                        }
                    }
                }
                None => {
                    out.push("customLimits must contain no more than 5000 elements".into());
                    out.push("customLimits must be an array".into());
                    out.push("nested property customLimits must be either object or array".into());
                }
            }
        }

        let ver = m.get("exceptionsVersion");
        let s = ver.and_then(Value::as_str);
        if s.is_none_or(|s| s.encode_utf16().count() > 64) {
            out.push("exceptionsVersion must be shorter than or equal to 64 characters".into());
        }
        if ver.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
            out.push("exceptionsVersion should not be empty".into());
        }
        if s.is_none() {
            out.push("exceptionsVersion must be a string".into());
        }
        out
    }
}

impl PutInput {
    /// Hanya dipanggil setelah `PutBody::validate` kosong.
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let s = |k: &str| {
            m.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            mode: s("mode"),
            limit_period: s("limitPeriod"),
            limit_minutes: as_int(defined(m.get("limitMinutes"))),
            auto_approve_new_employees: m
                .get("autoApproveNewEmployees")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            exception_user_uuids: m
                .get("exceptionUserUuids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            custom_limits: defined(m.get("customLimits"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let o = x.as_object()?;
                            Some(CustomLimit {
                                user_uuid: o.get("userUuid")?.as_str()?.to_owned(),
                                limit_minutes: as_int(o.get("limitMinutes"))?,
                            })
                        })
                        .collect()
                }),
            exceptions_version: s("exceptionsVersion"),
        }
    }
}

/// Validasi level service (`validate()` di Node): 400 / 422.
pub fn validate_input(dto: &PutInput) -> ApiResult<()> {
    if let Some(custom) = &dto.custom_limits {
        let uuids: Vec<&str> = custom.iter().map(|c| c.user_uuid.as_str()).collect();
        if uuids.iter().collect::<HashSet<_>>().len() != uuids.len() {
            return Err(ApiError::bad_request(
                "customLimits must not contain duplicate userUuid",
            ));
        }
        let always: HashSet<&str> = dto
            .exception_user_uuids
            .iter()
            .map(String::as_str)
            .collect();
        if uuids.iter().any(|u| always.contains(u)) {
            return Err(ApiError::bad_request(
                "A user cannot be in both exceptionUserUuids and customLimits",
            ));
        }
        if always.len() + uuids.len() > EXCEPTION_LIST_MAX {
            return Err(ApiError::bad_request(format!(
                "The combined exception and custom limit rows must not exceed {EXCEPTION_LIST_MAX}"
            )));
        }
    }

    if dto.mode == "LIMIT" && dto.limit_minutes.is_none() {
        return Err(ApiError::body(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": MANUAL_TIME_LIMIT_REQUIRED,
                "message": "limitMinutes is required when mode is LIMIT",
            }),
        ));
    }
    let (min, max) = limit_range(&dto.limit_period);
    let invalid: Vec<&str> = dto
        .custom_limits
        .iter()
        .flatten()
        .filter(|c| {
            c.limit_minutes % LIMIT_STEP_MINUTES != 0
                || c.limit_minutes < min
                || c.limit_minutes > max
        })
        .map(|c| c.user_uuid.as_str())
        .collect();
    if !invalid.is_empty() {
        return Err(ApiError::body(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": MANUAL_TIME_CUSTOM_LIMIT_RANGE,
                "message": format!("customLimits.limitMinutes must be a multiple of {LIMIT_STEP_MINUTES} between {min} and {max}"),
                "details": { "invalidUserUuids": invalid, "minMinutes": min, "maxMinutes": max },
            }),
        ));
    }
    if let Some(minutes) = dto.limit_minutes
        && (minutes % LIMIT_STEP_MINUTES != 0 || minutes < min || minutes > max)
    {
        return Err(ApiError::body(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": MANUAL_TIME_LIMIT_OUT_OF_RANGE,
                "message": format!("limitMinutes must be a multiple of {LIMIT_STEP_MINUTES} between {min} and {max}"),
                "details": { "min": min, "max": max, "step": LIMIT_STEP_MINUTES },
            }),
        ));
    }
    Ok(())
}

// ───────────────────────── diff aturan ─────────────────────────

/// Hasil perbandingan baris hidup vs aturan yang diinginkan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub revoked_ids: Vec<i32>,
    pub revoked_user_ids: Vec<i32>,
    /// (userId, limitMinutes) baris baru, urut sisipan seperti `Map` JS.
    pub added: Vec<(i32, Option<i32>)>,
}

/// Baris pengecualian hidup: (id, userId, limitMinutes).
pub type LiveRow = (i32, i32, Option<i32>);

/// Isi `desired` (Map JS: urutan sisip, nilai ditimpa di tempat) lalu bandingkan.
/// `custom` = `None` berarti klien lama: batas khusus yang hidup dipertahankan.
pub fn plan_changes(live: &[LiveRow], always: &[i32], custom: Option<&[(i32, i32)]>) -> Plan {
    let mut desired: Vec<(i32, Option<i32>)> = Vec::new();
    let set = |d: &mut Vec<(i32, Option<i32>)>, uid: i32, v: Option<i32>| {
        if let Some(e) = d.iter_mut().find(|e| e.0 == uid) {
            e.1 = v;
        } else {
            d.push((uid, v));
        }
    };
    match custom {
        Some(c) => {
            for (uid, minutes) in c {
                set(&mut desired, *uid, Some(*minutes));
            }
        }
        None => {
            for (_, uid, limit) in live {
                if limit.is_some() {
                    set(&mut desired, *uid, *limit);
                }
            }
        }
    }
    for uid in always {
        set(&mut desired, *uid, None);
    }

    let mut unchanged: HashSet<i32> = HashSet::new();
    let mut revoked_ids = Vec::new();
    let mut revoked_user_ids = Vec::new();
    for (id, uid, limit) in live {
        if desired.iter().any(|d| d.0 == *uid && d.1 == *limit) {
            unchanged.insert(*uid);
        } else {
            revoked_ids.push(*id);
            revoked_user_ids.push(*uid);
        }
    }
    let added = desired
        .into_iter()
        .filter(|(uid, _)| !unchanged.contains(uid))
        .collect();
    Plan {
        revoked_ids,
        revoked_user_ids,
        added,
    }
}

// ───────────────────────── DB ─────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct CompanySettings {
    mode: String,
    limit_minutes: Option<i32>,
    period: String,
    auto_new: bool,
}

const COMPANY_SETTINGS_SQL: &str = r#"SELECT "manualTimeApprovalMode"::text AS mode,
    "manualTimeAutoApproveLimitMinutes" AS limit_minutes,
    "manualTimeAutoApprovePeriod"::text AS period,
    "manualTimeAutoApproveNewEmployees" AS auto_new
    FROM company WHERE id = $1"#;

#[derive(Debug, sqlx::FromRow)]
struct MemberRow {
    user_id: i32,
    is_allow: bool,
    uuid: String,
    name: String,
    username: String,
}

/// Node: manual-time-settings.service.ts:415 (`readSettings`).
async fn read_settings(state: &AppState, company_id: i32) -> ApiResult<Value> {
    let row: Option<CompanySettings> = sqlx::query_as(COMPANY_SETTINGS_SQL)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
    let members: Vec<MemberRow> = sqlx::query_as(
        r#"SELECT uc."userId" AS user_id, uc."isAllowManualTime" AS is_allow, u.uuid::text AS uuid,
                  u.name AS name, u.username AS username
           FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 AND uc.status = 1
             AND EXISTS (SELECT 1 FROM "manualTimeApprovalException" e
                         WHERE e."userId" = u.id AND e."companyId" = $1 AND e."deletedAt" IS NULL)
           ORDER BY u.name ASC, u.id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let allowed_employees: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1 AND "isAllowManualTime" = true"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    let live: Vec<LiveRow> = sqlx::query_as(
        r#"SELECT id, "userId", "limitMinutes" FROM "manualTimeApprovalException"
           WHERE "companyId" = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;

    let limit_by_user: HashMap<i32, Option<i32>> = live.iter().map(|(_, u, l)| (*u, *l)).collect();
    let allowed_members: Vec<&MemberRow> = members.iter().filter(|m| m.is_allow).collect();
    let allowed_custom = allowed_members
        .iter()
        .filter(|m| limit_by_user.get(&m.user_id).copied().flatten().is_some())
        .count();
    let ids: Vec<i32> = live.iter().map(|(id, _, _)| *id).collect();
    let (mode, limit_minutes, period, auto_new) = row
        .map_or(("ALL".to_owned(), None, "DAY".to_owned(), true), |r| {
            (r.mode, r.limit_minutes, r.period, r.auto_new)
        });
    Ok(json!({
        "data": {
            "mode": mode,
            "limitMinutes": limit_minutes,
            "limitPeriod": period,
            "autoApproveNewEmployees": auto_new,
            "exceptions": members.iter().map(|m| json!({
                "uuid": m.uuid,
                "name": m.name,
                "username": m.username,
                "isAllowManualTime": m.is_allow,
                "limitMinutes": limit_by_user.get(&m.user_id).copied().flatten(),
            })).collect::<Vec<_>>(),
            "exceptionsVersion": exceptions_version(&ids),
            "counts": {
                "allowedEmployees": allowed_employees,
                "allowedOnExceptionList": allowed_members.len(),
                "allowedAlwaysApproval": allowed_members.len() - allowed_custom,
                "allowedCustomLimit": allowed_custom,
            },
        }
    }))
}

/// Node: manual-time-settings.service.ts:50 (`getSettings`).
pub async fn get_settings(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    read_settings(state, company_id).await
}

/// Resolve uuid → user id (anggota aktif company); sisanya 422.
async fn resolve_member_ids(
    state: &AppState,
    company_id: i32,
    uuids: &[String],
) -> ApiResult<HashMap<String, i32>> {
    let mut unique: Vec<String> = Vec::new();
    for u in uuids {
        if !unique.contains(u) {
            unique.push(u.clone());
        }
    }
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, i32)> = sqlx::query_as(
        r#"SELECT u.uuid::text, uc."userId" FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = ANY($2::uuid[])"#,
    )
    .bind(company_id)
    .bind(&unique)
    .fetch_all(&state.db)
    .await?;
    let found: HashMap<String, i32> = rows.into_iter().collect();
    let unknown: Vec<&String> = unique.iter().filter(|u| !found.contains_key(*u)).collect();
    if !unknown.is_empty() {
        return Err(ApiError::body(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": MANUAL_TIME_EXCEPTION_INVALID,
                "message": "exceptionUserUuids or customLimits contains users that are not active members of this company",
                "details": { "unknownUserIds": unknown },
            }),
        ));
    }
    Ok(found)
}

/// Node: manual-time-settings.service.ts:56 (`replaceSettings`).
pub async fn replace_settings(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &PutInput,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    validate_input(dto)?;

    // TOCTOU T10 (diterima di Node): keanggotaan di-resolve di luar transaksi.
    let mut all_uuids = dto.exception_user_uuids.clone();
    all_uuids.extend(
        dto.custom_limits
            .iter()
            .flatten()
            .map(|c| c.user_uuid.clone()),
    );
    let member_ids = resolve_member_ids(state, company_id, &all_uuids).await?;
    let id_of = |u: &str| member_ids.get(u).copied().unwrap_or_default();
    let always: Vec<i32> = dto.exception_user_uuids.iter().map(|u| id_of(u)).collect();
    let custom: Option<Vec<(i32, i32)>> = dto.custom_limits.as_ref().map(|c| {
        c.iter()
            .map(|c| {
                (
                    id_of(&c.user_uuid),
                    i32::try_from(c.limit_minutes).unwrap_or_default(),
                )
            })
            .collect()
    });

    let mut tx = state.db.begin().await?;
    // `.catch(() => requestContext.discardAudit())`: transaksi yang rollback tidak
    // boleh meninggalkan baris audit yang berbohong.
    let discard_on_error = audit.discard_guard();
    // T7: dua admin menyimpan bersamaan → berurutan.
    advisory_xact_lock(
        &mut tx,
        &format!("manual-time-settings:company:{company_id}"),
    )
    .await?;

    let live: Vec<LiveRow> = sqlx::query_as(
        r#"SELECT id, "userId", "limitMinutes" FROM "manualTimeApprovalException"
           WHERE "companyId" = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    let ids: Vec<i32> = live.iter().map(|(id, _, _)| *id).collect();
    if exceptions_version(&ids) != dto.exceptions_version {
        return Err(ApiError::coded(
            http::StatusCode::CONFLICT,
            MANUAL_TIME_EXCEPTIONS_STALE,
            "The exception list has changed since it was loaded; reload the settings and save again",
        ));
    }

    let plan = plan_changes(&live, &always, custom.as_deref());
    // `desired.size` = baris yang bertahan + baru.
    let desired_size = plan.added.len() + (live.len() - plan.revoked_ids.len());
    if desired_size > EXCEPTION_LIST_MAX {
        return Err(ApiError::bad_request(format!(
            "The combined exception and custom limit rows must not exceed {EXCEPTION_LIST_MAX}"
        )));
    }

    // Aturan C: hanya user yang barisnya dicabut/dibuat yang dinilai.
    let added_ids: HashSet<i32> = plan.added.iter().map(|(u, _)| *u).collect();
    let mut changed: Vec<i32> = added_ids.iter().copied().collect();
    changed.extend(plan.revoked_user_ids.iter().copied());
    changed.sort_unstable();
    changed.dedup();
    if !changed.is_empty() {
        let still_members: HashSet<i32> = if plan.revoked_user_ids.is_empty() {
            HashSet::new()
        } else {
            sqlx::query_scalar::<_, i32>(
                r#"SELECT "userId" FROM "userCompany"
                   WHERE "companyId" = $1 AND status = 1 AND "userId" = ANY($2)"#,
            )
            .bind(company_id)
            .bind(&plan.revoked_user_ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect()
        };
        let to_check: Vec<i32> = changed
            .into_iter()
            .filter(|u| added_ids.contains(u) || still_members.contains(u))
            .collect();
        if !to_check.is_empty() {
            let actor = require_actor_role(&mut tx, user_id, company_id).await?;
            assert_targets_within_rank(&mut tx, &actor, &to_check, company_id).await?;
        }
    }

    let current: Option<CompanySettings> = sqlx::query_as(COMPANY_SETTINGS_SQL)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;

    // Hanya kolom yang berubah ditulis.
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE company SET ");
    let mut n = 0;
    {
        let mut sep = |qb: &mut QueryBuilder<Postgres>| {
            if n > 0 {
                qb.push(", ");
            }
            n += 1;
        };
        if current.as_ref().map(|c| c.mode.as_str()) != Some(dto.mode.as_str()) {
            sep(&mut qb);
            qb.push(r#""manualTimeApprovalMode" = "#)
                .push_bind(dto.mode.clone())
                .push(r#"::"ManualTimeApprovalMode""#);
        }
        if current.as_ref().map(|c| c.period.as_str()) != Some(dto.limit_period.as_str()) {
            sep(&mut qb);
            qb.push(r#""manualTimeAutoApprovePeriod" = "#)
                .push_bind(dto.limit_period.clone())
                .push(r#"::"ManualTimeLimitPeriod""#);
        }
        if let Some(minutes) = dto.limit_minutes
            && current
                .as_ref()
                .and_then(|c| c.limit_minutes)
                .map(i64::from)
                != Some(minutes)
        {
            sep(&mut qb);
            qb.push(r#""manualTimeAutoApproveLimitMinutes" = "#)
                .push_bind(i32::try_from(minutes).unwrap_or_default());
        }
        if current.as_ref().map(|c| c.auto_new) != Some(dto.auto_approve_new_employees) {
            sep(&mut qb);
            qb.push(r#""manualTimeAutoApproveNewEmployees" = "#)
                .push_bind(dto.auto_approve_new_employees);
        }
    }
    if n > 0 {
        // `tx.company.update({ where: { id }, data })` tanpa `select`.
        let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
        qb.push(" WHERE id = ").push_bind(company_id);
        qb.build().execute(&mut *tx).await?;
        if let Some(row) = audit
            .result_row(&mut *tx, Model::Company, Lookup::Id(company_id))
            .await
        {
            audit.record_update(Model::Company, pre, &row).await;
        }
    }

    if !plan.revoked_ids.is_empty() {
        // `updateMany({ data: { deletedAt } })` = anggota keluar.
        let membership = audit
            .membership_pre(
                Model::ManualTimeApprovalException,
                MemberEffect::Remove,
                Touched::Where(Lookup::Ids(plan.revoked_ids.clone())),
            )
            .await;
        sqlx::query(
            r#"UPDATE "manualTimeApprovalException" SET "deletedAt" = $2 WHERE id = ANY($1)"#,
        )
        .bind(&plan.revoked_ids)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
        audit.record_membership(membership).await;
    }
    if !plan.added.is_empty() {
        let users: Vec<i32> = plan.added.iter().map(|(u, _)| *u).collect();
        let limits: Vec<Option<i32>> = plan.added.iter().map(|(_, l)| *l).collect();
        // `createMany({ data })` = anggota masuk.
        let rows: Vec<Row> = plan
            .added
            .iter()
            .map(|(uid, minutes)| {
                Row::new()
                    .with("companyId", company_id)
                    .with("userId", *uid)
                    .with("limitMinutes", *minutes)
                    .with("createdById", user_id)
            })
            .collect();
        let membership = audit
            .membership_pre(
                Model::ManualTimeApprovalException,
                MemberEffect::Add,
                Touched::Rows(rows),
            )
            .await;
        sqlx::query(
            r#"INSERT INTO "manualTimeApprovalException" ("companyId", "userId", "limitMinutes", "createdById")
               SELECT $1, u, l, $4 FROM UNNEST($2::int[], $3::int[]) AS t(u, l)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(company_id)
        .bind(&users)
        .bind(&limits)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        audit.record_membership(membership).await;
    }
    tx.commit().await?;
    discard_on_error.disarm();

    read_settings(state, company_id).await
}

/// Query `GET /employees` (`ListManualTimeEmployeesDto`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EmployeesQuery {
    pub search: Option<String>,
    #[serde(deserialize_with = "de::opt_num")]
    pub page: Option<i64>,
    #[serde(deserialize_with = "de::opt_num")]
    pub limit: Option<i64>,
}

impl Validate for EmployeesQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self
            .search
            .as_ref()
            .is_some_and(|s| s.trim().encode_utf16().count() > 100)
        {
            out.push("search must be shorter than or equal to 100 characters".into());
        }
        // Bawah ke atas: Min, lalu IsInt; `limit` juga Max.
        if self.page.is_some_and(|p| p < 1) {
            out.push("page must not be less than 1".into());
        }
        if self.limit.is_some_and(|l| l > 100) {
            out.push("limit must not be greater than 100".into());
        }
        if self.limit.is_some_and(|l| l < 1) {
            out.push("limit must not be less than 1".into());
        }
        out
    }
}

/// Node: manual-time-settings.service.ts:322 (`listEmployees`).
pub async fn list_employees(
    state: &AppState,
    user_id: i32,
    q: &EmployeesQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let page = q.page.unwrap_or(1);
    let limit = q.limit.unwrap_or(20);
    let search = q.search.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let build = |select: &str| {
        let mut qb = QueryBuilder::<Postgres>::new(select);
        qb.push(
            r#" FROM "userCompany" uc JOIN users u ON u.id = uc."userId" WHERE uc."companyId" = "#,
        )
        .push_bind(company_id)
        .push(r#" AND uc.status = 1 AND uc."isAllowManualTime" = true"#);
        if let Some(s) = search {
            let pattern = format!(
                "%{}%",
                s.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            qb.push(" AND (u.name ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR u.username ILIKE ")
                .push_bind(pattern)
                .push(")");
        }
        qb
    };
    let total: i64 = build("SELECT COUNT(*)")
        .build_query_scalar()
        .fetch_one(&state.db)
        .await?;
    let mut rows_q = build("SELECT u.id, u.uuid::text, u.name, u.username");
    rows_q
        .push(" ORDER BY u.name ASC, u.id ASC LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind((page - 1).saturating_mul(limit));
    let rows: Vec<(i32, String, String, String)> =
        rows_q.build_query_as().fetch_all(&state.db).await?;

    let page_ids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    let listed: Vec<(i32, Option<i32>)> = if page_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT "userId", "limitMinutes" FROM "manualTimeApprovalException"
               WHERE "companyId" = $1 AND "deletedAt" IS NULL AND "userId" = ANY($2)"#,
        )
        .bind(company_id)
        .bind(&page_ids)
        .fetch_all(&state.db)
        .await?
    };
    let limit_by: HashMap<i32, Option<i32>> = listed.into_iter().collect();
    Ok(json!({
        "data": rows.iter().map(|(id, uuid, name, username)| json!({
            "uuid": uuid,
            "name": name,
            "username": username,
            "onExceptionList": limit_by.contains_key(id),
            "limitMinutes": limit_by.get(id).copied().flatten(),
        })).collect::<Vec<_>>(),
        "meta": { "page": page, "limit": limit, "total": total },
    }))
}

/// Node: manual-time-settings.service.ts:393 (`listAllowedEmployeeUuids`).
pub async fn list_employee_uuids(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let uuids: Vec<String> = sqlx::query_scalar(
        r#"SELECT u.uuid::text FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 AND uc.status = 1 AND uc."isAllowManualTime" = true
           ORDER BY u.name ASC, u.id ASC LIMIT $2"#,
    )
    .bind(company_id)
    .bind(EMPLOYEE_UUIDS_MAX)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1 AND "isAllowManualTime" = true"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "data": { "uuids": uuids, "total": total } }))
}

// ───────────────────────── handler HTTP ─────────────────────────

/// Node: manual-time-settings.controller.ts:42 (`getSettings`)
pub async fn h_get(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    Ok(Json(get_settings(&state, ctx.require_user_id()?).await?))
}

/// Node: manual-time-settings.controller.ts:50 (`listEmployees`)
pub async fn h_employees(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<EmployeesQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        list_employees(&state, ctx.require_user_id()?, &q).await?,
    ))
}

/// Node: manual-time-settings.controller.ts:62 (`listEmployeeUuids`)
pub async fn h_employee_uuids(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        list_employee_uuids(&state, ctx.require_user_id()?).await?,
    ))
}

/// Node: manual-time-settings.controller.ts:84 (`replaceSettings`)
pub async fn h_put(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<PutBody>,
) -> ApiResult<Json<Value>> {
    let input = PutInput::from_map(&body.0);
    Ok(Json(
        replace_settings(&state, &audit, ctx.require_user_id()?, &input).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const U1: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const U2: &str = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";

    fn put_body(v: Value) -> PutBody {
        PutBody(v.as_object().cloned().unwrap_or_default())
    }

    fn valid() -> Value {
        json!({
            "mode": "LIMIT", "limitPeriod": "DAY", "limitMinutes": 240,
            "autoApproveNewEmployees": true, "exceptionUserUuids": [U1],
            "exceptionsVersion": "abc",
        })
    }

    #[test]
    fn version_is_deterministic_and_order_free() {
        assert_eq!(
            exceptions_version(&[3, 1, 2]),
            exceptions_version(&[1, 2, 3])
        );
        assert_eq!(exceptions_version(&[]).len(), 16);
        // sha256("") = e3b0c442 98fc1c14...
        assert_eq!(exceptions_version(&[]), "e3b0c44298fc1c14");
        assert_ne!(exceptions_version(&[1]), exceptions_version(&[2]));
    }

    #[test]
    fn dto_valid_and_invalid() {
        assert!(put_body(valid()).validate().is_empty());
        let mut v = valid();
        v["mode"] = json!("X");
        v["limitMinutes"] = json!(5);
        v["exceptionUserUuids"] = json!([U1, U1]);
        let e = put_body(v).validate();
        assert!(
            e.contains(&"mode must be one of the following values: NONE, ALL, LIMIT".to_owned())
        );
        assert!(e.contains(&"limitMinutes must not be less than 15".to_owned()));
        assert!(e.contains(&"All exceptionUserUuids's elements must be unique".to_owned()));
        assert_eq!(put_body(json!({})).validate().len(), 2 + 1 + 4 + 3);
    }

    #[test]
    fn custom_limit_nested_errors() {
        let mut v = valid();
        v["customLimits"] = json!([{ "userUuid": "x", "limitMinutes": 1.5 }]);
        let e = put_body(v).validate();
        assert_eq!(
            e,
            vec![
                "customLimits.0.userUuid must be a UUID",
                "customLimits.0.limitMinutes must be an integer number"
            ]
        );
    }

    #[test]
    fn service_validation_codes() {
        let mut v = valid();
        v["limitMinutes"] = json!(20);
        let err = validate_input(&PutInput::from_map(&put_body(v).0));
        let Err(ApiError::Body { body, .. }) = err else {
            panic!("expected body error")
        };
        assert_eq!(body["code"], MANUAL_TIME_LIMIT_OUT_OF_RANGE);
        assert_eq!(body["details"]["max"], 1440);

        let mut v = valid();
        v["limitMinutes"] = Value::Null;
        let Err(ApiError::Body { body, .. }) = validate_input(&PutInput::from_map(&put_body(v).0))
        else {
            panic!("expected body error")
        };
        assert_eq!(body["code"], MANUAL_TIME_LIMIT_REQUIRED);

        let mut v = valid();
        v["mode"] = json!("ALL");
        v["customLimits"] = json!([{ "userUuid": U2, "limitMinutes": 7 }]);
        let Err(ApiError::Body { body, .. }) = validate_input(&PutInput::from_map(&put_body(v).0))
        else {
            panic!("expected body error")
        };
        assert_eq!(body["code"], MANUAL_TIME_CUSTOM_LIMIT_RANGE);
        assert_eq!(body["details"]["invalidUserUuids"], json!([U2]));
    }

    #[test]
    fn plan_keeps_unchanged_and_replaces_changed() {
        // live: user 10 "selalu", user 11 batas 60, user 12 batas 30
        let live = [(1, 10, None), (2, 11, Some(60)), (3, 12, Some(30))];
        // desired: 10 selalu (sama), 11 jadi selalu (berubah), 12 hilang, 13 baru batas 45
        let plan = plan_changes(&live, &[10, 11], Some(&[(13, 45)]));
        assert_eq!(plan.revoked_ids, vec![2, 3]);
        assert_eq!(plan.revoked_user_ids, vec![11, 12]);
        assert_eq!(plan.added, vec![(13, Some(45)), (11, None)]);
    }

    #[test]
    fn plan_legacy_client_preserves_custom() {
        let live = [(1, 11, Some(60))];
        let plan = plan_changes(&live, &[12], None);
        assert!(plan.revoked_ids.is_empty());
        assert_eq!(plan.added, vec![(12, None)]);
    }
}
