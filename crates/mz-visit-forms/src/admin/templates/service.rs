//! Port `AdminStoreVisitFormsService` bagian template/assignee/versi/validate/pratinjau/aset
//! (`admin-store-visit-forms.service.ts:241–1350`).

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::time::{now_naive, to_js_iso};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use super::catalog::{
    EMPTY_FIELD_MARK, MAX_SCHEMA_BYTES, empty_form_schema, preview_media, schema_byte_size,
    submission_pdf_filename,
};
use super::dto::{
    CreateTemplateDto, ListTemplatesQuery, UpdateTemplateDto, UpdateVersionSchemaDto,
};
use crate::admin::{company_id as require_company, time_zone};
use crate::errors::{coded, coded_details};
use crate::files;
use crate::layout_validator::collect_layout_asset_references;
use crate::pdf::{PdfStatus, SubmissionPdfData, generate_submission_pdf};
use crate::preview_sample::build_sample_answers;
use crate::schema_validator::{ValidateFormSchemaOptions, validate_form_schema};
use crate::types::{FormDocument, FormSchemaIssue};
use crate::value_util::js_normalize;
use crate::visibility::prune_hidden_values;

fn ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

fn iso(dt: NaiveDateTime) -> String {
    to_js_iso(&dt.and_utc())
}

// ── Baris ────────────────────────────────────────────────────────────────────

/// Semua kolom skalar `storeVisitFormTemplate`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TemplateRow {
    pub id: i32,
    pub uuid: String,
    pub company_id: i32,
    pub name: String,
    pub description: Option<String>,
    pub sort_order: i32,
    pub scope: String,
    pub is_active: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: Option<NaiveDateTime>,
}

const TEMPLATE_COLUMNS: &str = r#"id, uuid::text AS uuid, "companyId" AS company_id, name,
    description, "sortOrder" AS sort_order, scope::text AS scope, "isActive" AS is_active,
    "createdAt" AS created_at, "updatedAt" AS updated_at"#;

/// `VERSION_SELECT`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VersionRow {
    pub id: i32,
    pub uuid: String,
    pub version: i32,
    pub schema: Value,
    pub published_at: Option<NaiveDateTime>,
    pub published_by_id: Option<i32>,
    pub created_at: NaiveDateTime,
}

const VERSION_COLUMNS: &str = r#"id, uuid::text AS uuid, version, schema,
    "publishedAt" AS published_at, "publishedById" AS published_by_id, "createdAt" AS created_at"#;

/// Agregat versi satu template.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VersionCounts {
    pub latest_version: Option<i32>,
    pub published_version: Option<i32>,
    pub version_count: i64,
}

// ── Serializer ───────────────────────────────────────────────────────────────

/// `toTemplate(template, versions, assigneeCount = 0)`.
pub fn to_template(t: &TemplateRow, v: VersionCounts, assignee_count: i64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(t.uuid));
    m.insert("name".into(), json!(t.name));
    m.insert("description".into(), json!(t.description));
    m.insert("sortOrder".into(), json!(t.sort_order));
    m.insert("scope".into(), json!(t.scope));
    m.insert("isActive".into(), json!(t.is_active));
    m.insert("assigneeCount".into(), json!(assignee_count));
    m.insert("versionCount".into(), json!(v.version_count));
    m.insert("publishedVersion".into(), json!(v.published_version));
    m.insert(
        "hasDraftVersion".into(),
        json!(v.latest_version.is_some() && v.latest_version != v.published_version),
    );
    m.insert("createdAt".into(), json!(ms(t.created_at)));
    m.insert("updatedAt".into(), json!(t.updated_at.map(ms)));
    m
}

/// `toVersionSummary` — tanpa `schema`.
pub fn to_version_summary(v: &VersionRow) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(v.uuid));
    m.insert("version".into(), json!(v.version));
    m.insert("isPublished".into(), json!(v.published_at.is_some()));
    m.insert("publishedAt".into(), json!(v.published_at.map(ms)));
    m.insert("publishedById".into(), json!(v.published_by_id));
    m.insert("createdAt".into(), json!(ms(v.created_at)));
    m
}

/// `toVersionDetail`.
pub fn to_version_detail(v: &VersionRow) -> Map<String, Value> {
    let mut m = to_version_summary(v);
    m.insert("schema".into(), v.schema.clone());
    m
}

// ── Pencarian ────────────────────────────────────────────────────────────────

fn template_not_found() -> ApiError {
    coded(
        StatusCode::NOT_FOUND,
        "FORM_TEMPLATE_NOT_FOUND",
        "Form template not found",
    )
}

fn version_not_found() -> ApiError {
    coded(
        StatusCode::NOT_FOUND,
        "FORM_VERSION_NOT_FOUND",
        "Form version not found for this template",
    )
}

/// `findTemplateOrThrow(companyId, uuid)` — soft-deleted = tidak ada.
pub async fn find_template_or_throw(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> ApiResult<TemplateRow> {
    sqlx::query_as::<_, TemplateRow>(&format!(
        r#"SELECT {TEMPLATE_COLUMNS} FROM "storeVisitFormTemplate"
            WHERE uuid = $1::uuid AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(template_not_found)
}

async fn find_version(
    conn: &mut PgConnection,
    template_id: i32,
    version_uuid: &str,
) -> sqlx::Result<Option<VersionRow>> {
    sqlx::query_as::<_, VersionRow>(&format!(
        r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion"
            WHERE uuid = $1::uuid AND "templateId" = $2 LIMIT 1"#
    ))
    .bind(version_uuid)
    .bind(template_id)
    .fetch_optional(conn)
    .await
}

/// `findVersionOrThrow(templateId, versionUuid)`.
async fn find_version_or_throw(
    state: &AppState,
    template_id: i32,
    version_uuid: &str,
) -> ApiResult<VersionRow> {
    let mut conn = state.db.acquire().await?;
    find_version(&mut conn, template_id, version_uuid)
        .await?
        .ok_or_else(version_not_found)
}

/// Versi terpublish terakhir (`publishedAt: { not: null }`, `version desc`).
async fn latest_version(
    conn: &mut PgConnection,
    template_id: i32,
    published: bool,
    exclude_id: Option<i32>,
) -> sqlx::Result<Option<VersionRow>> {
    let cond = if published {
        r#""publishedAt" IS NOT NULL"#
    } else {
        r#""publishedAt" IS NULL"#
    };
    sqlx::query_as::<_, VersionRow>(&format!(
        r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = $1 AND {cond} AND ($2::int IS NULL OR id <> $2)
            ORDER BY version DESC LIMIT 1"#
    ))
    .bind(template_id)
    .bind(exclude_id)
    .fetch_optional(conn)
    .await
}

/// `versionSummaries(templateIds)`.
async fn version_summaries(
    state: &AppState,
    template_ids: &[i32],
) -> sqlx::Result<HashMap<i32, VersionCounts>> {
    if template_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, Option<i32>, i64, Option<i32>)> = sqlx::query_as(
        r#"SELECT "templateId", MAX(version), COUNT(*),
                  MAX(version) FILTER (WHERE "publishedAt" IS NOT NULL)
             FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = ANY($1)
            GROUP BY "templateId""#,
    )
    .bind(template_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, latest, count, published)| {
            (
                id,
                VersionCounts {
                    latest_version: latest,
                    published_version: published,
                    version_count: count,
                },
            )
        })
        .collect())
}

/// `assigneeCounts(templateIds)` — hanya baris hidup.
async fn assignee_counts(
    state: &AppState,
    template_ids: &[i32],
) -> sqlx::Result<HashMap<i32, i64>> {
    if template_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        r#"SELECT "templateId", COUNT(*) FROM "storeVisitFormTemplateUser"
            WHERE "templateId" = ANY($1) AND "deletedAt" IS NULL GROUP BY "templateId""#,
    )
    .bind(template_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `assertSchemaSize(schema)` — 413 `FORM_SCHEMA_TOO_LARGE`.
pub fn assert_schema_size(schema: Option<&Value>) -> ApiResult<()> {
    let bytes = schema_byte_size(schema);
    if bytes > MAX_SCHEMA_BYTES {
        return Err(coded_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FORM_SCHEMA_TOO_LARGE",
            format!("Schema exceeds the {MAX_SCHEMA_BYTES} byte limit"),
            json!({ "maxSizeBytes": MAX_SCHEMA_BYTES, "sizeBytes": bytes }),
        ));
    }
    Ok(())
}

// ── Template ─────────────────────────────────────────────────────────────────

/// `createTemplate(userId, dto)` (admin-store-visit-forms.service.ts:413).
pub async fn create_template(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateTemplateDto,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let schema = js_normalize(&dto.schema.clone().unwrap_or_else(empty_form_schema));
    assert_schema_size(Some(&schema))?;

    // §9.1 — rollback membuang audit (`.catch(() => discardAudit())`).
    let guard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    crate::plan_limit::assert_form_template_limit(&mut tx, company_id).await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "storeVisitFormTemplate"
             ("companyId", name, description, "sortOrder", scope, "isActive", "createdById")
           VALUES ("#,
    );
    qb.push_bind(company_id)
        .push(", ")
        .push_bind(&dto.name)
        .push(", ")
        .push_bind(&dto.description)
        .push(", ")
        .push_bind(dto.sort_order.unwrap_or(0))
        .push(", ");
    match &dto.scope {
        Some(s) => {
            qb.push_bind(s).push(r#"::"StoreVisitFormScope""#);
        }
        None => {
            qb.push("DEFAULT");
        }
    }
    qb.push(", ")
        .push_bind(dto.is_active.unwrap_or(true))
        .push(", ")
        .push_bind(user_id)
        .push(format!(") RETURNING {TEMPLATE_COLUMNS}"));
    let template: TemplateRow = qb.build_query_as().fetch_one(&mut *tx).await?;
    if let Some(row) = audit
        .result_row(
            &mut *tx,
            Model::StoreVisitFormTemplate,
            Lookup::Id(template.id),
        )
        .await
    {
        audit
            .record_create(Model::StoreVisitFormTemplate, &row)
            .await;
    }
    let version: VersionRow = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisitFormTemplateVersion" ("templateId", version, schema)
           VALUES ($1, 1, $2) RETURNING {VERSION_COLUMNS}"#
    ))
    .bind(template.id)
    .bind(&schema)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    guard.disarm();

    let mut out = to_template(
        &template,
        VersionCounts {
            latest_version: Some(1),
            published_version: None,
            version_count: 1,
        },
        0,
    );
    out.insert(
        "draftVersion".into(),
        Value::Object(to_version_detail(&version)),
    );
    out.insert("publishedVersionDetail".into(), Value::Null);
    Ok(Value::Object(out))
}

/// `findTemplates(userId, query)` (admin-store-visit-forms.service.ts:463) — `{ data, meta }`.
pub async fn find_templates(
    state: &AppState,
    user_id: i32,
    query: &ListTemplatesQuery,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND "deletedAt" IS NULL"#);
        if let Some(active) = query.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(active);
        }
        if let Some(q) = query.q.as_deref().filter(|q| !q.is_empty()) {
            // Prisma `contains` + `insensitive` tidak meng-escape `%`/`_`.
            let pattern = format!("%{q}%");
            qb.push(" AND (name ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR description ILIKE ")
                .push_bind(pattern)
                .push(")");
        }
    };
    let mut qb = QueryBuilder::new(format!(
        r#"SELECT {TEMPLATE_COLUMNS} FROM "storeVisitFormTemplate""#
    ));
    push_where(&mut qb);
    qb.push(r#" ORDER BY "sortOrder" ASC, name ASC, id ASC OFFSET "#)
        .push_bind((query.page - 1) * query.limit)
        .push(" LIMIT ")
        .push_bind(query.limit);
    let templates: Vec<TemplateRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut count_qb = QueryBuilder::new(r#"SELECT COUNT(*) FROM "storeVisitFormTemplate""#);
    push_where(&mut count_qb);
    let total: i64 = count_qb.build_query_scalar().fetch_one(&state.db).await?;

    let ids: Vec<i32> = templates.iter().map(|t| t.id).collect();
    let summaries = version_summaries(state, &ids).await?;
    let assignees = assignee_counts(state, &ids).await?;
    let data: Vec<Value> = templates
        .iter()
        .map(|t| {
            Value::Object(to_template(
                t,
                summaries.get(&t.id).copied().unwrap_or_default(),
                assignees.get(&t.id).copied().unwrap_or(0),
            ))
        })
        .collect();
    let total_pages = (total + query.limit - 1) / query.limit;
    Ok(json!({
        "data": data,
        "meta": { "total": total, "page": query.page, "limit": query.limit, "totalPages": total_pages },
    }))
}

/// `findTemplate(userId, uuid)` (admin-store-visit-forms.service.ts:516).
pub async fn find_template(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let mut conn = state.db.acquire().await?;
    let published = latest_version(&mut conn, template.id, true, None).await?;
    let draft = latest_version(&mut conn, template.id, false, None).await?;
    drop(conn);
    let summaries = version_summaries(state, &[template.id]).await?;
    let assignees = assignee_counts(state, &[template.id]).await?;
    let mut out = to_template(
        &template,
        summaries.get(&template.id).copied().unwrap_or_default(),
        assignees.get(&template.id).copied().unwrap_or(0),
    );
    out.insert(
        "publishedVersionDetail".into(),
        published.map_or(Value::Null, |v| Value::Object(to_version_detail(&v))),
    );
    out.insert(
        "draftVersion".into(),
        draft.map_or(Value::Null, |v| Value::Object(to_version_detail(&v))),
    );
    Ok(Value::Object(out))
}

/// `updateTemplate(userId, uuid, dto)` (admin-store-visit-forms.service.ts:553).
pub async fn update_template(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdateTemplateDto,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    if let Some(field) = dto.null_required.first() {
        // Prisma menolak `null` untuk kolom wajib (PrismaClientValidationError → 500).
        return Err(ApiError::internal(anyhow::anyhow!(
            "Argument `{field}` must not be null"
        )));
    }
    let pre = audit
        .pre_read(Model::StoreVisitFormTemplate, Lookup::Id(template.id))
        .await;
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(r#"UPDATE "storeVisitFormTemplate" SET "#);
    let mut sep = qb.separated(", ");
    if let Some(name) = &dto.name {
        sep.push("name = ").push_bind_unseparated(name.clone());
    }
    if let Some(d) = &dto.description {
        sep.push("description = ").push_bind_unseparated(d.clone());
    } else if dto.description_null {
        sep.push("description = NULL");
    }
    if let Some(s) = dto.sort_order {
        sep.push(r#""sortOrder" = "#).push_bind_unseparated(s);
    }
    if let Some(s) = &dto.scope {
        sep.push("scope = ")
            .push_bind_unseparated(s.clone())
            .push_unseparated(r#"::"StoreVisitFormScope""#);
    }
    if let Some(a) = dto.is_active {
        sep.push(r#""isActive" = "#).push_bind_unseparated(a);
    }
    sep.push(r#""updatedAt" = "#)
        .push_bind_unseparated(now_naive());
    qb.push(" WHERE id = ")
        .push_bind(template.id)
        .push(format!(" RETURNING {TEMPLATE_COLUMNS}"));
    let updated: TemplateRow = qb.build_query_as().fetch_one(&state.db).await?;
    if let Some(row) = audit
        .result_row(
            &state.db,
            Model::StoreVisitFormTemplate,
            Lookup::Id(template.id),
        )
        .await
    {
        audit
            .record_update(Model::StoreVisitFormTemplate, pre, &row)
            .await;
    }
    let summaries = version_summaries(state, &[template.id]).await?;
    Ok(Value::Object(to_template(
        &updated,
        summaries.get(&template.id).copied().unwrap_or_default(),
        0,
    )))
}

/// `removeTemplate(userId, uuid)` (admin-store-visit-forms.service.ts:592) — soft delete.
pub async fn remove_template(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let pre = audit
        .pre_read(Model::StoreVisitFormTemplate, Lookup::Id(template.id))
        .await;
    let now = now_naive();
    sqlx::query(
        r#"UPDATE "storeVisitFormTemplate" SET "deletedAt" = $1, "isActive" = false,
                  "updatedAt" = $1 WHERE id = $2"#,
    )
    .bind(now)
    .bind(template.id)
    .execute(&state.db)
    .await?;
    if let Some(row) = audit
        .result_row(
            &state.db,
            Model::StoreVisitFormTemplate,
            Lookup::Id(template.id),
        )
        .await
    {
        audit
            .record_update(Model::StoreVisitFormTemplate, pre, &row)
            .await;
    }
    Ok(json!({ "id": template.uuid, "deleted": true }))
}

// ── Assignment ───────────────────────────────────────────────────────────────

/// `(userId, createdAt, uuid, name, username)`.
type AssigneeRow = (
    i32,
    NaiveDateTime,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// `serializeAssignees(templateId)` — baris user yang hilang tetap tampil (`name: null`).
async fn serialize_assignees(state: &AppState, template_id: i32) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<AssigneeRow> = sqlx::query_as(
        r#"SELECT tu."userId", tu."createdAt", u.uuid::text, u.name, u.username
                 FROM "storeVisitFormTemplateUser" tu
                 LEFT JOIN users u ON u.id = tu."userId"
                WHERE tu."templateId" = $1 AND tu."deletedAt" IS NULL
                ORDER BY tu.id ASC"#,
    )
    .bind(template_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, created_at, uuid, name, username)| {
            json!({
                "id": uuid,
                "userId": user_id,
                "name": name,
                "username": username,
                "assignedAt": ms(created_at),
            })
        })
        .collect())
}

/// `findAssignees(userId, uuid)` (admin-store-visit-forms.service.ts:646) — `{ data, scope }`.
pub async fn find_assignees(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    Ok(json!({
        "data": serialize_assignees(state, template.id).await?,
        "scope": template.scope,
    }))
}

/// `[...new Set(list)]`.
pub fn dedupe(list: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    list.iter()
        .filter(|u| seen.insert(u.as_str()))
        .cloned()
        .collect()
}

/// `replaceAssignees(userId, uuid, dto)` (admin-store-visit-forms.service.ts:671).
pub async fn replace_assignees(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    user_uuids: &[String],
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;

    let requested = dedupe(user_uuids);
    let mut user_ids: Vec<i32> = Vec::new();
    if !requested.is_empty() {
        let members: Vec<(i32, String)> = sqlx::query_as(
            r#"SELECT uc."userId", u.uuid::text FROM "userCompany" uc
                 JOIN users u ON u.id = uc."userId"
                WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = ANY($2::uuid[])
                ORDER BY uc.id"#,
        )
        .bind(company_id)
        .bind(&requested)
        .fetch_all(&state.db)
        .await?;
        // `new Map(members.map(m => [uuid, userId]))` — urutan kunci = kemunculan pertama.
        let mut found: Vec<(String, i32)> = Vec::new();
        for (uid, uuid) in members {
            match found.iter_mut().find(|(u, _)| *u == uuid) {
                Some(slot) => slot.1 = uid,
                None => found.push((uuid, uid)),
            }
        }
        let missing: Vec<&String> = requested
            .iter()
            .filter(|u| !found.iter().any(|(f, _)| f == *u))
            .collect();
        if !missing.is_empty() {
            return Err(coded_details(
                StatusCode::UNPROCESSABLE_ENTITY,
                "FORM_ASSIGNEE_INVALID",
                "Every assignee must be an active employee of this company",
                json!({ "unknownUserIds": missing }),
            ));
        }
        for (_, uid) in found {
            if !user_ids.contains(&uid) {
                user_ids.push(uid);
            }
        }
    }

    let guard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    let current: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, "userId" FROM "storeVisitFormTemplateUser"
            WHERE "templateId" = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(template.id)
    .fetch_all(&mut *tx)
    .await?;
    let wanted: HashSet<i32> = user_ids.iter().copied().collect();
    let revoked: Vec<i32> = current
        .iter()
        .filter(|(_, uid)| !wanted.contains(uid))
        .map(|(id, _)| *id)
        .collect();
    if !revoked.is_empty() {
        let m = audit
            .membership_pre(
                Model::StoreVisitFormTemplateUser,
                MemberEffect::Remove,
                Touched::Where(Lookup::Ids(revoked.clone())),
            )
            .await;
        sqlx::query(
            r#"UPDATE "storeVisitFormTemplateUser" SET "deletedAt" = $2 WHERE id = ANY($1)"#,
        )
        .bind(&revoked)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
        audit.record_membership(m).await;
    }
    let kept: HashSet<i32> = current.iter().map(|(_, uid)| *uid).collect();
    let added: Vec<i32> = user_ids
        .iter()
        .copied()
        .filter(|id| !kept.contains(id))
        .collect();
    if !added.is_empty() {
        let rows: Vec<Row> = added
            .iter()
            .map(|uid| {
                Row::new()
                    .with("templateId", template.id)
                    .with("userId", *uid)
            })
            .collect();
        let m = audit
            .membership_pre(
                Model::StoreVisitFormTemplateUser,
                MemberEffect::Add,
                Touched::Rows(rows),
            )
            .await;
        // `createMany({ skipDuplicates: true })` — bersandar pada partial unique index.
        sqlx::query(
            r#"INSERT INTO "storeVisitFormTemplateUser" ("templateId", "userId")
               SELECT $1, u FROM UNNEST($2::int[]) AS t(u)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(template.id)
        .bind(&added)
        .execute(&mut *tx)
        .await?;
        audit.record_membership(m).await;
    }
    tx.commit().await?;
    guard.disarm();

    Ok(json!({
        "data": serialize_assignees(state, template.id).await?,
        "scope": template.scope,
    }))
}

// ── Versi ────────────────────────────────────────────────────────────────────

/// `findVersions(userId, uuid)` (admin-store-visit-forms.service.ts:760) — `{ data }`.
pub async fn find_versions(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let versions: Vec<VersionRow> = sqlx::query_as(&format!(
        r#"SELECT {VERSION_COLUMNS} FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = $1 ORDER BY version DESC"#
    ))
    .bind(template.id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = versions
        .iter()
        .map(|v| Value::Object(to_version_summary(v)))
        .collect();
    Ok(json!({ "data": data }))
}

/// `findVersion(userId, uuid, versionUuid)` (admin-store-visit-forms.service.ts:773).
pub async fn find_version_detail(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    version_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let version = find_version_or_throw(state, template.id, version_uuid).await?;
    Ok(Value::Object(to_version_detail(&version)))
}

/// `isVersionNumberConflict(e)` — P2002 pada `(templateId, version)`, bukan `uuid`
/// (constraint `storeVisitFormTemplateVersion_templateId_version_key`).
fn is_version_number_conflict(e: &ApiError) -> bool {
    matches!(e, ApiError::UniqueViolation(msg) if msg.contains("_version_key"))
}

/// `createVersion(userId, uuid)` (admin-store-visit-forms.service.ts:788) — §6.4 balapan 1.
pub async fn create_version(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let mut attempt = 0;
    loop {
        match try_create_version(state, template.id).await {
            Ok(v) => return Ok(Value::Object(to_version_detail(&v))),
            Err(e) if attempt < 2 && is_version_number_conflict(&e) => attempt += 1,
            Err(e) => return Err(e),
        }
    }
}

async fn try_create_version(state: &AppState, template_id: i32) -> ApiResult<VersionRow> {
    let mut tx = state.db.begin().await?;
    let existing: Option<(String, i32)> = sqlx::query_as(
        r#"SELECT uuid::text, version FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = $1 AND "publishedAt" IS NULL LIMIT 1"#,
    )
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((version_id, version)) = existing {
        return Err(coded_details(
            StatusCode::CONFLICT,
            "FORM_DRAFT_VERSION_EXISTS",
            "This template already has an unpublished draft version; edit or publish it first",
            json!({ "versionId": version_id, "version": version }),
        ));
    }
    let latest: Option<(i32, Value)> = sqlx::query_as(
        r#"SELECT version, schema FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = $1 ORDER BY version DESC LIMIT 1"#,
    )
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (next, schema) = match latest {
        Some((v, s)) => (v + 1, s),
        None => (1, empty_form_schema()),
    };
    let row: VersionRow = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisitFormTemplateVersion" ("templateId", version, schema)
           VALUES ($1, $2, $3) RETURNING {VERSION_COLUMNS}"#
    ))
    .bind(template_id)
    .bind(next)
    .bind(&schema)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

fn version_locked(message: &str) -> ApiError {
    coded(StatusCode::CONFLICT, "FORM_VERSION_LOCKED", message)
}

/// `updateVersionSchema(userId, uuid, versionUuid, dto)` (admin-store-visit-forms.service.ts:881).
pub async fn update_version_schema(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    version_uuid: &str,
    dto: &UpdateVersionSchemaDto,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let version = find_version_or_throw(state, template.id, version_uuid).await?;
    if version.published_at.is_some() {
        return Err(version_locked("A published version cannot be modified"));
    }
    let schema = js_normalize(&dto.schema);
    assert_schema_size(Some(&schema))?;

    let expected = DateTime::<Utc>::from_timestamp_millis(dto.expected_template_updated_at)
        .map(|d| d.naive_utc())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))?;
    let updated_at = now_naive();

    let guard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    // Compare-and-set `updatedAt` template (pagar dua admin). `updateMany` ber-audit.
    let template_id = template.id;
    let pre = audit
        .pre_read_many(
            Model::StoreVisitFormTemplate,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(template_id)
                    .push(r#" AND "updatedAt" = "#)
                    .push_bind(expected);
            }),
        )
        .await;
    let fence = sqlx::query(
        r#"UPDATE "storeVisitFormTemplate" SET "updatedAt" = $1 WHERE id = $2 AND "updatedAt" = $3"#,
    )
    .bind(updated_at)
    .bind(template.id)
    .bind(expected)
    .execute(&mut *tx)
    .await?;
    audit
        .record_update_many(
            Model::StoreVisitFormTemplate,
            pre,
            fence.rows_affected(),
            &Row::new().with("updatedAt", iso(updated_at)),
            json!({ "id": template.id, "updatedAt": iso(expected) }),
        )
        .await;
    if fence.rows_affected() == 0 {
        let current: Option<Option<NaiveDateTime>> =
            sqlx::query_scalar(r#"SELECT "updatedAt" FROM "storeVisitFormTemplate" WHERE id = $1"#)
                .bind(template.id)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(coded_details(
            StatusCode::CONFLICT,
            "FORM_TEMPLATE_STALE",
            "This template changed since you opened it; reload before saving again",
            json!({
                "expectedUpdatedAt": dto.expected_template_updated_at,
                "currentUpdatedAt": current.flatten().map(ms),
            }),
        ));
    }
    let written = sqlx::query(
        r#"UPDATE "storeVisitFormTemplateVersion" SET schema = $1
            WHERE id = $2 AND "publishedAt" IS NULL"#,
    )
    .bind(&schema)
    .bind(version.id)
    .execute(&mut *tx)
    .await?;
    if written.rows_affected() == 0 {
        return Err(version_locked("A published version cannot be modified"));
    }
    tx.commit().await?;
    guard.disarm();

    let mut out = to_version_detail(&VersionRow { schema, ..version });
    out.insert("templateUpdatedAt".into(), json!(ms(updated_at)));
    Ok(Value::Object(out))
}

/// `layoutAssetIssues(client, templateId, schema)` — aset kop/kaki yang bukan unggahan
/// terkonfirmasi template ini.
async fn layout_asset_issues(
    conn: &mut PgConnection,
    template_id: i32,
    schema: &Value,
) -> sqlx::Result<Vec<FormSchemaIssue>> {
    let refs = collect_layout_asset_references(schema);
    let uuids: Vec<String> = refs.iter().map(|r| r.asset_uuid.clone()).collect();
    let missing = files::find_missing_template_assets(conn, template_id, &uuids).await?;
    Ok(refs
        .into_iter()
        .filter(|r| missing.contains(&r.asset_uuid))
        .map(|r| {
            FormSchemaIssue::new(
                r.path,
                format!(
                    "asset \"{}\" is not a confirmed upload of this template",
                    r.asset_uuid
                ),
            )
        })
        .collect())
}

/// `publishVersion(userId, uuid, versionUuid)` (admin-store-visit-forms.service.ts:974).
pub async fn publish_version(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    version_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let tz = time_zone(state, company_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;

    let mut tx = state.db.begin().await?;
    let version = find_version(&mut tx, template.id, version_uuid)
        .await?
        .ok_or_else(version_not_found)?;
    if version.published_at.is_some() {
        return Err(version_locked("This version has already been published"));
    }
    let previous = latest_version(&mut tx, template.id, true, Some(version.id)).await?;
    let mut issues = validate_form_schema(
        Some(&version.schema),
        &ValidateFormSchemaOptions {
            previous_schema: previous.as_ref().map(|p| &p.schema),
            time_zone: Some(&tz),
        },
    );
    issues.extend(layout_asset_issues(&mut tx, template.id, &version.schema).await?);
    if !issues.is_empty() {
        return Err(coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FORM_SCHEMA_INVALID",
            "Schema failed validation and cannot be published",
            json!({ "errors": issues }),
        ));
    }
    let published_at = now_naive();
    let res = sqlx::query(
        r#"UPDATE "storeVisitFormTemplateVersion" SET "publishedAt" = $1, "publishedById" = $2
            WHERE id = $3 AND "publishedAt" IS NULL"#,
    )
    .bind(published_at)
    .bind(user_id)
    .bind(version.id)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(version_locked("This version has already been published"));
    }
    tx.commit().await?;
    Ok(Value::Object(to_version_detail(&VersionRow {
        published_at: Some(published_at),
        published_by_id: Some(user_id),
        ..version
    })))
}

/// `validateSchema(userId, uuid, dto)` (admin-store-visit-forms.service.ts:1057) — `{ errors }`.
pub async fn validate_schema(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    schema: &Value,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let tz = time_zone(state, company_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    assert_schema_size(Some(schema))?;
    let mut conn = state.db.acquire().await?;
    let previous = latest_version(&mut conn, template.id, true, None).await?;
    let mut errors = validate_form_schema(
        Some(schema),
        &ValidateFormSchemaOptions {
            previous_schema: previous.as_ref().map(|p| &p.schema),
            time_zone: Some(&tz),
        },
    );
    errors.extend(layout_asset_issues(&mut conn, template.id, schema).await?);
    Ok(json!({ "errors": errors }))
}

/// `generatePreviewPdf(userId, uuid, dto)` (admin-store-visit-forms.service.ts:1121) →
/// `(pdf, filename)`.
pub async fn generate_preview_pdf(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    schema_value: &Value,
) -> ApiResult<(Vec<u8>, String)> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    assert_schema_size(Some(schema_value))?;
    let company: Option<(String, String)> =
        sqlx::query_as(r#"SELECT name, "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let requester: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
    let tz = company
        .as_ref()
        .map(|(_, tz)| tz.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "UTC".to_owned());

    let issues = validate_form_schema(
        Some(schema_value),
        &ValidateFormSchemaOptions {
            previous_schema: None,
            time_zone: Some(&tz),
        },
    );
    if !issues.is_empty() {
        return Err(coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FORM_SCHEMA_INVALID",
            "Fix the schema problems before previewing the PDF",
            json!({ "errors": issues }),
        ));
    }
    let schema = FormDocument::from_value(schema_value);
    let layout_media = files::load_layout_media_for_pdf(state, schema_value, template.id).await?;
    let now = Utc::now();
    let now = DateTime::<Utc>::from_timestamp_millis(now.timestamp_millis()).unwrap_or(now);
    let answers = prune_hidden_values(&schema, &build_sample_answers(&schema, &tz));
    let data = SubmissionPdfData {
        company_name: company.map(|(n, _)| n).unwrap_or_default(),
        template_name: template.name.clone(),
        template_version: None,
        status: PdfStatus::Submitted,
        submitted_at: Some(now),
        created_at: now,
        sales_name: "Nama Sales".to_owned(),
        store_name: "Toko Contoh".to_owned(),
        store_code: Some("TK-001".to_owned()),
        visit_number: "SV-PRATINJAU".to_owned(),
        check_in_at: now - Duration::hours(1),
        check_out_at: Some(now),
        revision_count: 0,
        last_edited_at: None,
        last_edited_by_name: None,
        schema,
        answers,
        media: preview_media().clone(),
        layout_media: Some(layout_media),
        time_zone: tz,
        generated_at: now,
        generated_by: requester.unwrap_or_else(|| EMPTY_FIELD_MARK.to_owned()),
    };
    let buffer = tokio::task::spawn_blocking(move || generate_submission_pdf(&data))
        .await
        .map_err(ApiError::internal)?;
    Ok((
        buffer,
        submission_pdf_filename(&template.name, Some("pratinjau"), &template.uuid),
    ))
}

// ── Aset kop/kaki ───────────────────────────────────────────────────────────

/// `createTemplateAssetUpload(userId, uuid, dto)` (admin-store-visit-forms.service.ts:1206).
pub async fn create_template_asset_upload(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    content_type: &str,
    size_bytes: i64,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    let upload = files::register_template_asset(
        state,
        template.id,
        &template.uuid,
        company_id,
        user_id,
        content_type,
        size_bytes,
    )
    .await?;
    let mut out = serde_json::to_value(&upload).map_err(ApiError::internal)?;
    if let Some(m) = out.as_object_mut() {
        m.insert(
            "expiresInSeconds".into(),
            json!(files::UPLOAD_URL_TTL_SECONDS),
        );
    }
    Ok(out)
}

/// `confirmTemplateAsset(userId, uuid, assetUuid)` (admin-store-visit-forms.service.ts:1224).
pub async fn confirm_template_asset(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    asset_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company(state, user_id).await?;
    let template = find_template_or_throw(state, company_id, uuid).await?;
    files::confirm_template_asset(state, template.id, asset_uuid).await
}

/// Template milik company peminta (`streamTemplateAsset` / `templateAssetUrl` prolog).
pub async fn template_for(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<TemplateRow> {
    let company_id = require_company(state, user_id).await?;
    find_template_or_throw(state, company_id, uuid).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(ms_: i64) -> NaiveDateTime {
        DateTime::<Utc>::from_timestamp_millis(ms_)
            .unwrap()
            .naive_utc()
    }

    fn template() -> TemplateRow {
        TemplateRow {
            id: 1,
            uuid: "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f".into(),
            company_id: 7,
            name: "Laporan".into(),
            description: None,
            sort_order: 0,
            scope: "all".into(),
            is_active: true,
            created_at: at(1_786_000_000_000),
            updated_at: None,
        }
    }

    fn version(v: i32, published: bool) -> VersionRow {
        VersionRow {
            id: v,
            uuid: format!("v{v}"),
            version: v,
            schema: json!({"schemaVersion": 1, "fields": []}),
            published_at: published.then(|| at(1_786_050_000_000)),
            published_by_id: published.then_some(42),
            created_at: at(1_786_100_000_000),
        }
    }

    #[test]
    fn template_shape_and_draft_flag() {
        let t = template();
        let with_draft = to_template(
            &t,
            VersionCounts {
                latest_version: Some(2),
                published_version: Some(1),
                version_count: 2,
            },
            3,
        );
        assert_eq!(
            Value::Object(with_draft),
            json!({
                "id": t.uuid, "name": "Laporan", "description": null, "sortOrder": 0,
                "scope": "all", "isActive": true, "assigneeCount": 3, "versionCount": 2,
                "publishedVersion": 1, "hasDraftVersion": true,
                "createdAt": 1_786_000_000_000_i64, "updatedAt": null
            })
        );
        let all_published = to_template(
            &t,
            VersionCounts {
                latest_version: Some(2),
                published_version: Some(2),
                version_count: 2,
            },
            0,
        );
        assert_eq!(all_published["hasDraftVersion"], json!(false));
        let none = to_template(&t, VersionCounts::default(), 0);
        assert_eq!(none["hasDraftVersion"], json!(false));
        assert_eq!(none["versionCount"], json!(0));
    }

    #[test]
    fn version_shapes() {
        let summary = to_version_summary(&version(2, false));
        assert!(!summary.contains_key("schema"));
        assert_eq!(summary["isPublished"], json!(false));
        assert_eq!(summary["publishedAt"], Value::Null);
        let keys: Vec<&String> = summary.keys().collect();
        assert_eq!(
            keys,
            [
                "id",
                "version",
                "isPublished",
                "publishedAt",
                "publishedById",
                "createdAt"
            ]
        );
        let detail = to_version_detail(&version(1, true));
        assert_eq!(detail["publishedById"], json!(42));
        assert_eq!(detail["publishedAt"], json!(1_786_050_000_000_i64));
        assert!(detail.contains_key("schema"));
    }

    #[test]
    fn schema_size_limit() {
        assert!(assert_schema_size(Some(&json!({"fields": []}))).is_ok());
        let big = json!({ "x": "a".repeat(MAX_SCHEMA_BYTES) });
        let Err(ApiError::Body { status, body }) = assert_schema_size(Some(&big)) else {
            panic!("harus 413");
        };
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["code"], "FORM_SCHEMA_TOO_LARGE");
        assert_eq!(body["message"], "Schema exceeds the 262144 byte limit");
        assert_eq!(body["details"]["maxSizeBytes"], 262_144);
    }

    #[test]
    fn dedupes_in_order() {
        let v = vec!["b".to_owned(), "a".to_owned(), "b".to_owned()];
        assert_eq!(dedupe(&v), ["b", "a"]);
    }

    #[test]
    fn ms_is_epoch_millis() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_milli_opt(1, 2, 3, 456)
            .unwrap();
        assert_eq!(ms(d), 1_794_877_323_456);
        assert_eq!(iso(d), "2026-11-17T01:02:03.456Z");
    }
}
