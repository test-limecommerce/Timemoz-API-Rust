//! Port `src/visit-masters/*`: `/v1/admin/visit-masters/:kind` (5 route; `kind` = `types` |
//! `frequencies`) + helper `pub` untuk modul rencana/kunjungan (`resolveVisitTypes`,
//! `findDefault`, `toVisitTypeRef`). Model `visitType`/`visitFrequency` TIDAK ber-audit di Node.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery, check_int_range};
use mz_core::jsfmt::trim as js_trim;
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::dto::{
    Field, Raw, check_bool, check_length_string, check_max_string, gate, page_limit,
    parse_int_value, present, to_boolean,
};
use crate::geofences::internal;

// ───────────────────────── kind ─────────────────────────

/// `VisitMasterKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitMasterKind {
    Types,
    Frequencies,
}

impl VisitMasterKind {
    /// `ParseEnumPipe(VisitMasterKind)`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "types" => Some(Self::Types),
            "frequencies" => Some(Self::Frequencies),
            _ => None,
        }
    }

    const fn table(self) -> &'static str {
        match self {
            Self::Types => r#""visitType""#,
            Self::Frequencies => r#""visitFrequency""#,
        }
    }

    /// `LABEL_BY_KIND`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Types => "Visit type",
            Self::Frequencies => "Visit frequency",
        }
    }

    /// `hasIsDefault`.
    const fn has_is_default(self) -> bool {
        matches!(self, Self::Types)
    }

    /// `hasRequiresFlags`.
    const fn has_requires_flags(self) -> bool {
        matches!(self, Self::Types)
    }

    /// Kolom `SELECT` (superset; kolom milik kind lain = NULL).
    const fn columns(self) -> &'static str {
        match self {
            Self::Types => {
                r#"id, uuid::text AS uuid, "companyId", code, name, description, "sortOrder", "isActive",
                   "createdAt", "updatedAt", "requiresOrder", "requiresPhoto", "requiresGps", "isDefault",
                   NULL::int AS "intervalDays""#
            }
            Self::Frequencies => {
                r#"id, uuid::text AS uuid, "companyId", code, name, description, "sortOrder", "isActive",
                   "createdAt", "updatedAt", NULL::bool AS "requiresOrder", NULL::bool AS "requiresPhoto",
                   NULL::bool AS "requiresGps", NULL::bool AS "isDefault", "intervalDays""#
            }
        }
    }

    /// `extraFields`.
    fn accepts(self, field: ExtraField) -> bool {
        match self {
            Self::Types => !matches!(field, ExtraField::IntervalDays),
            Self::Frequencies => matches!(field, ExtraField::IntervalDays),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExtraField {
    RequiresOrder,
    RequiresPhoto,
    RequiresGps,
    IsDefault,
    IntervalDays,
}

impl ExtraField {
    /// `ALL_EXTRA_FIELD_NAMES` (urutan config).
    const ALL: [ExtraField; 5] = [
        ExtraField::RequiresOrder,
        ExtraField::RequiresPhoto,
        ExtraField::RequiresGps,
        ExtraField::IsDefault,
        ExtraField::IntervalDays,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::RequiresOrder => "requiresOrder",
            Self::RequiresPhoto => "requiresPhoto",
            Self::RequiresGps => "requiresGps",
            Self::IsDefault => "isDefault",
            Self::IntervalDays => "intervalDays",
        }
    }
}

/// `@Param('kind', new ParseEnumPipe(VisitMasterKind))`: 400 `Validation failed (enum string is expected)`.
pub struct KindPath(pub VisitMasterKind);

impl FromRequestParts<AppState> for KindPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path(raw): Path<String> = Path::from_request_parts(parts, state)
            .await
            .map_err(|e| internal(&e.to_string()))?;
        Ok(Self(kind_pipe(&raw)?))
    }
}

/// `:kind` + `:id` (`ParseUUIDPipe`: 400 `Validation failed (uuid is expected)`), urutan pipe Nest.
pub struct KindIdPath(pub VisitMasterKind, pub uuid::Uuid);

impl FromRequestParts<AppState> for KindIdPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path((kind, id)): Path<(String, String)> = Path::from_request_parts(parts, state)
            .await
            .map_err(|e| internal(&e.to_string()))?;
        let kind = kind_pipe(&kind)?;
        Ok(Self(kind, parse_uuid_pipe(&id)?))
    }
}

fn kind_pipe(raw: &str) -> ApiResult<VisitMasterKind> {
    VisitMasterKind::parse(raw)
        .ok_or_else(|| ApiError::bad_request("Validation failed (enum string is expected)"))
}

// ───────────────────────── baris & entitas ─────────────────────────

/// `VisitMasterRow`: superset kolom kedua tabel (kolom milik kind lain = `None`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VisitMasterRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: i32,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    /// `visitType` saja.
    #[sqlx(rename = "requiresOrder")]
    pub requires_order: Option<bool>,
    #[sqlx(rename = "requiresPhoto")]
    pub requires_photo: Option<bool>,
    #[sqlx(rename = "requiresGps")]
    pub requires_gps: Option<bool>,
    #[sqlx(rename = "isDefault")]
    pub is_default: Option<bool>,
    /// `visitFrequency` saja.
    #[sqlx(rename = "intervalDays")]
    pub interval_days: Option<i32>,
}

/// `VisitMastersService.toEntity` (waktu = epoch ms).
pub fn to_entity(row: &VisitMasterRow, kind: VisitMasterKind) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(row.uuid));
    m.insert("code".into(), json!(row.code));
    m.insert("name".into(), json!(row.name));
    m.insert("description".into(), json!(row.description));
    m.insert("sortOrder".into(), json!(row.sort_order));
    m.insert("isActive".into(), json!(row.is_active));
    if kind.has_requires_flags() {
        m.insert(
            "requiresOrder".into(),
            json!(row.requires_order.unwrap_or(false)),
        );
        m.insert(
            "requiresPhoto".into(),
            json!(row.requires_photo.unwrap_or(false)),
        );
        m.insert(
            "requiresGps".into(),
            json!(row.requires_gps.unwrap_or(true)),
        );
    }
    if kind.has_is_default() {
        m.insert("isDefault".into(), json!(row.is_default.unwrap_or(false)));
    }
    if kind == VisitMasterKind::Frequencies {
        m.insert("intervalDays".into(), json!(row.interval_days));
    }
    m.insert("createdAt".into(), json!(row.created_at.timestamp_millis()));
    m.insert(
        "updatedAt".into(),
        json!(row.updated_at.map(|d| d.timestamp_millis())),
    );
    Value::Object(m)
}

/// `toVisitTypeRef` (`visit-type-ref.ts`): rujukan visit type di respons rencana/kunjungan.
pub fn to_visit_type_ref(row: &VisitMasterRow) -> Value {
    json!({
        "id": row.uuid,
        "code": row.code,
        "name": row.name,
        "requiresOrder": row.requires_order.unwrap_or(false),
        "requiresPhoto": row.requires_photo.unwrap_or(false),
        "requiresGps": row.requires_gps.unwrap_or(true),
        "isActive": row.is_active,
    })
}

// ───────────────────────── DTO ─────────────────────────

#[derive(Debug, Default)]
struct Dto {
    code: Option<String>,
    name: Option<String>,
    description: Field<String>,
    sort_order: Field<i32>,
    is_active: Field<bool>,
    requires_order: Field<bool>,
    requires_photo: Field<bool>,
    requires_gps: Field<bool>,
    is_default: Field<bool>,
    interval_days: Field<i32>,
}

impl Dto {
    fn defined(&self, f: ExtraField) -> bool {
        match f {
            ExtraField::RequiresOrder => self.requires_order.is_defined(),
            ExtraField::RequiresPhoto => self.requires_photo.is_defined(),
            ExtraField::RequiresGps => self.requires_gps.is_defined(),
            ExtraField::IsDefault => self.is_default.is_defined(),
            ExtraField::IntervalDays => self.interval_days.is_defined(),
        }
    }
}

/// `VISIT_MASTER_CODE_PATTERN = /^[A-Z0-9_]{1,32}$/`.
fn code_ok(s: &str) -> bool {
    let n = s.chars().count();
    (1..=32).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Properti boolean ber-`@Transform(toBoolean) @IsOptional() @IsBoolean()`.
fn bool_field(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Field<bool> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => match check_bool(errs, key, Some(&to_boolean(v))) {
            Some(b) => Field::Val(b),
            None => Field::Absent,
        },
    }
}

/// Properti int ber-`@Transform(parseInt) @IsOptional() @IsInt() @Min @Max` (`null` → NaN → galat).
fn int_field(
    m: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
    errs: &mut Vec<String>,
) -> Field<i32> {
    let Some(v) = m.get(key) else {
        return Field::Absent;
    };
    #[allow(clippy::cast_precision_loss)]
    let n = parse_int_value(v).map(|n| n as f64);
    match check_int_range(errs, key, n, min, max).and_then(|i| i32::try_from(i).ok()) {
        Some(i) => Field::Val(i),
        None => Field::Absent,
    }
}

/// `CreateVisitMasterDto` (`required`) / `UpdateVisitMasterDto`.
fn parse(m: &Map<String, Value>, required: bool) -> Result<Dto, Vec<String>> {
    let mut errs = Vec::new();
    let mut d = Dto::default();

    // code: Transform(upper) + [Matches, IsString]
    if let Some(v) = gate(m, "code", required) {
        let upper = match v {
            Some(Value::String(s)) => Some(s.to_uppercase()),
            _ => None,
        };
        match upper {
            Some(s) if code_ok(&s) => d.code = Some(s),
            other => {
                errs.push(
                    "code must be 1-32 characters of uppercase letters, digits, or underscore"
                        .into(),
                );
                if other.is_none() {
                    errs.push("code must be a string".into());
                }
            }
        }
    }
    // name: [Length(1, 120), IsString]
    if let Some(v) = gate(m, "name", required) {
        d.name = check_length_string(&mut errs, "name", v, 1, 120);
    }
    // description: Transform(emptyToNull) + IsOptional + [MaxLength(255), IsString]
    match m.get("description") {
        None => {}
        Some(Value::Null) => d.description = Field::Null,
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                d.description = Field::Null;
            } else if let Some(v) = check_max_string(&mut errs, "description", Some(&json!(t)), 255)
            {
                d.description = Field::Val(v);
            }
        }
        Some(other) => {
            check_max_string(&mut errs, "description", Some(other), 255);
        }
    }
    d.sort_order = int_field(m, "sortOrder", 0, 9999, &mut errs);
    d.is_active = bool_field(m, "isActive", &mut errs);
    d.requires_order = bool_field(m, "requiresOrder", &mut errs);
    d.requires_photo = bool_field(m, "requiresPhoto", &mut errs);
    d.requires_gps = bool_field(m, "requiresGps", &mut errs);
    d.is_default = bool_field(m, "isDefault", &mut errs);
    d.interval_days = int_field(m, "intervalDays", 1, 366, &mut errs);
    if errs.is_empty() { Ok(d) } else { Err(errs) }
}

/// `ListVisitMastersQueryDto`.
#[derive(Debug)]
struct ListQuery {
    search: Option<String>,
    is_active: Option<bool>,
    is_default: Option<bool>,
    page: i64,
    limit: i64,
}

fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = ListQuery {
        search: None,
        is_active: None,
        is_default: None,
        page: 1,
        limit: 20,
    };
    if let Some(v) = present(m, "search") {
        q.search = check_max_string(&mut errs, "search", Some(v), 120);
    }
    for (key, slot) in [
        ("isActive", &mut q.is_active),
        ("isDefault", &mut q.is_default),
    ] {
        if let Some(v) = present(m, key) {
            *slot = check_bool(&mut errs, key, Some(&to_boolean(v)));
        }
    }
    (q.page, q.limit) = page_limit(m, &mut errs);
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

// ───────────────────────── service ─────────────────────────

fn coded(status: StatusCode, code: &str, message: String) -> ApiError {
    ApiError::body(status, json!({ "code": code, "message": message }))
}

fn invalid_field(field: &str, kind: VisitMasterKind) -> ApiError {
    coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "VISIT_MASTER_INVALID_FIELD",
        format!("{field} is not accepted for {}", kind.label()),
    )
}

fn default_archived() -> ApiError {
    coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "VISIT_TYPE_DEFAULT_ARCHIVED",
        "isDefault cannot be true on an archived (isActive: false) row".into(),
    )
}

/// `rethrowP2002`: constraint unik `(companyId, code)` → 409 kode terpakai; selain itu (indeks parsial
/// `visitType_one_default_per_company`) → 409 default direbut.
fn map_write_error(e: sqlx::Error, kind: VisitMasterKind, code: &str) -> ApiError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        let on_code = db.constraint().is_some_and(|c| c.ends_with("_code_key"));
        return if on_code {
            coded(
                StatusCode::CONFLICT,
                "VISIT_MASTER_CODE_TAKEN",
                format!(
                    "{} code {code} is already used in this company",
                    kind.label()
                ),
            )
        } else {
            coded(
                StatusCode::CONFLICT,
                "VISIT_TYPE_DEFAULT_CONFLICT",
                "Another visit type row won the default slot for this company — retry".into(),
            )
        };
    }
    e.into()
}

async fn find_row_or_throw(
    state: &AppState,
    kind: VisitMasterKind,
    company_id: i32,
    uuid: uuid::Uuid,
) -> ApiResult<VisitMasterRow> {
    let row: Option<VisitMasterRow> = sqlx::query_as(&format!(
        r#"SELECT {} FROM {} WHERE uuid = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#,
        kind.columns(),
        kind.table()
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| {
        coded(
            StatusCode::NOT_FOUND,
            "VISIT_MASTER_NOT_FOUND",
            format!("{} not found", kind.label()),
        )
    })
}

/// `assertFieldsAllowed`.
fn assert_fields_allowed(kind: VisitMasterKind, dto: &Dto) -> ApiResult<()> {
    for f in ExtraField::ALL {
        if dto.defined(f) && !kind.accepts(f) {
            return Err(invalid_field(f.name(), kind));
        }
    }
    Ok(())
}

/// `assertNameLength` (`name.length` = unit UTF-16).
fn assert_name_length(kind: VisitMasterKind, name: &str) -> ApiResult<()> {
    if name.encode_utf16().count() > 120 {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VISIT_MASTER_NAME_TOO_LONG",
            format!("name must be at most 120 characters for {}", kind.label()),
        ));
    }
    Ok(())
}

/// `null` pada kolom non-null: Prisma melempar → 500. `Absent` → `None`.
fn defined<T: Clone>(f: &Field<T>) -> ApiResult<Option<T>> {
    match f {
        Field::Absent => Ok(None),
        Field::Null => Err(internal("Argument must not be null")),
        Field::Val(v) => Ok(Some(v.clone())),
    }
}

/// Kolom ekstra yang ditulis (`buildExtraData`), sudah dicek `null`.
#[derive(Default)]
struct Extra {
    requires_order: Option<bool>,
    requires_photo: Option<bool>,
    requires_gps: Option<bool>,
    is_default: Option<bool>,
    interval_days: Option<i32>,
}

fn build_extra(kind: VisitMasterKind, dto: &Dto) -> ApiResult<Extra> {
    let mut e = Extra::default();
    if kind.has_requires_flags() {
        e.requires_order = defined(&dto.requires_order)?;
        e.requires_photo = defined(&dto.requires_photo)?;
        e.requires_gps = defined(&dto.requires_gps)?;
    }
    if kind.has_is_default() {
        e.is_default = defined(&dto.is_default)?;
    }
    if kind == VisitMasterKind::Frequencies {
        e.interval_days = defined(&dto.interval_days)?;
    }
    Ok(e)
}

/// §0.8: menyalakan satu default mematikan yang lain, dalam satu transaksi.
async fn clear_defaults(conn: &mut PgConnection, company_id: i32) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "visitType" SET "isDefault" = false WHERE "companyId" = $1 AND "isDefault" = true"#,
    )
    .bind(company_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn create(
    state: &AppState,
    user_id: i32,
    kind: VisitMasterKind,
    dto: &Dto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_fields_allowed(kind, dto)?;
    if kind == VisitMasterKind::Frequencies && !dto.interval_days.is_defined() {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VISIT_MASTER_FIELD_REQUIRED",
            format!("intervalDays is required to create {}", kind.label()),
        ));
    }
    let (Some(code), Some(name)) = (&dto.code, &dto.name) else {
        return Err(internal("visit master DTO incomplete"));
    };
    assert_name_length(kind, name)?;

    let resolved_active = match dto.is_active {
        Field::Val(b) => b,
        _ => true,
    };
    let want_default = matches!(dto.is_default, Field::Val(true));
    if kind.has_is_default() && want_default && !resolved_active {
        return Err(default_archived());
    }
    let extra = build_extra(kind, dto)?;
    let sort_order = defined(&dto.sort_order)?;
    let is_active = defined(&dto.is_active)?;

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"INSERT INTO {} ("companyId", code, name, "createdById""#,
        kind.table()
    ));
    if dto.description.is_defined() {
        qb.push(", description");
    }
    if sort_order.is_some() {
        qb.push(r#", "sortOrder""#);
    }
    if is_active.is_some() {
        qb.push(r#", "isActive""#);
    }
    if extra.requires_order.is_some() {
        qb.push(r#", "requiresOrder""#);
    }
    if extra.requires_photo.is_some() {
        qb.push(r#", "requiresPhoto""#);
    }
    if extra.requires_gps.is_some() {
        qb.push(r#", "requiresGps""#);
    }
    if extra.is_default.is_some() {
        qb.push(r#", "isDefault""#);
    }
    if extra.interval_days.is_some() {
        qb.push(r#", "intervalDays""#);
    }
    qb.push(") VALUES (")
        .push_bind(company_id)
        .push(", ")
        .push_bind(code.clone())
        .push(", ")
        .push_bind(name.clone())
        .push(", ")
        .push_bind(user_id);
    match &dto.description {
        Field::Val(s) => {
            qb.push(", ").push_bind(s.clone());
        }
        Field::Null => {
            qb.push(", NULL");
        }
        Field::Absent => {}
    }
    macro_rules! push_opt {
        ($v:expr) => {
            if let Some(v) = $v {
                qb.push(", ").push_bind(v);
            }
        };
    }
    push_opt!(sort_order);
    push_opt!(is_active);
    push_opt!(extra.requires_order);
    push_opt!(extra.requires_photo);
    push_opt!(extra.requires_gps);
    push_opt!(extra.is_default);
    push_opt!(extra.interval_days);
    qb.push(format!(") RETURNING {}", kind.columns()));

    let row: VisitMasterRow = if kind.has_is_default() && want_default {
        let mut tx = state.db.begin().await?;
        clear_defaults(&mut tx, company_id)
            .await
            .map_err(|e| map_write_error(e, kind, code))?;
        let row = qb
            .build_query_as::<VisitMasterRow>()
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_write_error(e, kind, code))?;
        tx.commit()
            .await
            .map_err(|e| map_write_error(e, kind, code))?;
        row
    } else {
        qb.build_query_as::<VisitMasterRow>()
            .fetch_one(&state.db)
            .await
            .map_err(|e| map_write_error(e, kind, code))?
    };
    Ok(to_entity(&row, kind))
}

async fn find_all(
    state: &AppState,
    user_id: i32,
    kind: VisitMasterKind,
    q: &ListQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    if q.is_default.is_some() && !kind.has_is_default() {
        return Err(invalid_field("isDefault", kind));
    }
    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(format!(r#" FROM {} WHERE "companyId" = "#, kind.table()))
            .push_bind(company_id);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(d) = q.is_default {
            qb.push(r#" AND "isDefault" = "#).push_bind(d);
        }
        // Prisma `contains` + `mode: insensitive`: ILIKE tanpa escape `%`/`_` (sama dengan Node).
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND (code ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR name ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%')");
        }
    };
    let skip = (q.page - 1).saturating_mul(q.limit);
    let mut list = QueryBuilder::<Postgres>::new(format!("SELECT {}", kind.columns()));
    push_where(&mut list);
    list.push(r#" ORDER BY "sortOrder" ASC, name ASC, id ASC OFFSET "#)
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<VisitMasterRow> = list.build_query_as().fetch_all(&state.db).await?;
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": rows.iter().map(|r| to_entity(r, kind)).collect::<Vec<_>>(),
        "meta": {
            "total": total,
            "page": q.page,
            "limit": q.limit,
            "totalPages": (total + q.limit - 1) / q.limit,
        },
    }))
}

async fn update(
    state: &AppState,
    user_id: i32,
    kind: VisitMasterKind,
    uuid: uuid::Uuid,
    dto: &Dto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, uuid).await?;
    assert_fields_allowed(kind, dto)?;
    if let Some(n) = &dto.name {
        assert_name_length(kind, n)?;
    }
    let resolved_active = match dto.is_active {
        Field::Val(b) => b,
        _ => row.is_active,
    };
    let want_default = matches!(dto.is_default, Field::Val(true));
    if kind.has_is_default() && want_default && !resolved_active {
        return Err(default_archived());
    }
    // §0.9: arsip membersihkan `isDefault` di UPDATE yang sama (tanpa melihat `row.isDefault`).
    let auto_clear = kind.has_is_default()
        && matches!(dto.is_active, Field::Val(false))
        && !dto.is_default.is_defined();

    let extra = build_extra(kind, dto)?;
    let sort_order = defined(&dto.sort_order)?;
    let is_active = defined(&dto.is_active)?;

    let mut qb = QueryBuilder::<Postgres>::new(format!("UPDATE {} SET ", kind.table()));
    if let Some(c) = &dto.code {
        qb.push("code = ").push_bind(c.clone()).push(", ");
    }
    if let Some(n) = &dto.name {
        qb.push("name = ").push_bind(n.clone()).push(", ");
    }
    match &dto.description {
        Field::Val(s) => {
            qb.push("description = ").push_bind(s.clone()).push(", ");
        }
        Field::Null => {
            qb.push("description = NULL, ");
        }
        Field::Absent => {}
    }
    macro_rules! set_opt {
        ($col:expr, $v:expr) => {
            if let Some(v) = $v {
                qb.push($col).push_bind(v).push(", ");
            }
        };
    }
    set_opt!(r#""sortOrder" = "#, sort_order);
    set_opt!(r#""isActive" = "#, is_active);
    set_opt!(r#""requiresOrder" = "#, extra.requires_order);
    set_opt!(r#""requiresPhoto" = "#, extra.requires_photo);
    set_opt!(r#""requiresGps" = "#, extra.requires_gps);
    set_opt!(r#""intervalDays" = "#, extra.interval_days);
    if auto_clear {
        qb.push(r#""isDefault" = false, "#);
    } else {
        set_opt!(r#""isDefault" = "#, extra.is_default);
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(row.id);
    qb.push(format!(" RETURNING {}", kind.columns()));

    let code_for_error = dto.code.clone().unwrap_or_else(|| row.code.clone());
    let updated: VisitMasterRow = if kind.has_is_default() && want_default {
        let mut tx = state.db.begin().await?;
        // Kunci baris lalu baca ulang `isActive` (menutup TOCTOU dengan arsip bersamaan).
        let locked: Option<bool> =
            sqlx::query_scalar(r#"SELECT "isActive" FROM "visitType" WHERE id = $1 FOR UPDATE"#)
                .bind(row.id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(active) = locked
            && !is_active.unwrap_or(active)
        {
            return Err(default_archived());
        }
        clear_defaults(&mut tx, company_id)
            .await
            .map_err(|e| map_write_error(e, kind, &code_for_error))?;
        let r = qb
            .build_query_as::<VisitMasterRow>()
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_write_error(e, kind, &code_for_error))?;
        tx.commit()
            .await
            .map_err(|e| map_write_error(e, kind, &code_for_error))?;
        r
    } else {
        qb.build_query_as::<VisitMasterRow>()
            .fetch_one(&state.db)
            .await
            .map_err(|e| map_write_error(e, kind, &code_for_error))?
    };
    Ok(to_entity(&updated, kind))
}

async fn remove(
    state: &AppState,
    user_id: i32,
    kind: VisitMasterKind,
    uuid: uuid::Uuid,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, uuid).await?;
    let clear = if kind.has_is_default() {
        r#", "isDefault" = false"#
    } else {
        ""
    };
    sqlx::query(&format!(
        r#"UPDATE {} SET "isActive" = false{clear}, "updatedAt" = $2 WHERE id = $1"#,
        kind.table()
    ))
    .bind(row.id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── konsumen (rencana/kunjungan) ─────────────────────────

/// `resolveVisitTypes(client, companyId, uuids)` (§6.2): uuid → baris, dalam company yang sama.
/// Uuid asing → `404 VISIT_TYPE_NOT_FOUND`; ditemukan tapi nonaktif → `422 VISIT_TYPE_ARCHIVED`
/// (keduanya menyebut SELURUH uuid bermasalah di `details.ids`). `conn` boleh transaksi pemanggil.
pub async fn resolve_visit_types(
    conn: &mut PgConnection,
    company_id: i32,
    uuids: &[String],
) -> ApiResult<HashMap<String, VisitMasterRow>> {
    let mut ids: Vec<&String> = Vec::new();
    for u in uuids {
        if !ids.contains(&u) {
            ids.push(u);
        }
    }
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: Vec<uuid::Uuid> = ids
        .iter()
        .map(|u| crate::geofences::parse_row_uuid(u))
        .collect::<ApiResult<_>>()?;
    let rows: Vec<VisitMasterRow> = sqlx::query_as(&format!(
        r#"SELECT {} FROM "visitType" WHERE "companyId" = $1 AND uuid = ANY($2) ORDER BY id"#,
        VisitMasterKind::Types.columns()
    ))
    .bind(company_id)
    .bind(parsed)
    .fetch_all(&mut *conn)
    .await?;
    let by_uuid: HashMap<String, VisitMasterRow> =
        rows.into_iter().map(|r| (r.uuid.clone(), r)).collect();

    let missing: Vec<&String> = ids
        .iter()
        .copied()
        .filter(|u| !by_uuid.contains_key(*u))
        .collect();
    if !missing.is_empty() {
        return Err(ApiError::body(
            StatusCode::NOT_FOUND,
            json!({
                "code": "VISIT_TYPE_NOT_FOUND",
                "message": "Some visit type ids do not exist in this company",
                "details": { "ids": missing },
            }),
        ));
    }
    // Urutan `rows` Prisma (tanpa orderBy) = urutan DB; di sini urut `id`.
    let mut archived: Vec<&VisitMasterRow> = by_uuid.values().filter(|r| !r.is_active).collect();
    archived.sort_by_key(|r| r.id);
    if !archived.is_empty() {
        return Err(ApiError::body(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": "VISIT_TYPE_ARCHIVED",
                "message": "Some visit types are archived and cannot be assigned to a schedule",
                "details": { "ids": archived.iter().map(|r| r.uuid.clone()).collect::<Vec<_>>() },
            }),
        ));
    }
    Ok(by_uuid)
}

/// `findDefault(client, companyId)` (§0.12 langkah 3): visit type default company yang aktif.
pub async fn find_default(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<VisitMasterRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {} FROM "visitType" WHERE "companyId" = $1 AND "isDefault" = true AND "isActive" = true
           ORDER BY id LIMIT 1"#,
        VisitMasterKind::Types.columns()
    ))
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/visit-masters/admin-visit-masters.controller.ts:97 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindPath(kind): KindPath,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        find_all(&state, ctx.require_user_id()?, kind, &q).await?,
    ))
}

/// Node: src/visit-masters/admin-visit-masters.controller.ts:123 `create`
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindPath(kind): KindPath,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, ctx.require_user_id()?, kind, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/visit-masters/admin-visit-masters.controller.ts:154 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindIdPath(kind, id): KindIdPath,
) -> ApiResult<Json<Value>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let row = find_row_or_throw(&state, kind, company_id, id).await?;
    Ok(Json(json!({ "data": to_entity(&row, kind) })))
}

/// Node: src/visit-masters/admin-visit-masters.controller.ts:170 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindIdPath(kind, id): KindIdPath,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(&state, ctx.require_user_id()?, kind, id, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/visit-masters/admin-visit-masters.controller.ts:203 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindIdPath(kind, id): KindIdPath,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, ctx.require_user_id()?, kind, id).await?;
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn code_is_uppercased_and_checked() {
        let d = parse(&obj(json!({"code": "regular_visit", "name": "A"})), true).unwrap();
        assert_eq!(d.code.as_deref(), Some("REGULAR_VISIT"));
        let e = parse(&obj(json!({"code": "a-b", "name": "A"})), true).unwrap_err();
        assert_eq!(
            e,
            vec!["code must be 1-32 characters of uppercase letters, digits, or underscore"]
        );
        let e = parse(&obj(json!({"code": 5, "name": 5})), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                "code must be 1-32 characters of uppercase letters, digits, or underscore",
                "code must be a string",
                "name must be longer than or equal to 1 and shorter than or equal to 120 characters",
                "name must be a string",
            ]
        );
    }

    #[test]
    fn transforms_and_nulls() {
        let d = parse(
            &obj(
                json!({"description": "  ", "sortOrder": "7", "isActive": "false",
                        "requiresGps": null, "intervalDays": 5.9}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(d.description, Field::Null);
        assert_eq!(d.sort_order, Field::Val(7));
        assert_eq!(d.is_active, Field::Val(false));
        assert_eq!(d.requires_gps, Field::Null);
        assert_eq!(d.interval_days, Field::Val(5));
        // null → parseInt(null) = NaN → galat (IsOptional tidak melewati NaN).
        let e = parse(&obj(json!({"sortOrder": null})), false).unwrap_err();
        assert_eq!(
            e,
            vec![
                "sortOrder must not be greater than 9999",
                "sortOrder must not be less than 0",
                "sortOrder must be an integer number"
            ]
        );
        let d = parse(&obj(json!({"description": "  hi "})), false).unwrap();
        assert_eq!(d.description, Field::Val("hi".into()));
    }

    #[test]
    fn fields_allowed_per_kind() {
        let d = parse(&obj(json!({"intervalDays": 3})), false).unwrap();
        assert!(assert_fields_allowed(VisitMasterKind::Types, &d).is_err());
        assert!(assert_fields_allowed(VisitMasterKind::Frequencies, &d).is_ok());
        let d = parse(&obj(json!({"isDefault": true})), false).unwrap();
        assert!(assert_fields_allowed(VisitMasterKind::Frequencies, &d).is_err());
        assert!(assert_fields_allowed(VisitMasterKind::Types, &d).is_ok());
    }

    #[test]
    fn list_query_rules() {
        let q = parse_list(&obj(
            json!({"isActive": "true", "search": "x", "page": "2"}),
        ))
        .unwrap();
        assert_eq!((q.is_active, q.page, q.limit), (Some(true), 2, 20));
        assert!(q.search.is_some() && q.is_default.is_none());
        let e = parse_list(&obj(json!({"isActive": "yes", "limit": "0"}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                "isActive must be a boolean value",
                "limit must not be less than 1"
            ]
        );
    }

    #[test]
    fn kind_pipe_values() {
        assert_eq!(
            VisitMasterKind::parse("types"),
            Some(VisitMasterKind::Types)
        );
        assert_eq!(VisitMasterKind::parse("Types"), None);
        assert!(kind_pipe("x").is_err());
    }
}
