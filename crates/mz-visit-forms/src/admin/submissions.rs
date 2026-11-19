//! Port bagian submission `admin-store-visit-forms.controller.ts` + `admin-store-visit-forms.service.ts`
//! (§5.1, §6.8, §7.3, §0.3): daftar/detail/koreksi/riwayat revisi submission, ekspor PDF,
//! dan proxy file jawaban untuk dashboard.
//!
//! Titik temu modul store-visits (tahap S3): tabel `storeVisit` (nomor kunjungan, check-in/out
//! untuk PDF, terjemahan `visitUuid` → id), `store` (uuid, kode), `users` (uuid, nama),
//! `userCompany` (terjemahan `userUuid`), `company` (nama, timezone) dibaca lewat SQL di
//! [`visit_lookup`] dan fungsi `lookup_*` di bawah. Tidak ada model ber-audit yang ditulis
//! (`storeVisitFormSubmission`/`Revision` bukan model audit).

use std::collections::{HashMap, HashSet};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDateTime, Utc};
use mz_core::extract::{Validate, ValidatedJson, validator_js_length};
use mz_core::gate::guard;
use mz_core::rate_limit::RateRule;
use mz_core::uuid_rules::{is_uuid, uuid_pipe};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use super::{ADMIN, MINUTE, company_id, limits, time_zone};
use crate::answers_validator::{
    AnswerValidationMode, ValidateAnswersOptions, answers_byte_size, validate_answers,
};
use crate::consts::{MAX_ANSWERS_BYTES, status};
use crate::dynamic_options::enrich_display_values;
use crate::errors::{coded, coded_details, not_found};
use crate::files::{
    Audience, company_file_url, enrich_media_values, load_layout_media_for_pdf, load_media_for_pdf,
    stream_company_file,
};
use crate::pdf::{PdfStatus, SubmissionPdfData, generate_submission_pdf};
use crate::revise_policy::{assert_can_revise, can_revise};
use crate::types::FormDocument;
use crate::value_util::{js_trim, local_to_utc_ms, parse_calendar_date, zone};
use crate::visibility::prune_hidden_values;

/// Sama dengan yang dipakai generator PDF untuk nilai yang tidak ada.
pub const EMPTY_FIELD_MARK: &str = "—";

/// `main.ts` `adminFormsPdfLimiter` — 10/menit per token.
pub const PDF_LIMIT: RateRule = RateRule::token("rl:admin-store-visit-forms-pdf", 10, MINUTE);

const BASE: &str = "/v1/admin/store-visit-forms/submissions";

/// GET list/detail/revisions (`adminFormsReadLimiter`).
const READ: RouteSpec = ADMIN.rate_limits(&[limits::READ]);
/// PATCH koreksi (`adminFormsCorrectLimiter`).
const CORRECT: RouteSpec = ADMIN.rate_limits(&[limits::CORRECT]);
/// GET pdf (`adminFormsPdfLimiter`).
const PDF: RouteSpec = ADMIN.rate_limits(&[PDF_LIMIT]);
/// Proxy file — tanpa limiter (main.ts F2).
const FILE: RouteSpec = ADMIN;
/// `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`.
const FILE_URL: RouteSpec =
    ADMIN.headers(&[("cache-control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]);

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(BASE, guard(state, READ, get(h_find_submissions)))
        .route(
            &format!("{BASE}/{{uuid}}"),
            guard(state, READ, get(h_find_submission)),
        )
        .route(
            &format!("{BASE}/{{uuid}}"),
            guard(state, CORRECT, patch(h_correct_submission)),
        )
        .route(
            &format!("{BASE}/{{uuid}}/revisions"),
            guard(state, READ, get(h_find_revisions)),
        )
        .route(
            &format!("{BASE}/{{uuid}}/pdf"),
            guard(state, PDF, get(h_download_pdf)),
        )
        .route(
            &format!("{BASE}/{{uuid}}/files/{{file_uuid}}"),
            guard(state, FILE, get(h_submission_file)),
        )
        .route(
            &format!("{BASE}/{{uuid}}/files/{{file_uuid}}/url"),
            guard(state, FILE_URL, get(h_submission_file_url)),
        )
}

// ── Pembantu JS ──────────────────────────────────────────────────────────────

/// `String(v)` untuk nilai query (`string` | array | object).
fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(js_string_of).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(mz_core::jsfmt::num_string)
            .unwrap_or_default(),
    }
}

/// `Number(value)` JS untuk nilai body JSON.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => mz_core::jsfmt::number(s),
        Value::Array(_) => mz_core::jsfmt::number(&js_string_of(v)),
        Value::Object(_) => f64::NAN,
    }
}

fn is_int(x: f64) -> bool {
    x.is_finite() && x.fract() == 0.0
}

/// `Date.getTime()` kolom `timestamp(3)` (UTC).
fn ms(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_millis()
}

fn ms_opt(t: Option<NaiveDateTime>) -> Value {
    t.map_or(Value::Null, |t| Value::from(ms(t)))
}

// ── DTO ──────────────────────────────────────────────────────────────────────

/// `FormSubmissionStatusFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Submitted,
    Draft,
    All,
}

impl StatusFilter {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "submitted" => Some(Self::Submitted),
            "draft" => Some(Self::Draft),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// `ListFormSubmissionsQueryDto` sesudah `ValidationPipe({ transform: true })`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub template_uuid: Option<String>,
    pub user_uuid: Option<String>,
    pub store_uuid: Option<String>,
    pub visit_uuid: Option<String>,
    pub status: StatusFilter,
    pub page: i64,
    pub limit: i64,
}

/// `@Transform(value !== undefined ? parseInt(value, 10) : d)` → `None` = NaN.
fn parse_int_param(v: Option<&Value>, default: i64) -> Option<f64> {
    match v {
        None => Some(default as f64),
        Some(v) => mz_core::jsfmt::parse_int(&js_string_of(v)).map(|n| n as f64),
    }
}

/// `ValidationPipe` atas `ListFormSubmissionsQueryDto` (pesan per properti, dekorator
/// dievaluasi bawah → atas).
pub fn parse_list_query(q: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut errors = Vec::new();
    // `@IsOptional()`: hanya `null`/`undefined` yang dilewati (query tidak pernah null).
    let present = |k: &str| q.get(k).filter(|v| !v.is_null());

    let mut day = |field: &str| -> Option<String> {
        let v = present(field)?;
        match v.as_str() {
            Some(s) if crate::consts::is_date_shape(s) => Some(s.to_owned()),
            _ => {
                errors.push(format!("{field} must be YYYY-MM-DD"));
                None
            }
        }
    };
    let date_from = day("dateFrom");
    let date_to = day("dateTo");

    let mut uuid_field = |field: &str| -> Option<String> {
        let v = present(field)?;
        match v.as_str() {
            Some(s) if is_uuid(s) => Some(s.to_owned()),
            _ => {
                errors.push(format!("{field} must be a UUID"));
                None
            }
        }
    };
    let template_uuid = uuid_field("templateUuid");
    let user_uuid = uuid_field("userUuid");
    let store_uuid = uuid_field("storeUuid");
    let visit_uuid = uuid_field("visitUuid");

    let status = match present("status") {
        None => StatusFilter::Submitted,
        Some(v) => match v.as_str().and_then(StatusFilter::parse) {
            Some(s) => s,
            None => {
                errors.push(
                    "status must be one of the following values: submitted, draft, all".to_owned(),
                );
                StatusFilter::Submitted
            }
        },
    };

    // page: IsOptional, Min(1), IsInt.
    let page = parse_int_param(q.get("page"), 1);
    if page.is_none_or(|p| p < 1.0) {
        errors.push("page must not be less than 1".to_owned());
    }
    if page.is_none_or(|p| !is_int(p)) {
        errors.push("page must be an integer number".to_owned());
    }
    // limit: IsOptional, Max(100), Min(1), IsInt.
    let limit = parse_int_param(q.get("limit"), 20);
    let mut limit_errors = Vec::new();
    let limit_ok = mz_core::extract::check_int_range(&mut limit_errors, "limit", limit, 1, 100);
    errors.extend(limit_errors);

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(ListQuery {
        date_from,
        date_to,
        template_uuid,
        user_uuid,
        store_uuid,
        visit_uuid,
        status,
        page: page.map_or(1, |p| p as i64),
        limit: limit_ok.unwrap_or(20),
    })
}

/// Nilai body apa adanya: `null` → `Some(Null)`, absen → `None` (lewat `default`).
fn present_value<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `CorrectFormSubmissionDto` mentah.
#[derive(Debug, Deserialize)]
pub struct CorrectBody {
    #[serde(default, deserialize_with = "present_value")]
    pub answers: Option<Value>,
    #[serde(default, deserialize_with = "present_value")]
    pub reason: Option<Value>,
    #[serde(
        default,
        rename = "expectedRevision",
        deserialize_with = "present_value"
    )]
    pub expected_revision: Option<Value>,
}

/// `CorrectFormSubmissionDto` sesudah transform + validasi.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectDto {
    pub answers: Value,
    pub reason: String,
    pub expected_revision: i64,
}

impl CorrectBody {
    /// `@Transform` + class-validator, pesan persis.
    pub fn check(&self) -> Result<CorrectDto, Vec<String>> {
        let mut e = Vec::new();
        // answers: @IsObject.
        let answers = self.answers.as_ref().filter(|v| v.is_object());
        if answers.is_none() {
            e.push("answers must be an object".to_owned());
        }
        // reason: trim → MaxLength(500), MinLength(1), IsString.
        let reason = self.reason.as_ref().and_then(Value::as_str).map(js_trim);
        if !reason.is_some_and(|s| validator_js_length(s) <= 500) {
            e.push("reason must be shorter than or equal to 500 characters".to_owned());
        }
        if !reason.is_some_and(|s| validator_js_length(s) >= 1) {
            e.push("reason must be longer than or equal to 1 characters".to_owned());
        }
        if reason.is_none() {
            e.push("reason must be a string".to_owned());
        }
        // expectedRevision: Number(value) bila dikirim → Min(0), IsInt.
        let rev = self.expected_revision.as_ref().map(js_number);
        if !rev.is_some_and(|r| r >= 0.0) {
            e.push("expectedRevision must not be less than 0".to_owned());
        }
        if !rev.is_some_and(is_int) {
            e.push("expectedRevision must be an integer number".to_owned());
        }
        match (answers, reason, rev) {
            (Some(a), Some(r), Some(n)) if e.is_empty() => Ok(CorrectDto {
                answers: a.clone(),
                reason: r.to_owned(),
                expected_revision: n as i64,
            }),
            _ => Err(e),
        }
    }
}

impl Validate for CorrectBody {
    fn validate(&self) -> Vec<String> {
        self.check().err().unwrap_or_default()
    }
}

// ── Baris & lookup ───────────────────────────────────────────────────────────

/// `SUBMISSION_LIST_SELECT` (+ `id`, `answers` untuk detail).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubmissionRow {
    pub id: i32,
    pub uuid: String,
    pub user_id: i32,
    pub visit_id: i32,
    pub store_id: i32,
    pub store_name: String,
    pub visit_uuid: String,
    pub visit_check_in_at: NaiveDateTime,
    pub template_id: i32,
    pub template_version_id: i32,
    pub submitted_at: Option<NaiveDateTime>,
    pub revision_count: i32,
    pub last_edited_at: Option<NaiveDateTime>,
    pub last_edited_by_id: Option<i32>,
    pub created_at: NaiveDateTime,
    /// `Value::Null` pada list (answers tidak dibaca).
    pub answers: Value,
}

const LIST_COLUMNS: &str = r#"s.id, s.uuid::text AS uuid, s."userId" AS user_id, s."visitId" AS visit_id,
       s."storeId" AS store_id, s."storeName" AS store_name, s."visitUuid"::text AS visit_uuid,
       s."visitCheckInAt" AS visit_check_in_at, s."templateId" AS template_id,
       s."templateVersionId" AS template_version_id, s."submittedAt" AS submitted_at,
       s."revisionCount" AS revision_count, s."lastEditedAt" AS last_edited_at,
       s."lastEditedById" AS last_edited_by_id, s."createdAt" AS created_at"#;

#[derive(Debug, Clone)]
pub struct UserRef {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct TemplateRef {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct VersionRef {
    pub uuid: String,
    pub version: i32,
}

/// Hasil `lookupPeopleAndStores`.
#[derive(Debug, Clone, Default)]
pub struct Lookups {
    pub users: HashMap<i32, UserRef>,
    pub stores: HashMap<i32, String>,
    pub templates: HashMap<i32, TemplateRef>,
    pub versions: HashMap<i32, VersionRef>,
}

fn unique<T: Copy + Eq + std::hash::Hash>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|x| seen.insert(*x)).collect()
}

/// `lookupPeopleAndStores(rows)` — tiap lookup dilewati kalau daftar id-nya kosong.
async fn lookup_people_and_stores(state: &AppState, rows: &[SubmissionRow]) -> ApiResult<Lookups> {
    let user_ids = unique(
        rows.iter()
            .map(|r| r.user_id)
            .chain(rows.iter().filter_map(|r| r.last_edited_by_id)),
    );
    let store_ids = unique(rows.iter().map(|r| r.store_id));
    let template_ids = unique(rows.iter().map(|r| r.template_id));
    let version_ids = unique(rows.iter().map(|r| r.template_version_id));
    let mut out = Lookups::default();
    if !user_ids.is_empty() {
        out.users = lookup_users(state, &user_ids).await?;
    }
    if !store_ids.is_empty() {
        let rows: Vec<(i32, String)> =
            sqlx::query_as(r#"SELECT id, uuid::text FROM store WHERE id = ANY($1)"#)
                .bind(&store_ids)
                .fetch_all(&state.db)
                .await?;
        out.stores = rows.into_iter().collect();
    }
    if !template_ids.is_empty() {
        let rows: Vec<(i32, String, String)> = sqlx::query_as(
            r#"SELECT id, uuid::text, name FROM "storeVisitFormTemplate" WHERE id = ANY($1)"#,
        )
        .bind(&template_ids)
        .fetch_all(&state.db)
        .await?;
        out.templates = rows
            .into_iter()
            .map(|(id, uuid, name)| (id, TemplateRef { uuid, name }))
            .collect();
    }
    if !version_ids.is_empty() {
        let rows: Vec<(i32, String, i32)> = sqlx::query_as(
            r#"SELECT id, uuid::text, version FROM "storeVisitFormTemplateVersion" WHERE id = ANY($1)"#,
        )
        .bind(&version_ids)
        .fetch_all(&state.db)
        .await?;
        out.versions = rows
            .into_iter()
            .map(|(id, uuid, version)| (id, VersionRef { uuid, version }))
            .collect();
    }
    Ok(out)
}

/// `users.findMany({ where: { id: { in } }, select: { id, uuid, name } })`.
async fn lookup_users(state: &AppState, ids: &[i32]) -> ApiResult<HashMap<i32, UserRef>> {
    let rows: Vec<(i32, String, String)> =
        sqlx::query_as(r#"SELECT id, uuid::text, name FROM users WHERE id = ANY($1)"#)
            .bind(ids)
            .fetch_all(&state.db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, uuid, name)| (id, UserRef { uuid, name }))
        .collect())
}

/// `toSubmission(row, lookups)`.
pub fn to_submission(row: &SubmissionRow, lookups: &Lookups) -> Map<String, Value> {
    let user = lookups.users.get(&row.user_id);
    let editor = row.last_edited_by_id.and_then(|id| lookups.users.get(&id));
    let template = lookups.templates.get(&row.template_id);
    let version = lookups.versions.get(&row.template_version_id);
    let last_edited_by = match (editor, row.last_edited_by_id) {
        (Some(e), _) => json!({ "id": e.uuid, "name": e.name }),
        // `lastEditedById` 0 bersifat falsy di JS.
        (None, Some(id)) if id != 0 => json!({ "id": null, "name": null }),
        _ => Value::Null,
    };
    let v = json!({
        "id": row.uuid,
        "status": if row.submitted_at.is_some() { status::SUBMITTED } else { status::DRAFT },
        "submittedAt": ms_opt(row.submitted_at),
        "createdAt": ms(row.created_at),
        "template": match template {
            Some(t) => json!({ "id": t.uuid, "name": t.name }),
            None => json!({ "id": null, "name": null }),
        },
        "version": match version {
            Some(v) => json!({ "id": v.uuid, "version": v.version }),
            None => json!({ "id": null, "version": null }),
        },
        "user": {
            "id": user.map(|u| u.uuid.clone()),
            "name": user.map(|u| u.name.clone()),
        },
        "store": {
            "id": lookups.stores.get(&row.store_id),
            "name": row.store_name,
        },
        "visit": { "id": row.visit_uuid, "checkInAt": ms(row.visit_check_in_at) },
        "revisionCount": row.revision_count,
        "lastEditedAt": ms_opt(row.last_edited_at),
        "lastEditedBy": last_edited_by,
    });
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Batas `visitCheckInAt` (`resolveDateRange`) — `None` = Invalid Date (Prisma 500 di Node).
pub fn day_bound_ms(day: &str, time_zone: &str, end: bool) -> Option<i64> {
    let d = parse_calendar_date(day)?;
    let tz = zone(time_zone);
    if end {
        let next = d.succ_opt()?;
        return Some(local_to_utc_ms(&tz, next.and_time(chrono::NaiveTime::MIN))? - 1);
    }
    local_to_utc_ms(&tz, d.and_time(chrono::NaiveTime::MIN))
}

fn naive_from_ms(ms: i64) -> Option<NaiveDateTime> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|t| t.naive_utc())
}

fn invalid_date() -> ApiError {
    ApiError::internal(anyhow::anyhow!("Invalid Date in visitCheckInAt filter"))
}

/// `resolveSubmissionFilters` — uuid yang bukan milik company → `-1` (nol baris).
async fn resolve_filters(
    state: &AppState,
    company_id: i32,
    q: &ListQuery,
) -> ApiResult<[Option<i32>; 4]> {
    let one = |sql: &'static str, uuid: Option<String>| {
        let db = state.db.clone();
        async move {
            let Some(uuid) = uuid else {
                return Ok::<_, sqlx::Error>(None);
            };
            let id: Option<i32> = sqlx::query_scalar(sql)
                .bind(uuid)
                .bind(company_id)
                .fetch_optional(&db)
                .await?;
            Ok(Some(id.unwrap_or(-1)))
        }
    };
    let (t, u, s, v) = futures::try_join!(
        one(
            r#"SELECT id FROM "storeVisitFormTemplate" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
            q.template_uuid.clone()
        ),
        one(
            r#"SELECT uc."userId" FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
                WHERE u.uuid = $1::uuid AND uc."companyId" = $2 ORDER BY uc.id LIMIT 1"#,
            q.user_uuid.clone()
        ),
        one(
            r#"SELECT id FROM store WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
            q.store_uuid.clone()
        ),
        visit_lookup::visit_id_by_uuid(state, q.visit_uuid.clone(), company_id),
    )?;
    Ok([t, u, s, v])
}

/// Titik temu tabel `storeVisit`/`store` (tahap S3: alihkan ke `mz-store-visits`).
pub mod visit_lookup {
    use chrono::NaiveDateTime;
    use mz_core::AppState;

    /// `storeVisit.findFirst({ where: { uuid, companyId }, select: { id } })` — `-1` bila tidak ada.
    pub async fn visit_id_by_uuid(
        state: &AppState,
        uuid: Option<String>,
        company_id: i32,
    ) -> sqlx::Result<Option<i32>> {
        let Some(uuid) = uuid else {
            return Ok(None);
        };
        let id: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "storeVisit" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        Ok(Some(id.unwrap_or(-1)))
    }

    /// Konteks kunjungan untuk PDF: `{ visitNumber, checkInAt, checkOutAt }`.
    #[derive(Debug, Clone, sqlx::FromRow)]
    pub struct VisitForPdf {
        pub visit_number: String,
        pub check_in_at: NaiveDateTime,
        pub check_out_at: Option<NaiveDateTime>,
    }

    /// `storeVisit.findFirst({ where: { id, companyId } })`.
    pub async fn visit_for_pdf(
        state: &AppState,
        visit_id: i32,
        company_id: i32,
    ) -> sqlx::Result<Option<VisitForPdf>> {
        sqlx::query_as(
            r#"SELECT "visitNumber" AS visit_number, "checkInAt" AS check_in_at,
                      "checkOutAt" AS check_out_at
                 FROM "storeVisit" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(visit_id)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await
    }

    /// `store.findFirst({ where: { id, companyId }, select: { code } })`.
    pub async fn store_code(
        state: &AppState,
        store_id: i32,
        company_id: i32,
    ) -> sqlx::Result<Option<String>> {
        sqlx::query_scalar(r#"SELECT code FROM store WHERE id = $1 AND "companyId" = $2 LIMIT 1"#)
            .bind(store_id)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await
    }
}

type DateRange = (Option<NaiveDateTime>, Option<NaiveDateTime>);

fn push_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    filters: &[Option<i32>; 4],
    q: &ListQuery,
    range: DateRange,
) {
    qb.push(r#" WHERE s."companyId" = "#).push_bind(company_id);
    match q.status {
        StatusFilter::All => {}
        StatusFilter::Draft => {
            qb.push(r#" AND s."submittedAt" IS NULL"#);
        }
        StatusFilter::Submitted => {
            qb.push(r#" AND s."submittedAt" IS NOT NULL"#);
        }
    }
    let cols = [
        r#""templateId""#,
        r#""userId""#,
        r#""storeId""#,
        r#""visitId""#,
    ];
    for (col, value) in cols.iter().zip(filters) {
        if let Some(v) = value {
            qb.push(" AND s.").push(*col).push(" = ").push_bind(*v);
        }
    }
    if let Some(gte) = range.0 {
        qb.push(r#" AND s."visitCheckInAt" >= "#).push_bind(gte);
    }
    if let Some(lte) = range.1 {
        qb.push(r#" AND s."visitCheckInAt" <= "#).push_bind(lte);
    }
}

/// `findSubmissions(userId, query)`.
async fn find_submissions(state: &AppState, user_id: i32, q: &ListQuery) -> ApiResult<Value> {
    let company_id = company_id(state, user_id).await?;
    let tz = time_zone(state, company_id).await?;
    let filters = resolve_filters(state, company_id, q).await?;
    let bound = |day: &Option<String>, end: bool| -> ApiResult<Option<NaiveDateTime>> {
        match day {
            Some(d) => Ok(Some(
                day_bound_ms(d, &tz, end)
                    .and_then(naive_from_ms)
                    .ok_or_else(invalid_date)?,
            )),
            None => Ok(None),
        }
    };
    let range = (bound(&q.date_from, false)?, bound(&q.date_to, true)?);

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT {LIST_COLUMNS}, 'null'::jsonb AS answers FROM "storeVisitFormSubmission" s"#
    ));
    push_where(&mut qb, company_id, &filters, q, range);
    qb.push(r#" ORDER BY s."visitCheckInAt" DESC, s.id DESC OFFSET "#)
        .push_bind((q.page - 1).saturating_mul(q.limit))
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<SubmissionRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let mut cq =
        QueryBuilder::<Postgres>::new(r#"SELECT COUNT(*) FROM "storeVisitFormSubmission" s"#);
    push_where(&mut cq, company_id, &filters, q, range);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;

    let lookups = lookup_people_and_stores(state, &rows).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| Value::Object(to_submission(r, &lookups)))
        .collect();
    let total_pages = (total + q.limit - 1) / q.limit;
    Ok(json!({
        "data": data,
        "meta": { "total": total, "page": q.page, "limit": q.limit, "totalPages": total_pages },
    }))
}

async fn find_row(
    conn: &mut PgConnection,
    uuid: &str,
    company_id: i32,
) -> sqlx::Result<Option<SubmissionRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {LIST_COLUMNS}, s.answers FROM "storeVisitFormSubmission" s
            WHERE s.uuid = $1::uuid AND s."companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

fn submission_not_found() -> ApiError {
    not_found("FORM_SUBMISSION_NOT_FOUND", "Form submission not found")
}

/// `storeVisitFormTemplateVersion.findUnique({ where: { id }, select: { schema } })`.
async fn version_schema(conn: &mut PgConnection, id: i32) -> sqlx::Result<Option<Value>> {
    sqlx::query_scalar(r#"SELECT schema FROM "storeVisitFormTemplateVersion" WHERE id = $1"#)
        .bind(id)
        .fetch_optional(conn)
        .await
}

/// `toSubmissionDetail(row, lookups, version, canRevise, companyId)`.
async fn to_submission_detail(
    state: &AppState,
    row: &SubmissionRow,
    lookups: &Lookups,
    schema_value: Option<&Value>,
    can_revise: bool,
    company_id: i32,
) -> ApiResult<Value> {
    let mut out = to_submission(row, lookups);
    let schema = schema_value.map(FormDocument::from_value);
    let answers_display = match &schema {
        Some(s) => enrich_display_values(&state.db, s, Some(&row.answers), company_id).await?,
        None => None,
    };
    out.insert("canRevise".into(), Value::Bool(can_revise));
    out.insert(
        "schema".into(),
        schema_value.cloned().unwrap_or(Value::Null),
    );
    let answers = match &schema {
        Some(s) => {
            enrich_media_values(state, s, &row.answers, row.id, &row.uuid, Audience::Admin).await?
        }
        None => row.answers.clone(),
    };
    out.insert("answers".into(), answers);
    if let Some(display) = answers_display {
        out.insert("answersDisplay".into(), Value::Object(display));
    }
    Ok(Value::Object(out))
}

/// `findSubmission(userId, uuid)`.
async fn find_submission(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = company_id(state, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let row = find_row(&mut conn, uuid, company_id)
        .await?
        .ok_or_else(submission_not_found)?;
    let schema = version_schema(&mut conn, row.template_version_id).await?;
    let lookups = lookup_people_and_stores(state, std::slice::from_ref(&row)).await?;
    // §6.2 — petunjuk UI, bukan gerbang.
    let decision = can_revise(&mut conn, user_id, company_id).await?;
    drop(conn);
    to_submission_detail(
        state,
        &row,
        &lookups,
        schema.as_ref(),
        decision.allowed,
        company_id,
    )
    .await
}

/// Urutan unik `fileUuid` (`new Set(...)`).
fn ordered_set(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|x| seen.insert(x.clone()))
        .collect()
}

/// `fileUuidSet(schema, answers)`.
pub fn file_uuid_set(schema: &FormDocument, answers: &Value) -> Vec<String> {
    let r = validate_answers(
        schema,
        Some(answers),
        &ValidateAnswersOptions {
            mode: AnswerValidationMode::Draft,
            time_zone: "UTC",
        },
    );
    ordered_set(r.files.into_iter().map(|f| f.file_uuid))
}

/// §0.28 — `FORM_MEDIA_IMMUTABLE` bila himpunan file berubah.
pub fn media_immutable_error(before: &[String], after: &[String]) -> Option<ApiError> {
    let b: HashSet<&String> = before.iter().collect();
    let same = before.len() == after.len() && after.iter().all(|u| b.contains(u));
    (!same).then(|| {
        coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FORM_MEDIA_IMMUTABLE",
            "Signature, photo, and file fields cannot be changed by a correction",
            json!({ "expected": before, "received": after }),
        )
    })
}

#[derive(sqlx::FromRow)]
struct BeforeRow {
    id: i32,
    answers: Value,
    revision_count: i32,
    submitted_at: Option<NaiveDateTime>,
    template_version_id: i32,
}

/// Langkah validasi koreksi (§7.3) atas schema versi yang di-pin: jawaban final tervalidasi,
/// batas ukuran, dan himpunan media tak berubah.
pub fn validate_correction(
    schema: &FormDocument,
    before_answers: &Value,
    dto_answers: &Value,
    time_zone: &str,
) -> ApiResult<Map<String, Value>> {
    let validated = validate_answers(
        schema,
        Some(dto_answers),
        &ValidateAnswersOptions {
            mode: AnswerValidationMode::Final,
            time_zone,
        },
    );
    if !validated.issues.is_empty() {
        return Err(coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FORM_VALIDATION_FAILED",
            "Answers did not pass validation",
            json!({ "errors": validated.issues }),
        ));
    }
    let answers = Value::Object(validated.answers);
    let bytes = answers_byte_size(Some(&answers));
    if bytes > MAX_ANSWERS_BYTES {
        return Err(coded_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FORM_ANSWERS_TOO_LARGE",
            format!("Answers exceed the {MAX_ANSWERS_BYTES} byte limit"),
            json!({ "maxSizeBytes": MAX_ANSWERS_BYTES, "sizeBytes": bytes }),
        ));
    }
    let before = file_uuid_set(schema, before_answers);
    let after = ordered_set(validated.files.into_iter().map(|f| f.file_uuid));
    if let Some(e) = media_immutable_error(&before, &after) {
        return Err(e);
    }
    Ok(match answers {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

/// `correctSubmission(userId, uuid, dto)` (§6.8).
async fn correct_submission(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    dto: &CorrectDto,
) -> ApiResult<Value> {
    let company_id = company_id(state, user_id).await?;
    let tz = time_zone(state, company_id).await?;

    let mut tx = state.db.begin().await?;
    let before: BeforeRow = sqlx::query_as(
        r#"SELECT id, answers, "revisionCount" AS revision_count, "submittedAt" AS submitted_at,
                  "templateVersionId" AS template_version_id
             FROM "storeVisitFormSubmission" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(submission_not_found)?;

    // §6.1 — sesudah pembacaan (404 dulu), sebelum segala pemeriksaan lain, di `tx`.
    assert_can_revise(&mut tx, user_id, company_id).await?;

    if before.submitted_at.is_none() {
        return Err(coded(
            StatusCode::CONFLICT,
            "FORM_NOT_SUBMITTED",
            "Only a submitted form can be corrected",
        ));
    }
    let schema_value = version_schema(&mut tx, before.template_version_id)
        .await?
        .ok_or_else(|| {
            coded(
                StatusCode::CONFLICT,
                "FORM_TEMPLATE_NOT_PUBLISHED",
                "The schema this submission was filled against is missing",
            )
        })?;
    let schema = FormDocument::from_value(&schema_value);
    let answers = validate_correction(&schema, &before.answers, &dto.answers, &tz)?;

    let now = mz_core::time::now_naive();
    let updated = sqlx::query(
        r#"UPDATE "storeVisitFormSubmission"
              SET answers = $1, "revisionCount" = "revisionCount" + 1, "lastEditedAt" = $2,
                  "lastEditedById" = $3, "updatedAt" = $2
            WHERE id = $4 AND "revisionCount" = $5"#,
    )
    .bind(Value::Object(answers))
    .bind(now)
    .bind(user_id)
    .bind(before.id)
    .bind(dto.expected_revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        let current: Option<i32> = sqlx::query_scalar(
            r#"SELECT "revisionCount" FROM "storeVisitFormSubmission" WHERE id = $1"#,
        )
        .bind(before.id)
        .fetch_optional(&mut *tx)
        .await?;
        return Err(coded_details(
            StatusCode::CONFLICT,
            "FORM_REVISION_CONFLICT",
            "This submission was corrected by someone else; reload before correcting again",
            json!({
                "expectedRevision": dto.expected_revision,
                "currentRevision": current.unwrap_or(before.revision_count),
            }),
        ));
    }
    sqlx::query(
        r#"INSERT INTO "storeVisitFormSubmissionRevision"
              ("submissionId", revision, "answersBefore", reason, "editedById")
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(before.id)
    .bind(dto.expected_revision + 1)
    .bind(&before.answers)
    .bind(&dto.reason)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let row = find_row(&mut tx, uuid, company_id)
        .await?
        .ok_or_else(submission_not_found)?;
    tx.commit().await?;

    let lookups = lookup_people_and_stores(state, std::slice::from_ref(&row)).await?;
    to_submission_detail(state, &row, &lookups, Some(&schema_value), true, company_id).await
}

/// `findSubmissionRevisions(userId, uuid)`.
async fn find_revisions(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = company_id(state, user_id).await?;
    let sub: (i32, i32) = sqlx::query_as(
        r#"SELECT id, "revisionCount" FROM "storeVisitFormSubmission"
            WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(submission_not_found)?;
    type RevRow = (String, i32, Value, String, i32, NaiveDateTime);
    let revisions: Vec<RevRow> = sqlx::query_as(
        r#"SELECT uuid::text, revision, "answersBefore", reason, "editedById", "createdAt"
             FROM "storeVisitFormSubmissionRevision" WHERE "submissionId" = $1
            ORDER BY revision DESC"#,
    )
    .bind(sub.0)
    .fetch_all(&state.db)
    .await?;
    let editor_ids = unique(revisions.iter().map(|r| r.4));
    let editors = if editor_ids.is_empty() {
        HashMap::new()
    } else {
        lookup_users(state, &editor_ids).await?
    };
    let data: Vec<Value> = revisions
        .into_iter()
        .map(
            |(uuid, revision, answers_before, reason, edited_by, created_at)| {
                let editor = editors.get(&edited_by);
                json!({
                    "id": uuid,
                    "revision": revision,
                    "reason": reason,
                    "answersBefore": answers_before,
                    "editedBy": {
                        "id": editor.map(|e| e.uuid.clone()),
                        "name": editor.map(|e| e.name.clone()),
                    },
                    "createdAt": ms(created_at),
                })
            },
        )
        .collect();
    Ok(json!({ "data": data, "revisionCount": sub.1 }))
}

/// `submissionPdfFilename(templateName, visitNumber, submissionUuid)` — hasil `[a-z0-9-]`.
pub fn submission_pdf_filename(
    template_name: &str,
    visit_number: Option<&str>,
    submission_uuid: &str,
) -> String {
    fn slug(value: &str) -> String {
        let lower = value.to_lowercase();
        let mut out = String::new();
        let mut in_run = false;
        for c in lower.chars() {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
                in_run = false;
            } else if !in_run {
                out.push('-');
                in_run = true;
            }
        }
        out.trim_matches('-').to_owned()
    }
    let name: String = slug(template_name).chars().take(60).collect();
    let name = name.trim_end_matches('-');
    let name = if name.is_empty() { "form" } else { name };
    let suffix = slug(visit_number.unwrap_or(""));
    let suffix = if suffix.is_empty() {
        submission_uuid.chars().take(8).collect()
    } else {
        suffix
    };
    format!("{name}-{suffix}.pdf")
}

/// `generateSubmissionPdf(userId, uuid)` → (byte, nama berkas).
async fn generate_pdf(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<(Vec<u8>, String)> {
    let company_id = company_id(state, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let row = find_row(&mut conn, uuid, company_id)
        .await?
        .ok_or_else(submission_not_found)?;
    let schema_value = version_schema(&mut conn, row.template_version_id)
        .await?
        .filter(|s| !s.is_null())
        .ok_or_else(|| {
            coded(
                StatusCode::CONFLICT,
                "FORM_SCHEMA_UNAVAILABLE",
                "This form's schema is no longer available",
            )
        })?;
    drop(conn);
    let schema = FormDocument::from_value(&schema_value);

    let lookups = lookup_people_and_stores(state, std::slice::from_ref(&row)).await?;
    let company: Option<(String, String)> =
        sqlx::query_as(r#"SELECT name, "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let visit = visit_lookup::visit_for_pdf(state, row.visit_id, company_id).await?;
    let store_code = visit_lookup::store_code(state, row.store_id, company_id).await?;
    let requester: Option<String> = sqlx::query_scalar(r#"SELECT name FROM users WHERE id = $1"#)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;

    // Dipangkas SEBELUM rujukan media dipungut.
    let raw = row.answers.as_object().cloned().unwrap_or_default();
    let pruned = prune_hidden_values(&schema, &raw);
    let loaded = load_media_for_pdf(state, &schema, &Value::Object(pruned), row.id).await?;
    let layout_media = load_layout_media_for_pdf(state, &schema_value, row.template_id).await?;

    let template_name = lookups
        .templates
        .get(&row.template_id)
        .map_or_else(|| "Form".to_owned(), |t| t.name.clone());
    let editor = row.last_edited_by_id.and_then(|id| lookups.users.get(&id));
    let (company_name, company_tz) = company.unwrap_or_default();
    let utc = |t: NaiveDateTime| t.and_utc();

    let data = SubmissionPdfData {
        company_name,
        template_name: template_name.clone(),
        template_version: lookups
            .versions
            .get(&row.template_version_id)
            .map(|v| i64::from(v.version)),
        status: if row.submitted_at.is_some() {
            PdfStatus::Submitted
        } else {
            PdfStatus::Draft
        },
        submitted_at: row.submitted_at.map(utc),
        created_at: utc(row.created_at),
        sales_name: lookups
            .users
            .get(&row.user_id)
            .map_or_else(|| EMPTY_FIELD_MARK.to_owned(), |u| u.name.clone()),
        store_name: row.store_name.clone(),
        store_code,
        visit_number: visit
            .as_ref()
            .map(|v| v.visit_number.clone())
            .unwrap_or_default(),
        check_in_at: utc(visit
            .as_ref()
            .map_or(row.visit_check_in_at, |v| v.check_in_at)),
        check_out_at: visit.as_ref().and_then(|v| v.check_out_at).map(utc),
        revision_count: i64::from(row.revision_count),
        last_edited_at: row.last_edited_at.map(utc),
        last_edited_by_name: editor.map(|e| e.name.clone()),
        schema,
        answers: loaded.answers.as_object().cloned().unwrap_or_default(),
        media: loaded.media,
        layout_media: Some(layout_media),
        time_zone: if company_tz.is_empty() {
            "UTC".to_owned()
        } else {
            company_tz
        },
        generated_at: DateTime::<Utc>::from_timestamp_millis(Utc::now().timestamp_millis())
            .unwrap_or_else(Utc::now),
        generated_by: requester.unwrap_or_else(|| EMPTY_FIELD_MARK.to_owned()),
    };
    let buffer = tokio::task::spawn_blocking(move || generate_submission_pdf(&data))
        .await
        .map_err(ApiError::internal)?;
    let filename = submission_pdf_filename(
        &template_name,
        visit.as_ref().map(|v| v.visit_number.as_str()),
        uuid,
    );
    Ok((buffer, filename))
}

// ── Handler ──────────────────────────────────────────────────────────────────

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1239 `findSubmissions`.
async fn h_find_submissions(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let raw: Map<String, Value> =
        mz_core::extract::query_from_str(uri.query().unwrap_or_default())?;
    let q = parse_list_query(&raw).map_err(ApiError::Validation)?;
    Ok(Json(find_submissions(&state, user_id, &q).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1268 `findSubmission`.
async fn h_find_submission(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let data = find_submission(&state, user_id, &uuid).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1328 `correctSubmission`.
async fn h_correct_submission(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    body: Result<ValidatedJson<CorrectBody>, ApiError>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    // ParseUUIDPipe sebelum ValidationPipe body.
    let uuid = uuid_pipe(uuid)?;
    let ValidatedJson(body) = body?;
    let dto = body.check().map_err(ApiError::Validation)?;
    let data = correct_submission(&state, user_id, &uuid, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1453 `findSubmissionRevisions`.
async fn h_find_revisions(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(Json(find_revisions(&state, user_id, &uuid).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1498 `downloadSubmissionPdf`.
async fn h_download_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let (buffer, filename) = generate_pdf(&state, user_id, &uuid).await?;
    let len = buffer.len();
    let mut res = Response::new(Body::from(buffer));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(res.into_response())
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1559 `submissionFile`.
async fn h_submission_file(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, file_uuid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let file_uuid = uuid_pipe(file_uuid)?;
    let company_id = company_id(&state, user_id).await?;
    stream_company_file(&state, company_id, &uuid, &file_uuid, &headers).await
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1602 `submissionFileUrl`.
async fn h_submission_file_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, file_uuid)): Path<(String, String)>,
) -> ApiResult<Json<mz_files::SignedReadUrl>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let file_uuid = uuid_pipe(file_uuid)?;
    let company_id = company_id(&state, user_id).await?;
    Ok(Json(
        company_file_url(&state, company_id, &uuid, &file_uuid).await?,
    ))
}

#[cfg(test)]
mod tests;
