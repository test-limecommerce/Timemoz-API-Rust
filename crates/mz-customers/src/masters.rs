//! Port `src/customers/{admin-customer-masters.controller,customer-masters.service,
//! customer-master.delegate}.ts`: `/v1/admin/customer-masters/:kind` (5 route; `kind` = `types` |
//! `groups` | `channels` | `sub-channels`) + resolver `pub` untuk modul customer/toko/order.
//! Model `customerType|Group|Channel|SubChannel` ber-audit (`AuditHandle`).

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_sales_geo::dto::{Field, Raw};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::dto::{
    ListMastersQuery, UpdateMaster, parse_create_master, parse_list_masters, parse_update_master,
};

/// 500 (galat Prisma/TypeError Node yang tak ditangkap).
pub(crate) fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

/// Kolom `@db.Uuid`: string non-UUID ditolak Prisma (P2023) → 500 di Node.
pub(crate) fn parse_row_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| internal(&m))
}

pub(crate) fn coded(status: StatusCode, code: &str, message: String) -> ApiError {
    ApiError::body(status, json!({ "code": code, "message": message }))
}

/// `master_code_ok`: pola kode master (`CUSTOMER_MASTER_CODE_PATTERN`).
pub fn master_code_ok(s: &str) -> bool {
    crate::dto::code_pattern_ok(s)
}

// ───────────────────────── kind ─────────────────────────

/// `CustomerMasterKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKind {
    Types,
    Groups,
    Channels,
    SubChannels,
}

impl MasterKind {
    /// `ParseEnumPipe(CustomerMasterKind)`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "types" => Some(Self::Types),
            "groups" => Some(Self::Groups),
            "channels" => Some(Self::Channels),
            "sub-channels" => Some(Self::SubChannels),
            _ => None,
        }
    }

    /// `MODEL_BY_KIND` (nama tabel, sudah dikutip).
    pub const fn table(self) -> &'static str {
        match self {
            Self::Types => r#""customerType""#,
            Self::Groups => r#""customerGroup""#,
            Self::Channels => r#""customerChannel""#,
            Self::SubChannels => r#""customerSubChannel""#,
        }
    }

    pub const fn model(self) -> Model {
        match self {
            Self::Types => Model::CustomerType,
            Self::Groups => Model::CustomerGroup,
            Self::Channels => Model::CustomerChannel,
            Self::SubChannels => Model::CustomerSubChannel,
        }
    }

    /// `LABEL_BY_KIND`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Types => "Customer type",
            Self::Groups => "Customer group",
            Self::Channels => "Customer channel",
            Self::SubChannels => "Customer sub channel",
        }
    }

    const fn columns(self) -> &'static str {
        match self {
            Self::SubChannels => {
                r#"id, uuid::text AS uuid, "companyId", code, name, description, "isActive",
                   "createdAt", "updatedAt", "channelId""#
            }
            _ => {
                r#"id, uuid::text AS uuid, "companyId", code, name, description, "isActive",
                   "createdAt", "updatedAt", NULL::int AS "channelId""#
            }
        }
    }

    const fn is_sub(self) -> bool {
        matches!(self, Self::SubChannels)
    }
}

/// `:kind` (+ `:id` bila ada). `kind` divalidasi `ParseEnumPipe` SEBELUM body/query divalidasi
/// (urutan parameter Nest); `:id` dilewatkan apa adanya (tanpa pipe).
pub struct KindParams(pub MasterKind, pub Option<String>);

impl FromRequestParts<AppState> for KindParams {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path(map): Path<std::collections::HashMap<String, String>> =
            Path::from_request_parts(parts, state)
                .await
                .map_err(|e| internal(&e.to_string()))?;
        let kind = map
            .get("kind")
            .and_then(|k| MasterKind::parse(k))
            .ok_or_else(|| ApiError::bad_request("Validation failed (enum string is expected)"))?;
        Ok(Self(kind, map.get("id").cloned()))
    }
}

// ───────────────────────── baris & entitas ─────────────────────────

/// `MasterRow` (kolom `channelId` hanya terisi pada `sub-channels`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MasterRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    #[sqlx(rename = "channelId")]
    pub channel_id: Option<i32>,
}

/// `ChannelRef` {uuid, code, name}.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelRef {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
}

/// Kunci `channel` pada entitas: tidak ada (kind selain `sub-channels`), `null`, atau objek.
pub enum ChannelKey {
    Absent,
    Ref(Option<ChannelRef>),
}

/// `CustomerMastersService.toEntity` (waktu = epoch ms).
pub fn to_entity(row: &MasterRow, channel: &ChannelKey) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(row.uuid));
    m.insert("code".into(), json!(row.code));
    m.insert("name".into(), json!(row.name));
    m.insert("description".into(), json!(row.description));
    m.insert("isActive".into(), json!(row.is_active));
    if let ChannelKey::Ref(r) = channel {
        m.insert(
            "channel".into(),
            match r {
                Some(c) => json!({ "id": c.uuid, "code": c.code, "name": c.name }),
                None => Value::Null,
            },
        );
    }
    m.insert("createdAt".into(), json!(row.created_at.timestamp_millis()));
    m.insert(
        "updatedAt".into(),
        json!(row.updated_at.map(|d| d.timestamp_millis())),
    );
    Value::Object(m)
}

// ───────────────────────── pembaca (pub untuk modul lain) ─────────────────────────

/// `masterDelegate(client, kind).findFirst({ where: { uuid, companyId } })`.
pub async fn find_by_uuid<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuid: uuid::Uuid,
) -> sqlx::Result<Option<MasterRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {} FROM {} WHERE uuid = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#,
        kind.columns(),
        kind.table()
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

/// `findFirst({ where: { id, companyId } })`.
pub async fn find_by_id<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    id: i32,
) -> sqlx::Result<Option<MasterRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {} FROM {} WHERE id = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#,
        kind.columns(),
        kind.table()
    ))
    .bind(id)
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

/// `CustomersService.resolveActiveMaster`: 404 `CUSTOMER_MASTER_NOT_FOUND`, 422
/// `CUSTOMER_MASTER_ARCHIVED`. `exec` boleh transaksi pemanggil.
pub async fn resolve_active<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuid: uuid::Uuid,
) -> ApiResult<MasterRow> {
    let row = find_by_uuid(exec, kind, company_id, uuid).await?;
    let Some(row) = row else {
        return Err(coded(
            StatusCode::NOT_FOUND,
            "CUSTOMER_MASTER_NOT_FOUND",
            "Customer master row not found".into(),
        ));
    };
    if !row.is_active {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CUSTOMER_MASTER_ARCHIVED",
            "Cannot assign an archived customer master row".into(),
        ));
    }
    Ok(row)
}

/// `channelRefs(companyId, ids)`.
async fn channel_refs<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    ids: &[i32],
) -> sqlx::Result<Vec<ChannelRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, code, name FROM "customerChannel"
            WHERE "companyId" = $1 AND id = ANY($2) ORDER BY id"#,
    )
    .bind(company_id)
    .bind(ids)
    .fetch_all(exec)
    .await
}

async fn one_channel_ref(
    state: &AppState,
    company_id: i32,
    id: i32,
) -> ApiResult<Option<ChannelRef>> {
    Ok(channel_refs(&state.db, company_id, &[id])
        .await?
        .into_iter()
        .next())
}

async fn find_row_or_throw(
    state: &AppState,
    kind: MasterKind,
    company_id: i32,
    raw_uuid: &str,
) -> ApiResult<MasterRow> {
    let uuid = parse_row_uuid(raw_uuid)?;
    find_by_uuid(&state.db, kind, company_id, uuid)
        .await?
        .ok_or_else(|| {
            coded(
                StatusCode::NOT_FOUND,
                "CUSTOMER_MASTER_NOT_FOUND",
                format!("{} not found", kind.label()),
            )
        })
}

/// `resolveActiveChannelId(companyId, channelUuid, client)`.
async fn resolve_active_channel_id<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    channel_uuid: &str,
) -> ApiResult<i32> {
    let uuid = parse_row_uuid(channel_uuid)?;
    let channel = find_by_uuid(exec, MasterKind::Channels, company_id, uuid).await?;
    let Some(channel) = channel else {
        return Err(coded(
            StatusCode::NOT_FOUND,
            "CUSTOMER_MASTER_NOT_FOUND",
            "Customer channel not found".into(),
        ));
    };
    if !channel.is_active {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CUSTOMER_MASTER_ARCHIVED",
            "Cannot assign an archived customer channel".into(),
        ));
    }
    Ok(channel.id)
}

/// `codeTaken` (P2002 pada `@@unique([companyId, code])`).
fn code_taken(kind: MasterKind, code: &str) -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "CUSTOMER_MASTER_CODE_TAKEN",
        format!(
            "{} code {code} is already used in this company",
            kind.label()
        ),
    )
}

fn map_write_error(e: sqlx::Error, kind: MasterKind, code: &str) -> ApiError {
    if mz_core::db::is_unique_violation(&e) {
        return code_taken(kind, code);
    }
    e.into()
}

/// `assertChannelFieldAllowed`.
fn assert_channel_field_allowed(kind: MasterKind, channel_id: Option<&str>) -> ApiResult<()> {
    if !kind.is_sub() && channel_id.is_some() {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CUSTOMER_MASTER_INVALID_FIELD",
            format!(
                "channelId is only accepted for {}",
                MasterKind::SubChannels.label()
            ),
        ));
    }
    Ok(())
}

/// `assertSubChannelNotInUse`: memindah sub channel ke channel lain ditolak selama ada customer
/// aktif yang memakainya. Jalan di transaksi yang sama dengan penulisan.
async fn assert_sub_channel_not_in_use(
    conn: &mut PgConnection,
    company_id: i32,
    sub_channel_id: i32,
) -> ApiResult<()> {
    let in_use: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM customer
            WHERE "companyId" = $1 AND "subChannelId" = $2 AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .bind(sub_channel_id)
    .fetch_one(&mut *conn)
    .await?;
    if in_use > 0 {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "statusCode": 409,
                "error": "SUBCHANNEL_IN_USE",
                "message": format!(
                    "Sub channel is still used by {in_use} customer(s) under its current channel. \
                     Reassign them before moving it to another channel."
                ),
            }),
        ));
    }
    Ok(())
}

// ───────────────────────── service ─────────────────────────

async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    dto: &crate::dto::CreateMaster,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_channel_field_allowed(kind, dto.channel_id.as_deref())?;
    if kind.is_sub() && dto.channel_id.is_none() {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SUBCHANNEL_WITHOUT_CHANNEL",
            "channelId is required to create a sub channel".into(),
        ));
    }
    let channel_id = match &dto.channel_id {
        Some(u) => Some(resolve_active_channel_id(&state.db, company_id, u).await?),
        None => None,
    };

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"INSERT INTO {} ("companyId", code, name, description, "createdById""#,
        kind.table()
    ));
    if channel_id.is_some() {
        qb.push(r#", "channelId""#);
    }
    qb.push(") VALUES (")
        .push_bind(company_id)
        .push(", ")
        .push_bind(dto.code.clone())
        .push(", ")
        .push_bind(dto.name.clone())
        .push(", ")
        .push_bind(dto.description.clone())
        .push(", ")
        .push_bind(user_id);
    if let Some(c) = channel_id {
        qb.push(", ").push_bind(c);
    }
    qb.push(") RETURNING id");
    let id: i32 = qb
        .build_query_scalar()
        .fetch_one(&state.db)
        .await
        .map_err(|e| map_write_error(e, kind, &dto.code))?;
    if let Some(r) = audit
        .result_row(&state.db, kind.model(), Lookup::Id(id))
        .await
    {
        audit.record_create(kind.model(), &r).await;
    }
    let row: MasterRow = sqlx::query_as(&format!(
        "SELECT {} FROM {} WHERE id = $1",
        kind.columns(),
        kind.table()
    ))
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let key = match channel_id {
        Some(c) => ChannelKey::Ref(one_channel_ref(state, company_id, c).await?),
        None => ChannelKey::Absent,
    };
    Ok(to_entity(&row, &key))
}

async fn find_all(
    state: &AppState,
    user_id: i32,
    kind: MasterKind,
    q: &ListMastersQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let channel_id = match (&q.channel_id, kind.is_sub()) {
        (Some(u), true) => Some(resolve_active_channel_id(&state.db, company_id, u).await?),
        _ => None,
    };
    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(format!(r#" FROM {} WHERE "companyId" = "#, kind.table()))
            .push_bind(company_id);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(c) = channel_id {
            qb.push(r#" AND "channelId" = "#).push_bind(c);
        }
        // Prisma `contains` + `mode: insensitive` = ILIKE (tanpa escape `%`/`_`, sama dengan Node).
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND (code ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR name ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR description ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%')");
        }
    };
    let skip = (q.page - 1).saturating_mul(q.limit);
    let mut list = QueryBuilder::<Postgres>::new(format!("SELECT {}", kind.columns()));
    push_where(&mut list);
    list.push(" ORDER BY name ASC, id ASC OFFSET ")
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<MasterRow> = list.build_query_as().fetch_all(&state.db).await?;
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let refs = if kind.is_sub() {
        let mut ids: Vec<i32> = rows.iter().filter_map(|r| r.channel_id).collect();
        ids.sort_unstable();
        ids.dedup();
        channel_refs(&state.db, company_id, &ids).await?
    } else {
        Vec::new()
    };
    let data: Vec<Value> = rows
        .iter()
        .map(|row| {
            let key = if kind.is_sub() {
                ChannelKey::Ref(
                    row.channel_id
                        .and_then(|c| refs.iter().find(|r| r.id == c).cloned()),
                )
            } else {
                ChannelKey::Absent
            };
            to_entity(row, &key)
        })
        .collect();
    Ok(json!({
        "data": data,
        "meta": {
            "total": total,
            "page": q.page,
            "limit": q.limit,
            "totalPages": (total + q.limit - 1) / q.limit,
        },
    }))
}

async fn find_one(
    state: &AppState,
    user_id: i32,
    kind: MasterKind,
    raw_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, raw_uuid).await?;
    let key = match (kind.is_sub(), row.channel_id) {
        (true, Some(c)) => ChannelKey::Ref(one_channel_ref(state, company_id, c).await?),
        _ => ChannelKey::Absent,
    };
    Ok(to_entity(&row, &key))
}

async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    raw_uuid: &str,
    dto: &UpdateMaster,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, raw_uuid).await?;
    let channel_str = match &dto.channel_id {
        Field::Val(s) => Some(s.as_str()),
        _ => None,
    };
    assert_channel_field_allowed(kind, channel_str)?;

    let mut tx = state.db.begin().await?;
    let channel_id: Option<i32> = match (&dto.channel_id, kind.is_sub()) {
        (Field::Val(u), true) => Some(resolve_active_channel_id(&mut *tx, company_id, u).await?),
        // `channelId: null` di PATCH → Prisma menolak `uuid: null` → 500 (ditiru).
        (Field::Null, true) => return Err(internal("Argument `uuid` must not be null")),
        _ => None,
    };
    // Hanya pemindahan sungguhan yang bisa membuat customer yatim.
    if let Some(c) = channel_id
        && Some(c) != row.channel_id
    {
        assert_sub_channel_not_in_use(&mut tx, company_id, row.id).await?;
    }
    // `null` pada kolom non-null: Prisma melempar → 500.
    for (f, name) in [(&dto.code, "code"), (&dto.name, "name")] {
        if matches!(f, Field::Null) {
            return Err(internal(&format!("Argument `{name}` must not be null")));
        }
    }
    if matches!(dto.is_active, Field::Null) {
        return Err(internal("Argument `isActive` must not be null"));
    }

    let pre = audit.pre_read(kind.model(), Lookup::Id(row.id)).await;
    let mut qb = QueryBuilder::<Postgres>::new(format!("UPDATE {} SET ", kind.table()));
    if let Field::Val(c) = &dto.code {
        qb.push("code = ").push_bind(c.clone()).push(", ");
    }
    if let Field::Val(n) = &dto.name {
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
    if let Field::Val(a) = dto.is_active {
        qb.push(r#""isActive" = "#).push_bind(a).push(", ");
    }
    if let Some(c) = channel_id {
        qb.push(r#""channelId" = "#).push_bind(c).push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(row.id)
        .push(format!(" RETURNING {}", kind.columns()));
    let code_for_error = match &dto.code {
        Field::Val(c) => c.clone(),
        _ => row.code.clone(),
    };
    let updated: MasterRow = qb
        .build_query_as()
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_write_error(e, kind, &code_for_error))?;
    if let Some(r) = audit
        .result_row(&mut *tx, kind.model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.model(), pre, &r).await;
    }
    tx.commit()
        .await
        .map_err(|e| map_write_error(e, kind, &code_for_error))?;

    let final_channel = updated.channel_id.or(row.channel_id);
    let key = match (kind.is_sub(), final_channel) {
        (true, Some(c)) => ChannelKey::Ref(one_channel_ref(state, company_id, c).await?),
        _ => ChannelKey::Absent,
    };
    Ok(to_entity(&updated, &key))
}

async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    raw_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, raw_uuid).await?;
    let pre = audit.pre_read(kind.model(), Lookup::Id(row.id)).await;
    sqlx::query(&format!(
        r#"UPDATE {} SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
        kind.table()
    ))
    .bind(row.id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, kind.model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.model(), pre, &r).await;
    }
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── handler ─────────────────────────

/// `:id` wajib ada pada route `/{kind}/{id}`.
fn id_of(id: Option<String>) -> ApiResult<String> {
    id.ok_or_else(|| internal("missing :id"))
}

/// Node: src/customers/admin-customer-masters.controller.ts:114 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindParams(kind, _): KindParams,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list_masters(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        find_all(&state, ctx.require_user_id()?, kind, &q).await?,
    ))
}

/// Node: src/customers/admin-customer-masters.controller.ts:137 `create` (201)
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, _): KindParams,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_create_master(&body.0).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, kind, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/customers/admin-customer-masters.controller.ts:171 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindParams(kind, id): KindParams,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, kind, &id_of(id)?).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/customers/admin-customer-masters.controller.ts:187 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, id): KindParams,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_update_master(&body.0).map_err(ApiError::Validation)?;
    let data = update(
        &state,
        &audit,
        ctx.require_user_id()?,
        kind,
        &id_of(id)?,
        &dto,
    )
    .await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/customers/admin-customer-masters.controller.ts:241 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, id): KindParams,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, kind, &id_of(id)?).await?;
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parse_is_exact() {
        assert_eq!(MasterKind::parse("types"), Some(MasterKind::Types));
        assert_eq!(
            MasterKind::parse("sub-channels"),
            Some(MasterKind::SubChannels)
        );
        assert_eq!(MasterKind::parse("subChannels"), None);
        assert_eq!(MasterKind::parse("Types"), None);
        assert_eq!(MasterKind::parse(""), None);
    }

    #[test]
    fn each_kind_maps_to_its_own_table_and_label() {
        let tables: Vec<&str> = [
            MasterKind::Types,
            MasterKind::Groups,
            MasterKind::Channels,
            MasterKind::SubChannels,
        ]
        .iter()
        .map(|k| k.table())
        .collect();
        assert_eq!(
            tables,
            [
                r#""customerType""#,
                r#""customerGroup""#,
                r#""customerChannel""#,
                r#""customerSubChannel""#
            ]
        );
        assert_eq!(MasterKind::SubChannels.label(), "Customer sub channel");
    }

    #[test]
    fn channel_field_only_for_sub_channels() {
        assert!(assert_channel_field_allowed(MasterKind::Types, None).is_ok());
        assert!(assert_channel_field_allowed(MasterKind::SubChannels, Some("x")).is_ok());
        let err = assert_channel_field_allowed(MasterKind::Groups, Some("x")).unwrap_err();
        match err {
            ApiError::Body { status, body } => {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(body["code"], "CUSTOMER_MASTER_INVALID_FIELD");
                assert_eq!(
                    body["message"],
                    "channelId is only accepted for Customer sub channel"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    fn row(channel_id: Option<i32>) -> MasterRow {
        MasterRow {
            id: 1,
            uuid: "u".into(),
            company_id: 1,
            code: "DIST".into(),
            name: "Distributor".into(),
            description: None,
            is_active: true,
            created_at: JsDate::from_millis(1_786_000_000_000).unwrap(),
            updated_at: None,
            channel_id,
        }
    }

    #[test]
    fn entity_shape_omits_channel_for_plain_kinds() {
        let v = to_entity(&row(None), &ChannelKey::Absent);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "code",
                "name",
                "description",
                "isActive",
                "createdAt",
                "updatedAt"
            ]
        );
        assert_eq!(v["createdAt"], 1_786_000_000_000_i64);
        assert_eq!(v["updatedAt"], Value::Null);
    }

    #[test]
    fn entity_shape_has_channel_key_for_sub_channels() {
        let r = ChannelRef {
            id: 2,
            uuid: "cu".into(),
            code: "GT".into(),
            name: "General Trade".into(),
        };
        let v = to_entity(&row(Some(2)), &ChannelKey::Ref(Some(r)));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys[4..6], ["isActive", "channel"]);
        assert_eq!(
            v["channel"],
            json!({"id":"cu","code":"GT","name":"General Trade"})
        );
        let v = to_entity(&row(Some(2)), &ChannelKey::Ref(None));
        assert_eq!(v["channel"], Value::Null);
    }

    #[test]
    fn code_taken_message() {
        match code_taken(MasterKind::Types, "DIST") {
            ApiError::Body { status, body } => {
                assert_eq!(status, StatusCode::CONFLICT);
                assert_eq!(
                    body,
                    json!({"code":"CUSTOMER_MASTER_CODE_TAKEN","message":"Customer type code DIST is already used in this company"})
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
