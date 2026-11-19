//! Port `products/product-import-writer.ts`: tulis SATU batch yang sudah tervalidasi di dalam
//! transaksi milik pemanggil (dryRun = pemanggil rollback). Produk baru → satu INSERT massal;
//! produk yang sudah ada (per `sku`) → UPDATE per baris + MERGE anak (UOM/barcode tidak pernah
//! dihapus — lihat kepala berkas Node).

use std::collections::{HashMap, HashSet};

use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::time::now_naive;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use super::row::{ResolvedImportRow, ResolvedImportUom};
use crate::common::dec_str;
use crate::util::compare_decimal_strings;

/// `WrittenProduct`.
#[derive(Debug, Clone)]
pub struct WrittenProduct {
    pub product_id: i32,
    pub product_uuid: String,
    pub row: ResolvedImportRow,
}

/// `BatchWriteResult`.
#[derive(Debug, Clone, Default)]
pub struct BatchWriteResult {
    pub written: Vec<WrittenProduct>,
    pub created_count: usize,
    pub updated_count: usize,
}

#[derive(sqlx::FromRow)]
struct IdRow {
    id: i32,
    uuid: String,
    sku: String,
}

#[derive(sqlx::FromRow)]
struct UomRow {
    id: i32,
    #[sqlx(rename = "productId")]
    product_id: i32,
    #[sqlx(rename = "uomId")]
    uom_id: i32,
    operator: String,
    factor: rust_decimal::Decimal,
}

/// `data` Prisma `createMany` untuk audit (kolom seperti argumen Node).
fn create_data(company_id: i32, created_by: i32, r: &ResolvedImportRow) -> Row {
    Row::new()
        .with("companyId", company_id)
        .with("sku", r.sku.clone())
        .with("name", r.name.clone())
        .with("description", r.description.clone())
        .with("categoryId", r.category_id)
        .with("subCategoryId", r.sub_category_id)
        .with("brandId", r.brand_id)
        .with("principalId", r.principal_id)
        .with("baseUomId", r.base_uom_id)
        .with("taxRateId", r.tax_rate_id)
        .with("weight", r.weight.clone())
        .with("volume", r.volume.clone())
        .with("isOrderable", r.is_orderable)
        .with("isActive", r.is_active)
        .with("customFields", Value::Object(r.custom_fields.clone()))
        .with("createdById", created_by)
}

async fn find_by_skus(
    conn: &mut PgConnection,
    company_id: i32,
    skus: &[String],
) -> sqlx::Result<Vec<IdRow>> {
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, sku FROM product
           WHERE "companyId" = $1 AND sku = ANY($2) AND "deletedAt" IS NULL ORDER BY id"#,
    )
    .bind(company_id)
    .bind(skus)
    .fetch_all(conn)
    .await
}

/// `writeImportBatch(tx, companyId, createdById, rows)`.
pub async fn write_import_batch(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    created_by: i32,
    rows: &[ResolvedImportRow],
) -> sqlx::Result<BatchWriteResult> {
    let skus: Vec<String> = rows.iter().map(|r| r.sku.clone()).collect();
    let existing = find_by_skus(conn, company_id, &skus).await?;
    let existing_by_sku: HashMap<&str, &IdRow> =
        existing.iter().map(|p| (p.sku.as_str(), p)).collect();
    let to_create: Vec<&ResolvedImportRow> = rows
        .iter()
        .filter(|r| !existing_by_sku.contains_key(r.sku.as_str()))
        .collect();
    let to_update: Vec<&ResolvedImportRow> = rows
        .iter()
        .filter(|r| existing_by_sku.contains_key(r.sku.as_str()))
        .collect();

    if !to_create.is_empty() {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            r#"INSERT INTO product ("companyId", sku, name, description, "categoryId", "subCategoryId",
               "brandId", "principalId", "baseUomId", "taxRateId", weight, volume, "isOrderable",
               "isActive", "customFields", "createdById") "#,
        );
        qb.push_values(to_create.iter(), |mut b, r| {
            b.push_bind(company_id)
                .push_bind(r.sku.clone())
                .push_bind(r.name.clone())
                .push_bind(r.description.clone())
                .push_bind(r.category_id)
                .push_bind(r.sub_category_id)
                .push_bind(r.brand_id)
                .push_bind(r.principal_id)
                .push_bind(r.base_uom_id)
                .push_bind(r.tax_rate_id)
                .push_bind(r.weight.clone())
                .push_unseparated("::numeric")
                .push_bind(r.volume.clone())
                .push_unseparated("::numeric")
                .push_bind(r.is_orderable)
                .push_bind(r.is_active)
                .push_bind(Json(Value::Object(r.custom_fields.clone())))
                .push_bind(created_by);
        });
        let affected = qb.build().execute(&mut *conn).await?.rows_affected();
        let data: Vec<Row> = to_create
            .iter()
            .map(|r| create_data(company_id, created_by, r))
            .collect();
        audit
            .record_create_many(Model::Product, &data, affected)
            .await;
    }

    let created_rows = if to_create.is_empty() {
        Vec::new()
    } else {
        let s: Vec<String> = to_create.iter().map(|r| r.sku.clone()).collect();
        find_by_skus(conn, company_id, &s).await?
    };
    let created_by_sku: HashMap<&str, &IdRow> =
        created_rows.iter().map(|p| (p.sku.as_str(), p)).collect();

    let now = now_naive();
    let mut created_written = Vec::with_capacity(to_create.len());
    let mut updated_written = Vec::with_capacity(to_update.len());
    for r in &to_create {
        let Some(c) = created_by_sku.get(r.sku.as_str()) else {
            return Err(sqlx::Error::Protocol(format!(
                "Data integrity: sku \"{}\" was created but not found back in the same transaction",
                r.sku
            )));
        };
        created_written.push(WrittenProduct {
            product_id: c.id,
            product_uuid: c.uuid.clone(),
            row: (*r).clone(),
        });
    }
    for r in &to_update {
        let Some(e) = existing_by_sku.get(r.sku.as_str()) else {
            continue;
        };
        let pre = audit.pre_read(Model::Product, Lookup::Id(e.id)).await;
        let (id, uuid): (i32, String) = sqlx::query_as(
            r#"UPDATE product SET name = $2, description = $3, "categoryId" = $4, "subCategoryId" = $5,
                 "brandId" = $6, "principalId" = $7, "baseUomId" = $8, "taxRateId" = $9,
                 weight = $10::numeric, volume = $11::numeric, "isOrderable" = $12, "isActive" = $13,
                 "customFields" = $14, "updatedAt" = $15
               WHERE id = $1 RETURNING id, uuid::text"#,
        )
        .bind(e.id)
        .bind(&r.name)
        .bind(&r.description)
        .bind(r.category_id)
        .bind(r.sub_category_id)
        .bind(r.brand_id)
        .bind(r.principal_id)
        .bind(r.base_uom_id)
        .bind(r.tax_rate_id)
        .bind(&r.weight)
        .bind(&r.volume)
        .bind(r.is_orderable)
        .bind(r.is_active)
        .bind(Json(Value::Object(r.custom_fields.clone())))
        .bind(now)
        .fetch_one(&mut *conn)
        .await?;
        if let Some(after) = audit
            .result_row(&mut *conn, Model::Product, Lookup::Id(id))
            .await
        {
            audit.record_update(Model::Product, pre, &after).await;
        }
        updated_written.push(WrittenProduct {
            product_id: id,
            product_uuid: uuid,
            row: (*r).clone(),
        });
    }

    write_children_for_created(conn, company_id, &created_written).await?;
    merge_children_for_updated(conn, company_id, &updated_written).await?;

    let created_count = to_create.len();
    let updated_count = to_update.len();
    let mut written = created_written;
    written.extend(updated_written);
    Ok(BatchWriteResult {
        written,
        created_count,
        updated_count,
    })
}

struct UomInsert<'a> {
    product_id: i32,
    uom: &'a ResolvedImportUom,
}

async fn insert_uoms(conn: &mut PgConnection, rows: &[UomInsert<'_>]) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "productUom" ("productId", "uomId", "conversionRate", operator, factor,
           "isBase", "isSalesUom", "isPurchaseUom") "#,
    );
    qb.push_values(rows.iter(), |mut b, r| {
        b.push_bind(r.product_id)
            .push_bind(r.uom.uom_id)
            .push_bind(r.uom.conversion_rate.clone())
            .push_unseparated("::numeric")
            .push_bind(r.uom.operator.clone())
            .push_unseparated(r#"::"UomOperator""#)
            .push_bind(r.uom.factor.clone())
            .push_unseparated("::numeric")
            .push_bind(r.uom.is_base)
            .push_bind(true)
            .push_bind(false);
    });
    qb.build().execute(conn).await?;
    Ok(())
}

async fn insert_barcodes(
    conn: &mut PgConnection,
    company_id: i32,
    rows: &[(i32, String)],
) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "productBarcode" ("companyId", "productId", "uomId", barcode) "#,
    );
    qb.push_values(rows.iter(), |mut b, (pid, code)| {
        b.push_bind(company_id)
            .push_bind(*pid)
            .push_bind(Option::<i32>::None)
            .push_bind(code.clone());
    });
    qb.build().execute(conn).await?;
    Ok(())
}

/// Produk baru: insert anak langsung.
async fn write_children_for_created(
    conn: &mut PgConnection,
    company_id: i32,
    created: &[WrittenProduct],
) -> sqlx::Result<()> {
    if created.is_empty() {
        return Ok(());
    }
    let uoms: Vec<UomInsert<'_>> = created
        .iter()
        .flat_map(|w| {
            w.row.uoms.iter().map(|u| UomInsert {
                product_id: w.product_id,
                uom: u,
            })
        })
        .collect();
    insert_uoms(conn, &uoms).await?;
    let barcodes: Vec<(i32, String)> = created
        .iter()
        .filter_map(|w| w.row.barcode.clone().map(|b| (w.product_id, b)))
        .collect();
    insert_barcodes(conn, company_id, &barcodes).await
}

/// Produk lama: spreadsheet = patch, tidak ada yang dihapus.
async fn merge_children_for_updated(
    conn: &mut PgConnection,
    company_id: i32,
    updated: &[WrittenProduct],
) -> sqlx::Result<()> {
    if updated.is_empty() {
        return Ok(());
    }
    let product_ids: Vec<i32> = updated.iter().map(|w| w.product_id).collect();
    let existing: Vec<UomRow> = sqlx::query_as(
        r#"SELECT id, "productId", "uomId", operator::text AS operator, factor
           FROM "productUom" WHERE "productId" = ANY($1) ORDER BY id"#,
    )
    .bind(&product_ids)
    .fetch_all(&mut *conn)
    .await?;
    let by_pair: HashMap<(i32, i32), &UomRow> = existing
        .iter()
        .map(|r| ((r.product_id, r.uom_id), r))
        .collect();
    sqlx::query(r#"UPDATE "productUom" SET "isBase" = false WHERE "productId" = ANY($1)"#)
        .bind(&product_ids)
        .execute(&mut *conn)
        .await?;
    let mut to_create: Vec<UomInsert<'_>> = Vec::new();
    for w in updated {
        for u in &w.row.uoms {
            let Some(ex) = by_pair.get(&(w.product_id, u.uom_id)) else {
                to_create.push(UomInsert {
                    product_id: w.product_id,
                    uom: u,
                });
                continue;
            };
            let changed = ex.operator != u.operator
                || compare_decimal_strings(&dec_str(&ex.factor), &u.factor).is_ne();
            if !u.is_base && !changed {
                continue;
            }
            sqlx::query(
                r#"UPDATE "productUom" SET "conversionRate" = $2::numeric, operator = $3::"UomOperator",
                     factor = $4::numeric, "isBase" = $5 WHERE id = $1"#,
            )
            .bind(ex.id)
            .bind(&u.conversion_rate)
            .bind(&u.operator)
            .bind(&u.factor)
            .bind(u.is_base)
            .execute(&mut *conn)
            .await?;
        }
    }
    insert_uoms(conn, &to_create).await?;

    let with_barcode: Vec<&WrittenProduct> =
        updated.iter().filter(|w| w.row.barcode.is_some()).collect();
    if with_barcode.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = with_barcode.iter().map(|w| w.product_id).collect();
    let pairs: HashSet<(i32, String)> = sqlx::query_as::<_, (i32, String)>(
        r#"SELECT "productId", barcode FROM "productBarcode" WHERE "productId" = ANY($1)"#,
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let to_insert: Vec<(i32, String)> = with_barcode
        .iter()
        .filter_map(|w| w.row.barcode.clone().map(|b| (w.product_id, b)))
        .filter(|p| !pairs.contains(p))
        .collect();
    insert_barcodes(conn, company_id, &to_insert).await
}

/// `describeBatchError(e)`.
pub fn describe_batch_error(e: &sqlx::Error) -> (&'static str, String) {
    if let Some(c) = crate::common::unique_constraint(e) {
        if c.contains("barcode") {
            return (
                "BARCODE_TAKEN",
                "One or more barcodes in this batch collided with an existing one".into(),
            );
        }
        return (
            "PRODUCT_SKU_TAKEN",
            "A unique constraint was violated while writing this batch".into(),
        );
    }
    ("IMPORT_BATCH_FAILED", e.to_string())
}
