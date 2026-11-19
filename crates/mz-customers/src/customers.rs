//! Port `src/customers/admin-customers.controller.ts` + `customers.service.ts` +
//! `customer.delegate.ts`: `/v1/admin/customers` (6 route: list, create, export, detail, update,
//! delete). Model `customer` ber-audit (`AuditHandle`).
//!
//! `@TableFields('customers', { path: 'data' })` (list/detail/create/update): kebijakan kolom
//! dijalankan di handler lewat `mz_table_fields` (`policy_for_request` → `censor_response_body`).
//! `search` memangkas cabang kolom tersensor dan `sort` ke kolom tersensor ditolak
//! (`400 FIELD_NOT_SORTABLE`). Export melewati interceptor, jadi `export::build_customers_xlsx`
//! menyensor sendiri.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_sales_geo::dto::{Field, Raw};
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::{CensoredColumns, policy_for_request};
use mz_table_fields::reveal::{RevealRequest, RevealResolver, RevealTarget};
use mz_table_fields::search_sort::{allowed_search_branches, assert_sortable_column};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::default_types::DEFAULT_CUSTOMER_TYPE_CODE;
use crate::dto::{
    CreateCustomer, ListCustomersQuery, SortBy, SortDir, UpdateCustomer, parse_create_customer,
    parse_list_customers, parse_update_customer,
};
use crate::export::build_customers_xlsx;
use crate::masters::{
    MasterKind, MasterRow, coded, find_by_id, internal, parse_row_uuid, resolve_active,
};
use crate::phone::normalize_store_phone;
use crate::tax::normalize_customer_tax_number;

/// Kunci tabel di registry `table-fields`.
const TABLE_KEY: &str = "customers";

// ───────────────────────── baris & entitas ─────────────────────────

/// Satu baris `customer` + ref master + jumlah toko hidup (`CUSTOMER_SELECT_WITH_STORE_COUNT`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomerRow {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "channelId")]
    pub channel_id: Option<i32>,
    #[sqlx(rename = "subChannelId")]
    pub sub_channel_id: Option<i32>,
    #[sqlx(rename = "taxNumber")]
    pub tax_number: Option<String>,
    pub phone: Option<String>,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    pub type_uuid: Option<String>,
    pub type_code: Option<String>,
    pub type_name: Option<String>,
    pub group_uuid: Option<String>,
    pub group_code: Option<String>,
    pub group_name: Option<String>,
    pub channel_uuid: Option<String>,
    pub channel_code: Option<String>,
    pub channel_name: Option<String>,
    pub sub_channel_uuid: Option<String>,
    pub sub_channel_code: Option<String>,
    pub sub_channel_name: Option<String>,
    pub price_list_uuid: Option<String>,
    pub price_list_code: Option<String>,
    pub price_list_name: Option<String>,
    pub store_count: i64,
}

/// `SELECT ... FROM customer c LEFT JOIN <5 ref>` (tanpa `WHERE`).
pub const SELECT_CUSTOMER: &str = r#"
SELECT c.id, c.uuid::text AS uuid, c.code, c.name, c."channelId", c."subChannelId",
       c."taxNumber", c.phone, c."isActive", c."createdAt", c."updatedAt",
       t.uuid::text AS type_uuid, t.code AS type_code, t.name AS type_name,
       g.uuid::text AS group_uuid, g.code AS group_code, g.name AS group_name,
       ch.uuid::text AS channel_uuid, ch.code AS channel_code, ch.name AS channel_name,
       sc.uuid::text AS sub_channel_uuid, sc.code AS sub_channel_code, sc.name AS sub_channel_name,
       pl.uuid::text AS price_list_uuid, pl.code AS price_list_code, pl.name AS price_list_name,
       (SELECT COUNT(*) FROM store s WHERE s."customerId" = c.id AND s."deletedAt" IS NULL) AS store_count
  FROM customer c
  LEFT JOIN "customerType" t ON t.id = c."typeId"
  LEFT JOIN "customerGroup" g ON g.id = c."groupId"
  LEFT JOIN "customerChannel" ch ON ch.id = c."channelId"
  LEFT JOIN "customerSubChannel" sc ON sc.id = c."subChannelId"
  LEFT JOIN "priceList" pl ON pl.id = c."priceListId"
"#;

type RefTriple<'a> = (&'a Option<String>, &'a Option<String>, &'a Option<String>);

/// `ref(r)`: `{ id: uuid, code, name }` atau `null`.
fn ref_json(r: RefTriple<'_>) -> Value {
    match r {
        (Some(uuid), Some(code), Some(name)) => json!({ "id": uuid, "code": code, "name": name }),
        _ => Value::Null,
    }
}

impl CustomerRow {
    /// `CustomersService.toEntity`.
    pub fn to_entity(&self) -> Value {
        json!({
            "id": self.uuid,
            "code": self.code,
            "name": self.name,
            "type": ref_json((&self.type_uuid, &self.type_code, &self.type_name)),
            "group": ref_json((&self.group_uuid, &self.group_code, &self.group_name)),
            "channel": ref_json((&self.channel_uuid, &self.channel_code, &self.channel_name)),
            "subChannel": ref_json((&self.sub_channel_uuid, &self.sub_channel_code, &self.sub_channel_name)),
            "priceList": ref_json((&self.price_list_uuid, &self.price_list_code, &self.price_list_name)),
            "taxNumber": self.tax_number,
            "phone": self.phone,
            "isActive": self.is_active,
            "storeCount": self.store_count,
            "createdAt": self.created_at.timestamp_millis(),
            "updatedAt": self.updated_at.map(|d| d.timestamp_millis()),
        })
    }

    fn pair<'a>(code: &'a Option<String>, name: &'a Option<String>) -> Option<(&'a str, &'a str)> {
        match (code, name) {
            (Some(c), Some(n)) => Some((c.as_str(), n.as_str())),
            _ => None,
        }
    }

    /// `(code, name)` ref tipe (export), `None` bila belum dipilih.
    pub fn type_ref(&self) -> Option<(&str, &str)> {
        Self::pair(&self.type_code, &self.type_name)
    }

    pub fn group_ref(&self) -> Option<(&str, &str)> {
        Self::pair(&self.group_code, &self.group_name)
    }

    pub fn channel_ref(&self) -> Option<(&str, &str)> {
        Self::pair(&self.channel_code, &self.channel_name)
    }

    pub fn sub_channel_ref(&self) -> Option<(&str, &str)> {
        Self::pair(&self.sub_channel_code, &self.sub_channel_name)
    }
}

// ───────────────────────── pembantu ─────────────────────────

fn code_taken(code: &str) -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "CUSTOMER_CODE_TAKEN",
        format!("Customer code {code} is already used in this company"),
    )
}

/// P2002 → `CUSTOMER_CODE_TAKEN` (unique parsial `(companyId, code)` pada baris hidup).
fn map_write_error(e: sqlx::Error, code: &str) -> ApiError {
    if mz_core::db::is_unique_violation(&e) {
        return code_taken(code);
    }
    e.into()
}

/// `CustomersService.resolveActiveMaster` (uuid dari DTO sudah v4; non-UUID → P2023 → 500).
async fn resolve_active_master<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuid: &str,
) -> ApiResult<MasterRow> {
    resolve_active(exec, kind, company_id, parse_row_uuid(uuid)?).await
}

/// `CustomersService.defaultType` (§14.3): tipe `DISTRIBUTOR` bila ada dan aktif, selain itu `None`
/// (company lama tanpa backfill, atau admin yang mengarsipkannya, tidak boleh membuat POST gagal).
async fn find_default_type<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<Option<i32>> {
    let row: Option<(i32, bool)> = sqlx::query_as(&format!(
        r#"SELECT id, "isActive" FROM {} WHERE "companyId" = $1 AND code = $2 ORDER BY id LIMIT 1"#,
        MasterKind::Types.table()
    ))
    .bind(company_id)
    .bind(DEFAULT_CUSTOMER_TYPE_CODE)
    .fetch_optional(exec)
    .await?;
    Ok(row.filter(|(_, active)| *active).map(|(id, _)| id))
}

/// `findRowOrThrow`: 404 `CUSTOMER_NOT_FOUND`. `uuid` bukan UUID → P2023 → 500 (seperti Node).
pub async fn find_row_or_throw<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    uuid: &str,
) -> ApiResult<CustomerRow> {
    let parsed = parse_row_uuid(uuid)?;
    sqlx::query_as::<_, CustomerRow>(&format!(
        r#"{SELECT_CUSTOMER} WHERE c.uuid = $1 AND c."companyId" = $2 AND c."deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(parsed)
    .bind(company_id)
    .fetch_optional(exec)
    .await?
    .ok_or_else(|| coded(
            StatusCode::NOT_FOUND,
            "CUSTOMER_NOT_FOUND",
            "Customer not found".into(),
        ))
}

/// `resolvePriceList`: tanpa cek `isActive`/rentang tanggal (price list kedaluwarsa boleh dipasang);
/// 404 `PRICE_LIST_NOT_FOUND`. Mengembalikan id internal.
async fn resolve_price_list(
    conn: &mut PgConnection,
    company_id: i32,
    uuid: &str,
) -> ApiResult<i32> {
    let parsed = parse_row_uuid(uuid)?;
    sqlx::query_scalar(
        r#"SELECT id FROM "priceList" WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL
            ORDER BY id LIMIT 1"#,
    )
    .bind(parsed)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| {
        coded(
            StatusCode::NOT_FOUND,
            "PRICE_LIST_NOT_FOUND",
            "Price list not found".into(),
        )
    })
}

/// `assertSubChannelConsistency` (§5.7) atas keadaan AKHIR (id internal).
async fn assert_sub_channel_consistency(
    conn: &mut PgConnection,
    company_id: i32,
    final_channel_id: Option<i32>,
    final_sub_channel_id: Option<i32>,
) -> ApiResult<()> {
    let Some(sub_id) = final_sub_channel_id else {
        return Ok(());
    };
    let Some(channel_id) = final_channel_id else {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SUBCHANNEL_WITHOUT_CHANNEL",
            "channelId is required when subChannelId is set".into(),
        ));
    };
    let sub = find_by_id(&mut *conn, MasterKind::SubChannels, company_id, sub_id).await?;
    if sub.and_then(|s| s.channel_id) != Some(channel_id) {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SUBCHANNEL_CHANNEL_MISMATCH",
            "subChannelId does not belong to channelId".into(),
        ));
    }
    Ok(())
}

fn meta(total: i64, page: i64, limit: i64) -> Value {
    json!({
        "total": total,
        "page": page,
        "limit": limit,
        "totalPages": (total + limit - 1) / limit,
    })
}

/// `buildOrderBy`: `[{ <kolom>: dir }, { id: 'asc' }]`, bawaan `name asc`.
fn push_order_by(
    qb: &mut QueryBuilder<'_, Postgres>,
    sort_by: Option<SortBy>,
    dir: Option<SortDir>,
) {
    let col = match sort_by {
        Some(SortBy::Code) => "c.code",
        Some(SortBy::CreatedAt) => r#"c."createdAt""#,
        Some(SortBy::Name) | None => "c.name",
    };
    qb.push(format!(
        " ORDER BY {col} {}, c.id ASC",
        dir.unwrap_or(SortDir::Asc).sql()
    ));
}

/// uuid[] filter → id internal[] dalam company ini; baris yang tak ada dibuang (`resolveManyIds`).
/// Daftar kosong/tidak ada = tanpa filter (`None`); daftar yang tak satu pun cocok = `Some([])`
/// (tidak ada baris yang lolos).
async fn resolve_many_ids(
    state: &AppState,
    company_id: i32,
    kind: MasterKind,
    uuids: Option<&[String]>,
) -> ApiResult<Option<Vec<i32>>> {
    let Some(uuids) = uuids.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let parsed: Vec<uuid::Uuid> = uuids
        .iter()
        .map(|u| parse_row_uuid(u))
        .collect::<ApiResult<_>>()?;
    let ids: Vec<i32> = sqlx::query_scalar(&format!(
        r#"SELECT id FROM {} WHERE "companyId" = $1 AND uuid = ANY($2) ORDER BY id"#,
        kind.table()
    ))
    .bind(company_id)
    .bind(&parsed)
    .fetch_all(&state.db)
    .await?;
    Ok(Some(ids))
}

/// Filter terselesaikan (`buildWhere`), dipakai list DAN export (filter export = filter list).
struct Where {
    company_id: i32,
    is_active: Option<bool>,
    type_ids: Option<Vec<i32>>,
    group_ids: Option<Vec<i32>>,
    channel_ids: Option<Vec<i32>>,
    sub_channel_ids: Option<Vec<i32>>,
    has_stores: Option<bool>,
    search: Option<String>,
    /// Cabang `search` yang lolos kebijakan (`allowedSearchBranches`).
    search_branches: Vec<&'static str>,
}

async fn build_where(
    state: &AppState,
    company_id: i32,
    q: &ListCustomersQuery,
    censored: &CensoredColumns,
) -> ApiResult<Where> {
    let type_ids =
        resolve_many_ids(state, company_id, MasterKind::Types, q.type_id.as_deref()).await?;
    let group_ids =
        resolve_many_ids(state, company_id, MasterKind::Groups, q.group_id.as_deref()).await?;
    let channel_ids = resolve_many_ids(
        state,
        company_id,
        MasterKind::Channels,
        q.channel_id.as_deref(),
    )
    .await?;
    let sub_channel_ids = resolve_many_ids(
        state,
        company_id,
        MasterKind::SubChannels,
        q.sub_channel_id.as_deref(),
    )
    .await?;
    // §6.4: `taxNumber` ikut `search`, justru karena itu wajib lewat kebijakan.
    let search_branches = allowed_search_branches(
        [
            ("code", "code"),
            ("name", "name"),
            ("taxNumber", "taxNumber"),
        ],
        censored,
    );
    Ok(Where {
        company_id,
        is_active: q.is_active,
        type_ids,
        group_ids,
        channel_ids,
        sub_channel_ids,
        has_stores: q.has_stores,
        search: q.search.clone().filter(|s| !s.is_empty()),
        search_branches,
    })
}

impl Where {
    fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        qb.push(r#" WHERE c."companyId" = "#)
            .push_bind(self.company_id)
            .push(r#" AND c."deletedAt" IS NULL"#);
        if let Some(a) = self.is_active {
            qb.push(r#" AND c."isActive" = "#).push_bind(a);
        }
        for (col, ids) in [
            (r#"c."typeId""#, &self.type_ids),
            (r#"c."groupId""#, &self.group_ids),
            (r#"c."channelId""#, &self.channel_ids),
            (r#"c."subChannelId""#, &self.sub_channel_ids),
        ] {
            if let Some(ids) = ids {
                qb.push(format!(" AND {col} = ANY("))
                    .push_bind(ids.clone())
                    .push(")");
            }
        }
        // `true` = punya >= 1 toko hidup, `false` = tak punya satu pun (kedua nilai bermakna).
        if let Some(has) = self.has_stores {
            qb.push(if has {
                " AND EXISTS"
            } else {
                " AND NOT EXISTS"
            });
            qb.push(
                r#" (SELECT 1 FROM store s WHERE s."customerId" = c.id AND s."deletedAt" IS NULL)"#,
            );
        }
        if let Some(search) = &self.search {
            // `OR: []` Prisma = tidak cocok baris mana pun (kode/nama `locked`, jadi tak pernah kosong).
            if self.search_branches.is_empty() {
                qb.push(" AND FALSE");
                return;
            }
            qb.push(" AND (");
            for (i, col) in self.search_branches.iter().enumerate() {
                if i > 0 {
                    qb.push(" OR ");
                }
                // Prisma `contains` + `mode: insensitive` = ILIKE tanpa escape `%`/`_` (sama dengan Node).
                qb.push(format!(r#"c."{col}" ILIKE '%' || "#))
                    .push_bind(search.clone())
                    .push(" || '%'");
            }
            qb.push(")");
        }
    }
}

// ───────────────────────── service ─────────────────────────

/// `findAll`. `censored` = kebijakan pembaca (sudah dihitung handler).
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    q: &ListCustomersQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    // §6.4: ditolak SEBELUM query berjalan.
    assert_sortable_column(
        TABLE_KEY,
        Some(q.sort_by.map_or("name", SortBy::key)),
        censored,
    )?;
    let filter = build_where(state, company_id, q, censored).await?;

    let skip = (q.page - 1).saturating_mul(q.limit);
    let mut list = QueryBuilder::<Postgres>::new(SELECT_CUSTOMER);
    filter.push(&mut list);
    push_order_by(&mut list, q.sort_by, q.sort_dir);
    list.push(" OFFSET ")
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<CustomerRow> = list.build_query_as().fetch_all(&state.db).await?;

    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM customer c");
    filter.push(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    Ok(json!({
        "data": rows.iter().map(CustomerRow::to_entity).collect::<Vec<_>>(),
        "meta": meta(total, q.page, q.limit),
    }))
}

/// `exportCustomers`: filter sama dengan `find_all`, tanpa page/limit. Mengembalikan isi xlsx.
pub async fn export_customers(
    state: &AppState,
    user_id: i32,
    q: &ListCustomersQuery,
    censored: &CensoredColumns,
) -> ApiResult<Vec<u8>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let time_zone: Option<Option<String>> =
        sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let Some(time_zone) = time_zone else {
        return Err(ApiError::forbidden("No company found for user"));
    };
    let tz = time_zone.unwrap_or_default();
    let filter = build_where(state, company_id, q, censored).await?;

    let mut list = QueryBuilder::<Postgres>::new(SELECT_CUSTOMER);
    filter.push(&mut list);
    push_order_by(&mut list, q.sort_by, q.sort_dir);
    let rows: Vec<CustomerRow> = list.build_query_as().fetch_all(&state.db).await?;
    build_customers_xlsx(&rows, &tz, censored)
        .map_err(|e| ApiError::internal(anyhow::Error::from(e)))
}

pub async fn find_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    Ok(find_row_or_throw(&state.db, company_id, uuid)
        .await?
        .to_entity())
}

/// `findForReveal` (§7.3): tanpa scope wilayah — `customer` tak pernah masuk hitungan otorisasi.
pub async fn find_for_reveal(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> ApiResult<RevealTarget> {
    let row = find_row_or_throw(&state.db, company_id, uuid).await?;
    Ok(RevealTarget {
        record_id: row.id,
        record_uuid: Some(row.uuid.clone()),
        record_label: Some(format!("{} \u{2014} {}", row.code, row.name)),
        record: row.to_entity(),
    })
}

/// `create` (§5.7): resolusi master, cek konsistensi, dan INSERT dalam SATU transaksi.
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateCustomer,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut tx = state.db.begin().await?;

    let type_id = match dto.type_id.as_deref() {
        Some(u) => Some(
            resolve_active_master(&mut *tx, MasterKind::Types, company_id, u)
                .await?
                .id,
        ),
        None => find_default_type(&mut *tx, company_id).await?,
    };
    let group_id = match dto.group_id.as_deref() {
        Some(u) => Some(
            resolve_active_master(&mut *tx, MasterKind::Groups, company_id, u)
                .await?
                .id,
        ),
        None => None,
    };
    let channel_id = match dto.channel_id.as_deref() {
        Some(u) => Some(
            resolve_active_master(&mut *tx, MasterKind::Channels, company_id, u)
                .await?
                .id,
        ),
        None => None,
    };
    let sub_channel_id = match dto.sub_channel_id.as_deref() {
        Some(u) => Some(
            resolve_active_master(&mut *tx, MasterKind::SubChannels, company_id, u)
                .await?
                .id,
        ),
        None => None,
    };
    let price_list_id = match dto.price_list_id.as_deref() {
        Some(u) => Some(resolve_price_list(&mut tx, company_id, u).await?),
        None => None,
    };
    assert_sub_channel_consistency(&mut tx, company_id, channel_id, sub_channel_id).await?;

    // §3.3 — dibersihkan di sini, di samping `phone`, bukan di DTO.
    let tax_number = normalize_customer_tax_number(dto.tax_number.as_deref());
    let phone = normalize_store_phone(dto.phone.as_deref());
    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO customer
             ("companyId", code, name, "typeId", "groupId", "channelId", "subChannelId",
              "priceListId", "taxNumber", phone, "isActive", "createdById")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
           RETURNING id"#,
    )
    .bind(company_id)
    .bind(&dto.code)
    .bind(&dto.name)
    .bind(type_id)
    .bind(group_id)
    .bind(channel_id)
    .bind(sub_channel_id)
    .bind(price_list_id)
    .bind(tax_number)
    .bind(phone)
    .bind(dto.is_active.unwrap_or(true))
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| map_write_error(e, &dto.code))?;

    if let Some(r) = audit
        .result_row(&mut *tx, Model::Customer, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::Customer, &r).await;
    }
    let row = sqlx::query_as::<_, CustomerRow>(&format!("{SELECT_CUSTOMER} WHERE c.id = $1"))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit()
        .await
        .map_err(|e| map_write_error(e, &dto.code))?;
    Ok(row.to_entity())
}

/// Nilai `Field` yang boleh `null` (ref master): `Absent` = biarkan, `Null` = lepas, `Val` = pasang.
enum Ref {
    Keep,
    Clear,
    Set(i32),
}

impl Ref {
    /// Nilai akhir kolom FK sesudah penulisan.
    fn final_id(&self, current: Option<i32>) -> Option<i32> {
        match self {
            Ref::Keep => current,
            Ref::Clear => None,
            Ref::Set(id) => Some(*id),
        }
    }
}

/// Prisma melempar bila kolom non-null (`code`, `name`, `isActive`) diberi `null` → 500.
fn non_null<T: Clone>(f: &Field<T>) -> ApiResult<Option<T>> {
    match f {
        Field::Absent => Ok(None),
        Field::Null => Err(internal("Argument must not be null")),
        Field::Val(v) => Ok(Some(v.clone())),
    }
}

async fn resolve_ref(
    conn: &mut PgConnection,
    kind: MasterKind,
    company_id: i32,
    f: &Field<String>,
) -> ApiResult<Ref> {
    Ok(match f {
        Field::Absent => Ref::Keep,
        Field::Null => Ref::Clear,
        Field::Val(u) => Ref::Set(
            resolve_active_master(&mut *conn, kind, company_id, u)
                .await?
                .id,
        ),
    })
}

/// `update` (§5.7): baca-gabung-validasi-tulis dalam SATU transaksi.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdateCustomer,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    // Dibaca di dalam transaksi, tapi galat P2002 dari PATCH tanpa `code` tetap menyebut kodenya.
    let mut current_code: Option<String> = None;
    let result = update_in_tx(state, audit, company_id, uuid, dto, &mut current_code).await;
    result.map_err(|e| match e {
        UpdateError::Write(e) => {
            let code = match &dto.code {
                Field::Val(c) => c.clone(),
                _ => current_code.unwrap_or_default(),
            };
            map_write_error(e, &code)
        }
        UpdateError::Api(e) => e,
    })
}

enum UpdateError {
    Api(ApiError),
    Write(sqlx::Error),
}

impl From<ApiError> for UpdateError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for UpdateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Write(e)
    }
}

async fn update_in_tx(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    uuid: &str,
    dto: &UpdateCustomer,
    current_code: &mut Option<String>,
) -> Result<Value, UpdateError> {
    let mut tx = state.db.begin().await?;
    let current_row: CustomerRowIds = {
        let row = find_row_or_throw(&mut *tx, company_id, uuid).await?;
        *current_code = Some(row.code.clone());
        let ids: CustomerRowIds =
            sqlx::query_as(r#"SELECT id, "channelId", "subChannelId" FROM customer WHERE id = $1"#)
                .bind(row.id)
                .fetch_one(&mut *tx)
                .await?;
        ids
    };

    // §14 — tiap ref master punya tiga keadaan: tidak dikirim, uuid, `null`.
    let type_ref = resolve_ref(&mut tx, MasterKind::Types, company_id, &dto.type_id).await?;
    let group_ref = resolve_ref(&mut tx, MasterKind::Groups, company_id, &dto.group_id).await?;
    let channel_ref =
        resolve_ref(&mut tx, MasterKind::Channels, company_id, &dto.channel_id).await?;
    let sub_channel_ref = resolve_ref(
        &mut tx,
        MasterKind::SubChannels,
        company_id,
        &dto.sub_channel_id,
    )
    .await?;
    let price_list_ref = match &dto.price_list_id {
        Field::Absent => Ref::Keep,
        Field::Null => Ref::Clear,
        Field::Val(u) => Ref::Set(resolve_price_list(&mut tx, company_id, u).await?),
    };

    // §5.7 — keadaan AKHIR gabungan, bukan payload mentah.
    let final_channel = channel_ref.final_id(current_row.channel_id);
    let final_sub_channel = sub_channel_ref.final_id(current_row.sub_channel_id);
    assert_sub_channel_consistency(&mut tx, company_id, final_channel, final_sub_channel).await?;

    let code = non_null(&dto.code)?;
    let name = non_null(&dto.name)?;
    let is_active = non_null(&dto.is_active)?;

    let mut qb = QueryBuilder::<Postgres>::new("UPDATE customer SET ");
    if let Some(c) = &code {
        qb.push("code = ").push_bind(c.clone()).push(", ");
    }
    if let Some(n) = &name {
        qb.push("name = ").push_bind(n.clone()).push(", ");
    }
    for (col, r) in [
        (r#""typeId" = "#, &type_ref),
        (r#""groupId" = "#, &group_ref),
        (r#""channelId" = "#, &channel_ref),
        (r#""subChannelId" = "#, &sub_channel_ref),
        (r#""priceListId" = "#, &price_list_ref),
    ] {
        match r {
            Ref::Keep => {}
            Ref::Clear => {
                qb.push(col).push("NULL, ");
            }
            Ref::Set(id) => {
                qb.push(col).push_bind(*id).push(", ");
            }
        }
    }
    // §3.3 — dibersihkan di sini, di samping `phone`.
    match &dto.tax_number {
        Field::Absent => {}
        Field::Null => {
            qb.push(r#""taxNumber" = NULL, "#);
        }
        Field::Val(s) => {
            qb.push(r#""taxNumber" = "#)
                .push_bind(normalize_customer_tax_number(Some(s)))
                .push(", ");
        }
    }
    match &dto.phone {
        Field::Absent => {}
        Field::Null => {
            qb.push("phone = NULL, ");
        }
        Field::Val(s) => {
            qb.push("phone = ")
                .push_bind(normalize_store_phone(Some(s)))
                .push(", ");
        }
    }
    if let Some(a) = is_active {
        qb.push(r#""isActive" = "#).push_bind(a).push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(current_row.id);

    let pre = audit
        .pre_read(Model::Customer, Lookup::Id(current_row.id))
        .await;
    qb.build().execute(&mut *tx).await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Customer, Lookup::Id(current_row.id))
        .await
    {
        audit.record_update(Model::Customer, pre, &r).await;
    }
    let row = sqlx::query_as::<_, CustomerRow>(&format!("{SELECT_CUSTOMER} WHERE c.id = $1"))
        .bind(current_row.id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row.to_entity())
}

#[derive(sqlx::FromRow)]
struct CustomerRowIds {
    id: i32,
    #[sqlx(rename = "channelId")]
    channel_id: Option<i32>,
    #[sqlx(rename = "subChannelId")]
    sub_channel_id: Option<i32>,
}

/// `remove` (§5.4): soft delete, 409 `CUSTOMER_HAS_STORES` selama masih punya toko hidup.
/// Hitung + tulis dalam SATU transaksi.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let current = find_row_or_throw(&state.db, company_id, uuid).await?;

    let mut tx = state.db.begin().await?;
    let active_stores: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM store
            WHERE "companyId" = $1 AND "customerId" = $2 AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .bind(current.id)
    .fetch_one(&mut *tx)
    .await?;
    if active_stores > 0 {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "statusCode": 409,
                "error": "CUSTOMER_HAS_STORES",
                "message": format!(
                    "Customer still has {active_stores} stores. Reassign or remove them first."
                ),
            }),
        ));
    }
    let now = now_naive();
    let pre = audit
        .pre_read(Model::Customer, Lookup::Id(current.id))
        .await;
    sqlx::query(r#"UPDATE customer SET "deletedAt" = $2, "updatedAt" = $2 WHERE id = $1"#)
        .bind(current.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Customer, Lookup::Id(current.id))
        .await
    {
        audit.record_update(Model::Customer, pre, &r).await;
    }
    tx.commit().await?;
    Ok(json!({ "id": current.uuid, "deleted": true }))
}

// ───────────────────────── reveal resolver ─────────────────────────

/// `CustomersRevealResolver`: didaftarkan ke registry reveal `table-fields` oleh `router()`.
pub struct CustomersRevealResolver;

impl RevealResolver for CustomersRevealResolver {
    fn resolve<'a>(
        &'a self,
        state: &'a AppState,
        request: RevealRequest,
    ) -> BoxFuture<'a, Result<RevealTarget, ApiError>> {
        Box::pin(
            async move { find_for_reveal(state, request.company_id, &request.record_uuid).await },
        )
    }
}

// ───────────────────────── handler ─────────────────────────

/// `policies.censoredColumns(req, 'customers')`.
async fn censored_for(state: &AppState, ctx: &RequestCtx) -> ApiResult<Arc<CensoredColumns>> {
    Ok(policy_for_request(state, ctx, false)
        .await?
        .for_table(TABLE_KEY))
}

/// Interceptor `@TableFields('customers', { path: 'data' })`: sensor nilai + tempel `fieldPolicy`.
fn censor(mut body: Value, censored: &CensoredColumns) -> Json<Value> {
    censor_response_body(&mut body, TABLE_KEY, "data", censored, None);
    Json(body)
}

/// Node: src/customers/admin-customers.controller.ts:104 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list_customers(&q.0, true).map_err(ApiError::Validation)?;
    let censored = censored_for(&state, &ctx).await?;
    let body = find_all(&state, ctx.require_user_id()?, &q, &censored).await?;
    Ok(censor(body, &censored))
}

/// Node: src/customers/admin-customers.controller.ts:131 `create` (201)
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_create_customer(&body.0).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok((
        StatusCode::CREATED,
        censor(json!({ "data": data }), &censored),
    ))
}

/// Node: src/customers/admin-customers.controller.ts:159 `export`
pub async fn h_export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Response> {
    let q = parse_list_customers(&q.0, false).map_err(ApiError::Validation)?;
    // §6.3: export melewati interceptor (bukan JSON), jadi peta yang sama diteruskan ke generator.
    let censored = censored_for(&state, &ctx).await?;
    let buffer = export_customers(&state, ctx.require_user_id()?, &q, &censored).await?;
    let len = HeaderValue::from_str(&buffer.len().to_string())
        .map_err(|e| ApiError::internal(anyhow::Error::from(e)))?;
    let headers: [(HeaderName, HeaderValue); 3] = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
        ),
        (
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"customers-export.xlsx\""),
        ),
        (header::CONTENT_LENGTH, len),
    ];
    Ok((StatusCode::OK, headers, buffer).into_response())
}

/// Node: src/customers/admin-customers.controller.ts:191 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, &id).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

/// Node: src/customers/admin-customers.controller.ts:203 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_update_customer(&body.0).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

/// Node: src/customers/admin-customers.controller.ts:222 `remove` (bukan `@TableFields`: `{ id, deleted }`)
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// Baris contoh (contoh respons `CUSTOMER_EXAMPLE` Node), semua ref terisi kecuali price list.
    pub fn sample_row() -> CustomerRow {
        CustomerRow {
            id: 1,
            uuid: "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f".into(),
            code: "CUST-001".into(),
            name: "PT Sumber Makmur".into(),
            channel_id: Some(3),
            sub_channel_id: Some(4),
            tax_number: Some("01.234.567.8-901.000".into()),
            phone: Some("+628123456789".into()),
            is_active: true,
            created_at: JsDate::from_millis(1_786_000_000_000).unwrap(),
            updated_at: JsDate::from_millis(1_786_100_000_000),
            type_uuid: Some("t-u".into()),
            type_code: Some("DIST".into()),
            type_name: Some("Distributor".into()),
            group_uuid: Some("g-u".into()),
            group_code: Some("VIP".into()),
            group_name: Some("VIP".into()),
            channel_uuid: Some("c-u".into()),
            channel_code: Some("GT".into()),
            channel_name: Some("General Trade".into()),
            sub_channel_uuid: Some("s-u".into()),
            sub_channel_code: Some("WRG".into()),
            sub_channel_name: Some("Warung".into()),
            price_list_uuid: None,
            price_list_code: None,
            price_list_name: None,
            store_count: 12,
        }
    }
}

#[cfg(test)]
impl CustomerRow {
    /// Kosongkan group/channel/subChannel/priceList (tipe tetap) — uji sel kosong export.
    pub(crate) fn clear_optional_refs(&mut self) {
        self.group_uuid = None;
        self.group_code = None;
        self.group_name = None;
        self.channel_uuid = None;
        self.channel_code = None;
        self.channel_name = None;
        self.sub_channel_uuid = None;
        self.sub_channel_code = None;
        self.sub_channel_name = None;
        self.price_list_uuid = None;
        self.price_list_code = None;
        self.price_list_name = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_table_fields::registry::FieldVisibilityMode;

    fn row() -> CustomerRow {
        tests_support::sample_row()
    }

    #[test]
    fn entity_key_order_and_shape() {
        let v = row().to_entity();
        assert_eq!(
            v.to_string(),
            concat!(
                r#"{"id":"9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f","code":"CUST-001","name":"PT Sumber Makmur","#,
                r#""type":{"id":"t-u","code":"DIST","name":"Distributor"},"group":{"id":"g-u","code":"VIP","name":"VIP"},"#,
                r#""channel":{"id":"c-u","code":"GT","name":"General Trade"},"#,
                r#""subChannel":{"id":"s-u","code":"WRG","name":"Warung"},"priceList":null,"#,
                r#""taxNumber":"01.234.567.8-901.000","phone":"+628123456789","isActive":true,"#,
                r#""storeCount":12,"createdAt":1786000000000,"updatedAt":1786100000000}"#
            )
        );
    }

    #[test]
    fn entity_null_type_and_updated_at() {
        let mut r = row();
        r.type_uuid = None;
        r.type_code = None;
        r.type_name = None;
        r.updated_at = None;
        let v = r.to_entity();
        assert_eq!(v["type"], Value::Null);
        assert_eq!(v["updatedAt"], Value::Null);
    }

    fn censored(entries: &[(&str, FieldVisibilityMode)]) -> CensoredColumns {
        entries.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect()
    }

    // ── penyambungan kebijakan kolom (customer-field-policy.wiring.spec.ts) ──

    #[test]
    fn censor_nulls_hidden_column_and_appends_field_policy() {
        let c = censored(&[("taxNumber", FieldVisibilityMode::Hidden)]);
        let Json(body) = censor(
            json!({"data": [row().to_entity()], "meta": {"total": 1}}),
            &c,
        );
        assert_eq!(body["data"][0]["taxNumber"], Value::Null);
        assert_eq!(body["data"][0]["phone"], "+628123456789");
        assert_eq!(body["fieldPolicy"]["tableKey"], "customers");
        assert_eq!(body["fieldPolicy"]["hidden"], json!(["taxNumber"]));
        // `fieldPolicy` ditempel di AKHIR (sesudah `meta`).
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["data", "meta", "fieldPolicy"]);
    }

    #[test]
    fn censor_applies_to_detail_envelope_and_ref_columns() {
        let c = censored(&[
            ("type", FieldVisibilityMode::Masked),
            ("storeCount", FieldVisibilityMode::Revealable),
        ]);
        let Json(body) = censor(json!({"data": row().to_entity()}), &c);
        assert_eq!(body["data"]["type"], Value::Null);
        assert_eq!(body["data"]["storeCount"], Value::Null);
        assert_eq!(body["data"]["code"], "CUST-001");
        assert_eq!(body["fieldPolicy"]["masked"], json!(["type"]));
        assert_eq!(body["fieldPolicy"]["revealable"], json!(["storeCount"]));
    }

    #[test]
    fn no_policy_leaves_body_without_field_policy() {
        let Json(body) = censor(json!({"data": row().to_entity()}), &CensoredColumns::new());
        assert!(body.get("fieldPolicy").is_none());
        assert_eq!(body["data"]["taxNumber"], "01.234.567.8-901.000");
    }

    #[test]
    fn sort_to_censored_column_is_rejected() {
        let c = censored(&[("createdAt", FieldVisibilityMode::Hidden)]);
        assert!(assert_sortable_column(TABLE_KEY, Some("code"), &c).is_ok());
        assert!(assert_sortable_column(TABLE_KEY, Some("name"), &c).is_ok());
        let e = assert_sortable_column(TABLE_KEY, Some("createdAt"), &c).unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn search_branches_drop_censored_tax_number_keep_locked_columns() {
        let all = allowed_search_branches(
            [
                ("code", "code"),
                ("name", "name"),
                ("taxNumber", "taxNumber"),
            ],
            &CensoredColumns::new(),
        );
        assert_eq!(all, ["code", "name", "taxNumber"]);
        let c = censored(&[("taxNumber", FieldVisibilityMode::Hidden)]);
        let kept = allowed_search_branches(
            [
                ("code", "code"),
                ("name", "name"),
                ("taxNumber", "taxNumber"),
            ],
            &c,
        );
        assert_eq!(kept, ["code", "name"]);
    }

    #[test]
    fn order_by_defaults_and_tiebreak() {
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_order_by(&mut qb, None, None);
        assert_eq!(qb.sql(), " ORDER BY c.name ASC, c.id ASC");
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_order_by(&mut qb, Some(SortBy::CreatedAt), Some(SortDir::Desc));
        assert_eq!(qb.sql(), r#" ORDER BY c."createdAt" DESC, c.id ASC"#);
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_order_by(&mut qb, Some(SortBy::Code), Some(SortDir::Asc));
        assert_eq!(qb.sql(), " ORDER BY c.code ASC, c.id ASC");
    }

    fn where_with(
        has_stores: Option<bool>,
        search: Option<&str>,
        branches: &[&'static str],
    ) -> String {
        let w = Where {
            company_id: 1,
            is_active: None,
            type_ids: None,
            group_ids: None,
            channel_ids: None,
            sub_channel_ids: None,
            has_stores,
            search: search.map(str::to_owned),
            search_branches: branches.to_vec(),
        };
        let mut qb = QueryBuilder::<Postgres>::new("");
        w.push(&mut qb);
        qb.sql().to_owned()
    }

    #[test]
    fn has_stores_true_false_and_omitted() {
        assert!(where_with(Some(true), None, &[]).contains(" AND EXISTS (SELECT 1 FROM store"));
        assert!(
            where_with(Some(false), None, &[]).contains(" AND NOT EXISTS (SELECT 1 FROM store")
        );
        assert!(!where_with(None, None, &[]).contains("store"));
    }

    #[test]
    fn where_always_scopes_company_and_live_rows() {
        let sql = where_with(None, None, &[]);
        assert!(sql.starts_with(r#" WHERE c."companyId" = $1 AND c."deletedAt" IS NULL"#));
    }

    #[test]
    fn search_clause_uses_allowed_branches_only() {
        let sql = where_with(None, Some("x"), &["code", "name"]);
        assert!(sql.contains(r#"c."code" ILIKE"#) && sql.contains(r#"c."name" ILIKE"#));
        assert!(!sql.contains("taxNumber"));
        assert!(where_with(None, Some("x"), &[]).ends_with("AND FALSE"));
        assert!(!where_with(None, None, &["code"]).contains("ILIKE"));
    }

    #[test]
    fn code_taken_body() {
        let e = code_taken("CUST-001");
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }
}
