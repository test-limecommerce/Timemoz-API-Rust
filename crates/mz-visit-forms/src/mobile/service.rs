//! Port `mobile-store-visit-forms.service.ts` (§5.2): daftar form satu kunjungan, schema
//! versi yang di-pin + jawaban, simpan draft, submit, unggah/konfirmasi file.
//!
//! Tidak ada satu pun jejak revisi (`revisionCount`, `lastEdited*`) yang keluar dari sini
//! (Keputusan §0.32): kolom submission disebut satu per satu di [`SUBMISSION_COLUMNS`].

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor};

use super::visit::{
    VisitContext, assert_visit_writable, denormalized_columns, find_own_visit,
    find_visit_for_write, prisma_uuid,
};
use crate::answers_validator::{
    AnswerValidationMode, ValidateAnswersOptions, ValidateAnswersResult, answers_byte_size,
    validate_answers,
};
use crate::consts::{MAX_ANSWERS_BYTES, status};
use crate::errors::{coded, coded_details, not_found};
use crate::files::{self, Audience, UPLOAD_URL_TTL_SECONDS, UploadRequest};
use crate::types::{FormAnswerIssue, FormDocument};

/// `MAX_WRITE_ATTEMPTS` — berapa kali seluruh operasi tulis diulang.
pub const MAX_WRITE_ATTEMPTS: usize = 3;

/// `TemplateRow`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TemplateRow {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub description: Option<String>,
    pub sort_order: i32,
    pub is_active: bool,
    pub deleted_at: Option<NaiveDateTime>,
}

const TEMPLATE_COLUMNS: &str = r#"t.id, t.uuid::text AS uuid, t.name, t.description,
    t."sortOrder" AS sort_order, t."isActive" AS is_active, t."deletedAt" AS deleted_at"#;

/// `VersionRow`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct VersionRow {
    pub id: i32,
    pub uuid: String,
    pub version: i32,
    pub template_id: i32,
    pub schema: Value,
}

const VERSION_COLUMNS: &str =
    r#"id, uuid::text AS uuid, version, "templateId" AS template_id, schema"#;

/// `MobileSubmissionRow` (`SUBMISSION_SELECT`).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct SubmissionRow {
    pub id: i32,
    pub uuid: String,
    pub template_id: i32,
    pub template_version_id: i32,
    pub answers: Value,
    pub submitted_at: Option<NaiveDateTime>,
}

const SUBMISSION_COLUMNS: &str = r#"id, uuid::text AS uuid, "templateId" AS template_id,
    "templateVersionId" AS template_version_id, answers, "submittedAt" AS submitted_at"#;

fn ms(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_millis()
}

/// `statusOf(submission)`.
pub fn status_of(submission: Option<&SubmissionRow>) -> &'static str {
    match submission {
        None => status::NOT_STARTED,
        Some(s) if s.submitted_at.is_some() => status::SUBMITTED,
        Some(_) => status::DRAFT,
    }
}

fn already_submitted() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "FORM_ALREADY_SUBMITTED",
        "This form has already been submitted",
    )
}

fn template_not_found() -> ApiError {
    not_found("FORM_TEMPLATE_NOT_FOUND", "Form template not found")
}

// ── §4.1 — form apa saja yang berlaku ────────────────────────────────────────

/// `eligibleTemplateWhere(companyId, userId)` — `$1` = companyId, `$2` = userId.
const ELIGIBLE_WHERE: &str = r#"t."companyId" = $1 AND t."deletedAt" IS NULL AND t."isActive" = true
    AND EXISTS (SELECT 1 FROM "storeVisitFormTemplateVersion" v
                 WHERE v."templateId" = t.id AND v."publishedAt" IS NOT NULL)
    AND (t.scope::text = 'all' OR EXISTS (SELECT 1 FROM "storeVisitFormTemplateUser" a
                 WHERE a."templateId" = t.id AND a."userId" = $2 AND a."deletedAt" IS NULL))"#;

const TEMPLATE_ORDER: &str = r#"ORDER BY t."sortOrder" ASC, t.name ASC, t.id ASC"#;

/// `resolveApplicableTemplates(visit)` (:195) — termasuk cabang UNION (template yang sudah
/// pernah diisi di kunjungan ini tetap tampil).
async fn resolve_applicable_templates(
    state: &AppState,
    visit: &VisitContext,
) -> ApiResult<(Vec<TemplateRow>, Vec<SubmissionRow>)> {
    let eligible: Vec<TemplateRow> = sqlx::query_as(&format!(
        r#"SELECT {TEMPLATE_COLUMNS} FROM "storeVisitFormTemplate" t WHERE {ELIGIBLE_WHERE} {TEMPLATE_ORDER}"#
    ))
    .bind(visit.company_id)
    .bind(visit.user_id)
    .fetch_all(&state.db)
    .await?;
    let submissions: Vec<SubmissionRow> = sqlx::query_as(&format!(
        r#"SELECT {SUBMISSION_COLUMNS} FROM "storeVisitFormSubmission" WHERE "visitId" = $1"#
    ))
    .bind(visit.id)
    .fetch_all(&state.db)
    .await?;
    let known: HashSet<i32> = eligible.iter().map(|t| t.id).collect();
    let started_elsewhere: Vec<i32> = submissions
        .iter()
        .map(|s| s.template_id)
        .filter(|id| !known.contains(id))
        .collect();
    let retained: Vec<TemplateRow> = if started_elsewhere.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            r#"SELECT {TEMPLATE_COLUMNS} FROM "storeVisitFormTemplate" t
                WHERE t.id = ANY($1) AND t."companyId" = $2 {TEMPLATE_ORDER}"#
        ))
        .bind(&started_elsewhere)
        .bind(visit.company_id)
        .fetch_all(&state.db)
        .await?
    };
    let mut templates = eligible;
    templates.extend(retained);
    Ok((templates, submissions))
}

/// Pilih versi tertinggi per template (`latestPublishedVersions`, :229).
pub fn pick_latest(versions: Vec<VersionRow>) -> HashMap<i32, VersionRow> {
    let mut latest: HashMap<i32, VersionRow> = HashMap::new();
    for v in versions {
        let replace = latest
            .get(&v.template_id)
            .is_none_or(|current| v.version > current.version);
        if replace {
            latest.insert(v.template_id, v);
        }
    }
    latest
}

async fn latest_published_versions(
    state: &AppState,
    template_ids: &[i32],
) -> ApiResult<HashMap<i32, VersionRow>> {
    if template_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let versions: Vec<VersionRow> = sqlx::query_as(&format!(
        r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = ANY($1) AND "publishedAt" IS NOT NULL"#
    ))
    .bind(template_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(pick_latest(versions))
}

async fn versions_by_id(state: &AppState, ids: &[i32]) -> ApiResult<HashMap<i32, VersionRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let versions: Vec<VersionRow> = sqlx::query_as(&format!(
        r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion" WHERE id = ANY($1)"#
    ))
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    Ok(versions.into_iter().map(|v| (v.id, v)).collect())
}

/// `listForms(userId, visitUuid)` (:277) → `{ data: [...] }`.
pub async fn list_forms(state: &AppState, user_id: i32, visit_uuid: &str) -> ApiResult<Value> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    let (templates, submissions) = resolve_applicable_templates(state, &visit).await?;
    let by_template: HashMap<i32, &SubmissionRow> =
        submissions.iter().map(|s| (s.template_id, s)).collect();
    let pinned_ids: Vec<i32> = submissions.iter().map(|s| s.template_version_id).collect();
    let pinned = versions_by_id(state, &pinned_ids).await?;
    let fresh_ids: Vec<i32> = templates
        .iter()
        .filter(|t| !by_template.contains_key(&t.id))
        .map(|t| t.id)
        .collect();
    let latest = latest_published_versions(state, &fresh_ids).await?;

    let data: Vec<Value> = templates
        .iter()
        .map(|template| {
            let submission = by_template.get(&template.id).copied();
            let version = match submission {
                Some(s) => pinned.get(&s.template_version_id),
                None => latest.get(&template.id),
            };
            json!({
                "id": template.uuid,
                "name": template.name,
                "description": template.description,
                "sortOrder": template.sort_order,
                "status": status_of(submission),
                "submissionId": submission.map(|s| s.uuid.clone()),
                "submittedAt": submission.and_then(|s| s.submitted_at).map(ms),
                "versionId": version.map(|v| v.uuid.clone()),
                "version": version.map(|v| v.version),
            })
        })
        .collect();
    Ok(json!({ "data": data }))
}

// ── §7 — typeahead order ─────────────────────────────────────────────────────

/// `listOrderChoices(userId, visitUuid, query)` (:330) → `{ data, meta: { hasMore: false } }`.
pub async fn list_order_choices(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    search: Option<&str>,
    limit: i64,
) -> ApiResult<Value> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT uuid::text, "orderNumber" FROM "order" WHERE "companyId" = "#,
    );
    qb.push_bind(visit.company_id)
        .push(r#" AND status::text NOT IN ('rejected', 'cancelled')"#);
    // `query.search ? { contains, mode: 'insensitive' } : {}` — tanpa escape wildcard (Prisma).
    if let Some(s) = search.filter(|s| !s.is_empty()) {
        qb.push(r#" AND "orderNumber" ILIKE "#)
            .push_bind(format!("%{s}%"));
    }
    qb.push(r#" ORDER BY "orderDate" DESC LIMIT "#)
        .push_bind(limit);
    let rows: Vec<(String, String)> = qb.build_query_as().fetch_all(&state.db).await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect();
    Ok(json!({ "data": data, "meta": { "hasMore": false } }))
}

// ── §4.2 — aturan tulis mengikuti aturan baca ────────────────────────────────

/// Hasil `resolveTemplateForVisit`.
#[derive(Debug, Clone)]
pub struct ResolvedTemplate {
    pub template: TemplateRow,
    pub submission: Option<SubmissionRow>,
    /// Versi yang di-pin bila baris sudah ada; versi terpublish terbaru bila belum.
    pub version: VersionRow,
}

impl ResolvedTemplate {
    pub fn schema(&self) -> FormDocument {
        FormDocument::from_value(&self.version.schema)
    }
}

/// `resolveTemplateForVisit(client, visit, templateUuid)` (:369).
pub async fn resolve_template_for_visit(
    conn: &mut PgConnection,
    visit: &VisitContext,
    template_uuid: &str,
) -> ApiResult<ResolvedTemplate> {
    let uuid = prisma_uuid(template_uuid)?;
    let template: TemplateRow = sqlx::query_as(&format!(
        r#"SELECT {TEMPLATE_COLUMNS} FROM "storeVisitFormTemplate" t
            WHERE t.uuid = $1 AND t."companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(visit.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(template_not_found)?;

    let submission = find_submission(&mut *conn, visit.id, template.id).await?;

    if submission.is_none() {
        if template.deleted_at.is_some() {
            return Err(template_not_found());
        }
        if !template.is_active {
            return Err(coded(
                StatusCode::CONFLICT,
                "FORM_TEMPLATE_INACTIVE",
                "This form has been deactivated",
            ));
        }
        let assigned: Option<i32> = sqlx::query_scalar(
            r#"SELECT t.id FROM "storeVisitFormTemplate" t
                WHERE t.id = $1 AND (t.scope::text = 'all' OR EXISTS (
                  SELECT 1 FROM "storeVisitFormTemplateUser" a
                   WHERE a."templateId" = t.id AND a."userId" = $2 AND a."deletedAt" IS NULL))
                LIMIT 1"#,
        )
        .bind(template.id)
        .bind(visit.user_id)
        .fetch_optional(&mut *conn)
        .await?;
        if assigned.is_none() {
            return Err(coded(
                StatusCode::FORBIDDEN,
                "FORM_NOT_ASSIGNED",
                "This form is not assigned to you",
            ));
        }
    }

    let version: Option<VersionRow> = match &submission {
        Some(s) => {
            sqlx::query_as(&format!(
                r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion" WHERE id = $1"#
            ))
            .bind(s.template_version_id)
            .fetch_optional(&mut *conn)
            .await?
        }
        None => {
            sqlx::query_as(&format!(
                r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion"
                    WHERE "templateId" = $1 AND "publishedAt" IS NOT NULL
                    ORDER BY version DESC LIMIT 1"#
            ))
            .bind(template.id)
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    let Some(version) = version else {
        return Err(coded(
            StatusCode::CONFLICT,
            "FORM_TEMPLATE_NOT_PUBLISHED",
            "This form has no published version yet",
        ));
    };
    Ok(ResolvedTemplate {
        template,
        submission,
        version,
    })
}

async fn find_submission<'e>(
    db: impl PgExecutor<'e>,
    visit_id: i32,
    template_id: i32,
) -> sqlx::Result<Option<SubmissionRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {SUBMISSION_COLUMNS} FROM "storeVisitFormSubmission"
            WHERE "visitId" = $1 AND "templateId" = $2 LIMIT 1"#
    ))
    .bind(visit_id)
    .bind(template_id)
    .fetch_optional(db)
    .await
}

// ── §6.1–§6.3 — jalur tulis ──────────────────────────────────────────────────

/// `rejectInvalidAnswers(issues)` — 422 `FORM_VALIDATION_FAILED`.
pub fn reject_invalid_answers(issues: &[FormAnswerIssue]) -> ApiError {
    coded_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        "FORM_VALIDATION_FAILED",
        "Answers did not pass validation",
        json!({ "errors": issues }),
    )
}

/// `assertAnswersSize(answers)` — 413 `FORM_ANSWERS_TOO_LARGE`.
pub fn assert_answers_size(answers: &Value) -> ApiResult<()> {
    let bytes = answers_byte_size(Some(answers));
    if bytes > MAX_ANSWERS_BYTES {
        return Err(coded_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FORM_ANSWERS_TOO_LARGE",
            format!("Answers exceed the {MAX_ANSWERS_BYTES} byte limit"),
            json!({ "maxSizeBytes": MAX_ANSWERS_BYTES, "sizeBytes": bytes }),
        ));
    }
    Ok(())
}

/// `validateOrThrow(version, answers, mode, timeZone)`.
fn validate_or_throw(
    version: &VersionRow,
    answers: &Value,
    mode: AnswerValidationMode,
    time_zone: &str,
) -> ApiResult<ValidateAnswersResult> {
    let schema = FormDocument::from_value(&version.schema);
    let result = validate_answers(
        &schema,
        Some(answers),
        &ValidateAnswersOptions { mode, time_zone },
    );
    if !result.issues.is_empty() {
        return Err(reject_invalid_answers(&result.issues));
    }
    assert_answers_size(&Value::Object(result.answers.clone()))?;
    Ok(result)
}

/// Galat di dalam transaksi tulis: `PinnedVersionChanged` + unique violation diulang.
enum WriteError {
    Pinned,
    Api(ApiError),
}

impl From<ApiError> for WriteError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for WriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(ApiError::from(e))
    }
}

/// `isUniqueOn(e, column)` — P2002 pada unique yang memuat `column`. `ApiError::from(sqlx)`
/// membawa nama constraint Prisma (`storeVisitFormSubmission_visitId_templateId_key`,
/// `storeVisitFormSubmission_userId_clientRequestId_key`).
pub fn is_unique_violation_on(e: &ApiError, column: &str) -> bool {
    match e {
        ApiError::UniqueViolation(msg) => msg.to_lowercase().contains(&column.to_lowercase()),
        _ => false,
    }
}

/// Hasil tulis + baris yang dikembalikan (`toWriteResponse`).
pub struct WriteResult {
    pub submission: SubmissionRow,
    pub template: TemplateRow,
    pub version: VersionRow,
    pub orphan_keys: Vec<String>,
}

/// `toWriteResponse(result)` — bentuk sama dengan `getForm`, tanpa jejak revisi.
pub fn to_write_response(r: &WriteResult) -> Value {
    json!({
        "template": {
            "id": r.template.uuid,
            "name": r.template.name,
            "description": r.template.description,
        },
        "version": { "id": r.version.uuid, "version": r.version.version },
        "status": status_of(Some(&r.submission)),
        "submission": {
            "id": r.submission.uuid,
            "answers": r.submission.answers,
            "submittedAt": r.submission.submitted_at.map(ms),
        },
    })
}

async fn insert_submission(
    conn: &mut PgConnection,
    fresh: &VisitContext,
    template_id: i32,
    version_id: i32,
    answers: &Value,
) -> ApiResult<SubmissionRow> {
    let d = denormalized_columns(&mut *conn, fresh).await?;
    let row = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisitFormSubmission"
             ("companyId", "visitId", "visitUuid", "userId", "storeId", "storeName", "visitCheckInAt",
              "templateId", "templateVersionId", answers)
           VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8, $9, $10)
           RETURNING {SUBMISSION_COLUMNS}"#
    ))
    .bind(d.company_id)
    .bind(d.visit_id)
    .bind(&d.visit_uuid)
    .bind(d.user_id)
    .bind(d.store_id)
    .bind(&d.store_name)
    .bind(d.visit_check_in_at)
    .bind(template_id)
    .bind(version_id)
    .bind(Json(answers))
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

async fn draft_tx(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    resolved: &ResolvedTemplate,
    answers: &Value,
) -> Result<WriteResult, WriteError> {
    let mut tx = state.db.begin().await?;
    let fresh = find_visit_for_write(&mut *tx, visit_uuid).await?;
    assert_visit_writable(&fresh, user_id)?;
    let existing = find_submission(&mut *tx, fresh.id, resolved.template.id).await?;
    let submission = match existing {
        Some(existing) => {
            if existing.template_version_id != resolved.version.id {
                return Err(WriteError::Pinned);
            }
            if existing.submitted_at.is_some() {
                return Err(already_submitted().into());
            }
            sqlx::query_as(&format!(
                r#"UPDATE "storeVisitFormSubmission" SET answers = $2, "updatedAt" = $3
                    WHERE id = $1 RETURNING {SUBMISSION_COLUMNS}"#
            ))
            .bind(existing.id)
            .bind(Json(answers))
            .bind(mz_core::time::now_naive())
            .fetch_one(&mut *tx)
            .await?
        }
        None => {
            insert_submission(
                &mut tx,
                &fresh,
                resolved.template.id,
                resolved.version.id,
                answers,
            )
            .await?
        }
    };
    tx.commit().await?;
    Ok(WriteResult {
        submission,
        template: resolved.template.clone(),
        version: resolved.version.clone(),
        orphan_keys: Vec::new(),
    })
}

/// `saveDraft(userId, visitUuid, templateUuid, dto)` (:590) — upsert, validasi longgar.
pub async fn save_draft(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    answers: &Value,
) -> ApiResult<WriteResult> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    let time_zone = crate::admin::time_zone(state, visit.company_id).await?;
    let mut attempt = 0;
    loop {
        let resolved = {
            let mut conn = state.db.acquire().await?;
            resolve_template_for_visit(&mut conn, &visit, template_uuid).await?
        };
        let validated = validate_or_throw(
            &resolved.version,
            answers,
            AnswerValidationMode::Draft,
            &time_zone,
        )?;
        let stored = Value::Object(validated.answers);
        match draft_tx(state, user_id, visit_uuid, &resolved, &stored).await {
            Ok(r) => return Ok(r),
            Err(WriteError::Pinned) if attempt < MAX_WRITE_ATTEMPTS - 1 => {}
            Err(WriteError::Api(e))
                if attempt < MAX_WRITE_ATTEMPTS - 1 && is_unique_violation_on(&e, "templateId") => {
            }
            Err(WriteError::Pinned) => return Err(pinned_exhausted()),
            Err(WriteError::Api(e)) => return Err(e),
        }
        attempt += 1;
    }
}

/// `PinnedVersionChanged` yang lolos dari percobaan terakhir: di Node `Error` biasa → 500.
fn pinned_exhausted() -> ApiError {
    ApiError::internal(anyhow::anyhow!("PinnedVersionChanged"))
}

async fn submit_tx(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    resolved: &ResolvedTemplate,
    validated: &ValidateAnswersResult,
    client_request_id: &str,
) -> Result<WriteResult, WriteError> {
    let mut tx = state.db.begin().await?;
    let fresh = find_visit_for_write(&mut *tx, visit_uuid).await?;
    assert_visit_writable(&fresh, user_id)?;
    let template_id = resolved.template.id;
    let existing = find_submission(&mut *tx, fresh.id, template_id).await?;
    if let Some(existing) = &existing
        && existing.template_version_id != resolved.version.id
    {
        return Err(WriteError::Pinned);
    }
    if existing.is_none() {
        insert_submission(
            &mut tx,
            &fresh,
            template_id,
            resolved.version.id,
            &Value::Object(Map::new()),
        )
        .await?;
    }
    let now = mz_core::time::now_naive();
    let count = sqlx::query(
        r#"UPDATE "storeVisitFormSubmission"
              SET answers = $3, "submittedAt" = $4, "clientRequestId" = $5::uuid, "updatedAt" = $4
            WHERE "visitId" = $1 AND "templateId" = $2 AND "submittedAt" IS NULL"#,
    )
    .bind(fresh.id)
    .bind(template_id)
    .bind(Json(Value::Object(validated.answers.clone())))
    .bind(now)
    .bind(client_request_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let row: Option<(i32, Option<String>)> = sqlx::query_as(
        r#"SELECT id, "clientRequestId"::text FROM "storeVisitFormSubmission"
            WHERE "visitId" = $1 AND "templateId" = $2 LIMIT 1"#,
    )
    .bind(fresh.id)
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((row_id, row_request_id)) = row else {
        return Err(already_submitted().into());
    };
    let submission = find_submission(&mut *tx, fresh.id, template_id)
        .await?
        .ok_or_else(already_submitted)?;

    if count == 0 {
        if row_request_id.as_deref() != Some(client_request_id) {
            return Err(already_submitted().into());
        }
        // Retry idempoten: file sudah direkonsiliasi request yang berhasil.
        tx.commit().await?;
        return Ok(WriteResult {
            submission,
            template: resolved.template.clone(),
            version: resolved.version.clone(),
            orphan_keys: Vec::new(),
        });
    }

    crate::dynamic_options::assert_exists_on_submit(
        &mut tx,
        fresh.company_id,
        &validated.product_order_refs,
    )
    .await?;
    let orphan_keys = files::reconcile_on_submit(&mut tx, row_id, &validated.files).await?;
    tx.commit().await?;
    Ok(WriteResult {
        submission,
        template: resolved.template.clone(),
        version: resolved.version.clone(),
        orphan_keys,
    })
}

/// `submit(userId, visitUuid, templateUuid, dto)` (:678) — submit final, update bersyarat,
/// idempoten lewat `clientRequestId`.
pub async fn submit(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    answers: &Value,
    client_request_id: &str,
) -> ApiResult<WriteResult> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    let time_zone = crate::admin::time_zone(state, visit.company_id).await?;
    let mut attempt = 0;
    loop {
        let resolved = {
            let mut conn = state.db.acquire().await?;
            resolve_template_for_visit(&mut conn, &visit, template_uuid).await?
        };
        let validated = validate_or_throw(
            &resolved.version,
            answers,
            AnswerValidationMode::Final,
            &time_zone,
        )?;
        match submit_tx(
            state,
            user_id,
            visit_uuid,
            &resolved,
            &validated,
            client_request_id,
        )
        .await
        {
            Ok(r) => return Ok(r),
            Err(WriteError::Api(e)) if is_unique_violation_on(&e, "clientRequestId") => {
                return Err(coded(
                    StatusCode::CONFLICT,
                    "FORM_ALREADY_SUBMITTED",
                    "This clientRequestId has already been used by another submission",
                ));
            }
            Err(WriteError::Pinned) if attempt < MAX_WRITE_ATTEMPTS - 1 => {}
            Err(WriteError::Api(e))
                if attempt < MAX_WRITE_ATTEMPTS - 1 && is_unique_violation_on(&e, "templateId") => {
            }
            Err(WriteError::Pinned) => return Err(pinned_exhausted()),
            Err(WriteError::Api(e)) => return Err(e),
        }
        attempt += 1;
    }
}

/// `saveDraftResponse`.
pub async fn save_draft_response(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    answers: &Value,
) -> ApiResult<Value> {
    let r = save_draft(state, user_id, visit_uuid, template_uuid, answers).await?;
    Ok(to_write_response(&r))
}

/// `submitResponse` — objek bucket yatim dihapus SESUDAH commit.
pub async fn submit_response(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    answers: &Value,
    client_request_id: &str,
) -> ApiResult<Value> {
    let r = submit(
        state,
        user_id,
        visit_uuid,
        template_uuid,
        answers,
        client_request_id,
    )
    .await?;
    files::delete_objects_quietly(state, &r.orphan_keys).await;
    Ok(to_write_response(&r))
}

// ── §6.7 — unggah sebelum draft ada ──────────────────────────────────────────

async fn upload_tx(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    req: &UploadRequest,
) -> ApiResult<files::RegisteredUpload> {
    let mut tx = state.db.begin().await?;
    let fresh = find_visit_for_write(&mut *tx, visit_uuid).await?;
    assert_visit_writable(&fresh, user_id)?;
    let resolved = resolve_template_for_visit(&mut tx, &fresh, template_uuid).await?;
    let submission_id = match &resolved.submission {
        None => {
            let created: i32 = {
                let d = denormalized_columns(&mut *tx, &fresh).await?;
                sqlx::query_scalar(
                    r#"INSERT INTO "storeVisitFormSubmission"
                         ("companyId", "visitId", "visitUuid", "userId", "storeId", "storeName",
                          "visitCheckInAt", "templateId", "templateVersionId", answers)
                       VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8, $9, '{}'::jsonb)
                       RETURNING id"#,
                )
                .bind(d.company_id)
                .bind(d.visit_id)
                .bind(&d.visit_uuid)
                .bind(d.user_id)
                .bind(d.store_id)
                .bind(&d.store_name)
                .bind(d.visit_check_in_at)
                .bind(resolved.template.id)
                .bind(resolved.version.id)
                .fetch_one(&mut *tx)
                .await?
            };
            created
        }
        Some(s) if s.submitted_at.is_some() => return Err(already_submitted()),
        Some(s) => s.id,
    };
    let registered = files::register_upload(
        &mut tx,
        submission_id,
        fresh.company_id,
        &fresh.uuid,
        &resolved.schema(),
        req,
    )
    .await?;
    tx.commit().await?;
    Ok(registered)
}

/// `createFileUploadUrl(userId, visitUuid, templateUuid, dto)` (:863) — baris draft kosong
/// dibuat implisit; URL ditandatangani DI LUAR transaksi.
pub async fn create_file_upload_url(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    req: &UploadRequest,
) -> ApiResult<Value> {
    find_own_visit(&state.db, user_id, visit_uuid).await?;
    let mut attempt = 0;
    let registered = loop {
        match upload_tx(state, user_id, visit_uuid, template_uuid, req).await {
            Ok(r) => break r,
            Err(e)
                if attempt < MAX_WRITE_ATTEMPTS - 1 && is_unique_violation_on(&e, "templateId") =>
            {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let upload_url =
        files::sign_upload_url(state, &registered.object_key, &registered.content_type).await?;
    Ok(json!({
        "fileUuid": registered.uuid,
        "fieldPath": req.field_path,
        "uploadUrl": upload_url,
        "expiresInSeconds": UPLOAD_URL_TTL_SECONDS,
    }))
}

/// `confirmFile(userId, visitUuid, templateUuid, fileUuid)` (:936).
pub async fn confirm_file(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
    file_uuid: &str,
) -> ApiResult<Value> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    assert_visit_writable(&visit, user_id)?;
    let resolved = {
        let mut conn = state.db.acquire().await?;
        resolve_template_for_visit(&mut conn, &visit, template_uuid).await?
    };
    let Some(submission) = &resolved.submission else {
        return Err(not_found(
            "FORM_FILE_NOT_FOUND",
            "File not found for this form",
        ));
    };
    if submission.submitted_at.is_some() {
        return Err(already_submitted());
    }
    files::confirm_file(state, submission.id, &resolved.schema(), file_uuid).await
}

// ── §5.2 — schema + jawaban satu form ────────────────────────────────────────

/// `getForm(userId, visitUuid, templateUuid)` (:975).
pub async fn get_form(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    template_uuid: &str,
) -> ApiResult<Value> {
    let visit = find_own_visit(&state.db, user_id, visit_uuid).await?;
    let resolved = {
        let mut conn = state.db.acquire().await?;
        resolve_template_for_visit(&mut conn, &visit, template_uuid).await?
    };
    let schema = resolved.schema();
    let submission = match &resolved.submission {
        None => Value::Null,
        Some(s) => {
            let display = crate::dynamic_options::enrich_display_values(
                &state.db,
                &schema,
                Some(&s.answers),
                visit.company_id,
            )
            .await?;
            let answers = files::enrich_media_values(
                state,
                &schema,
                &s.answers,
                s.id,
                &s.uuid,
                Audience::Mobile,
            )
            .await?;
            let mut out = Map::new();
            out.insert("id".into(), Value::String(s.uuid.clone()));
            out.insert("answers".into(), answers);
            if let Some(display) = display {
                out.insert("answersDisplay".into(), Value::Object(display));
            }
            out.insert("submittedAt".into(), json!(s.submitted_at.map(ms)));
            Value::Object(out)
        }
    };
    Ok(json!({
        "template": {
            "id": resolved.template.uuid,
            "name": resolved.template.name,
            "description": resolved.template.description,
        },
        "version": {
            "id": resolved.version.uuid,
            "version": resolved.version.version,
            "schema": resolved.version.schema,
        },
        "status": status_of(resolved.submission.as_ref()),
        "submission": submission,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: i32, template_id: i32, v: i32) -> VersionRow {
        VersionRow {
            id,
            uuid: format!("v{id}"),
            version: v,
            template_id,
            schema: json!({}),
        }
    }

    fn submission(submitted: bool) -> SubmissionRow {
        SubmissionRow {
            id: 9,
            uuid: "s".into(),
            template_id: 1,
            template_version_id: 1,
            answers: json!({"a": 1}),
            submitted_at: submitted.then(|| {
                chrono::DateTime::from_timestamp_millis(1_786_230_000_000)
                    .unwrap()
                    .naive_utc()
            }),
        }
    }

    // spec: listForms — memberi status draft dan submitted sesuai submittedAt
    #[test]
    fn status_follows_submitted_at() {
        assert_eq!(status_of(None), "notStarted");
        assert_eq!(status_of(Some(&submission(false))), "draft");
        assert_eq!(status_of(Some(&submission(true))), "submitted");
    }

    // spec: memilih versi terpublish TERTINGGI kalau ada beberapa
    #[test]
    fn picks_highest_version() {
        let latest = pick_latest(vec![version(1, 7, 1), version(3, 7, 3), version(2, 7, 2)]);
        assert_eq!(latest[&7].id, 3);
    }

    // spec: response jalur tulis tidak memuat jejak revisi (§0.32)
    #[test]
    fn write_response_has_no_revision_trail() {
        let r = WriteResult {
            submission: submission(true),
            template: TemplateRow {
                id: 1,
                uuid: "t".into(),
                name: "Form".into(),
                description: None,
                sort_order: 0,
                is_active: true,
                deleted_at: None,
            },
            version: version(1, 1, 2),
            orphan_keys: vec![],
        };
        let v = to_write_response(&r);
        assert_eq!(
            v,
            json!({
                "template": {"id": "t", "name": "Form", "description": null},
                "version": {"id": "v1", "version": 2},
                "status": "submitted",
                "submission": {"id": "s", "answers": {"a": 1}, "submittedAt": 1_786_230_000_000_i64},
            })
        );
        let text = v.to_string();
        assert!(!text.contains("revision") && !text.contains("lastEdited"));
    }

    #[test]
    fn answers_size_limit() {
        assert!(assert_answers_size(&json!({"a": "x"})).is_ok());
        let big = json!({ "a": "x".repeat(MAX_ANSWERS_BYTES) });
        let e = assert_answers_size(&big).unwrap_err();
        assert_eq!(e.status().as_u16(), 413);
    }

    #[test]
    fn validation_failure_shape() {
        let e = reject_invalid_answers(&[FormAnswerIssue::new("tgl", "value is required")]);
        match e {
            ApiError::Body { status, body } => {
                assert_eq!(status.as_u16(), 422);
                assert_eq!(
                    body,
                    json!({"code": "FORM_VALIDATION_FAILED", "message": "Answers did not pass validation",
                           "details": {"errors": [{"key": "tgl", "message": "value is required"}]}})
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // spec: TIDAK mengulang untuk P2002 pada constraint lain
    #[test]
    fn unique_violation_matches_column() {
        assert!(!is_unique_violation_on(&already_submitted(), "templateId"));
        let draft = ApiError::UniqueViolation(
            "Unique constraint failed on the constraint: `storeVisitFormSubmission_visitId_templateId_key`".into(),
        );
        let request = ApiError::UniqueViolation(
            "Unique constraint failed on the constraint: `storeVisitFormSubmission_userId_clientRequestId_key`".into(),
        );
        assert!(is_unique_violation_on(&draft, "templateId"));
        assert!(!is_unique_violation_on(&draft, "clientRequestId"));
        assert!(is_unique_violation_on(&request, "clientRequestId"));
        assert!(!is_unique_violation_on(&request, "templateId"));
    }
}
