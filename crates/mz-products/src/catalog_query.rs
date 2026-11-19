//! Port `src/catalog-query/catalog-query.service.ts` (lapisan Drizzle Node): HANYA SELECT di
//! tabel `product` — id + urutan + total untuk `cf[...]`/`sortBy=cf:<key>`, dan ringkasan nilai
//! custom field. `key` selalu parameter, tidak pernah digabung ke SQL.

use serde_json::{Map, Value};
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::custom_fields_validator::sort_cast;
use mz_core::ApiResult;

/// `CUSTOM_FIELD_SUMMARY_MAX_VALUES`.
pub const CUSTOM_FIELD_SUMMARY_MAX_VALUES: usize = 500;

/// `CustomFieldContainment` — nilai sudah dicor (`cast_filter_value`).
#[derive(Debug, Clone, PartialEq)]
pub struct Containment {
    pub key: String,
    pub value: Value,
}

/// `ProductIdsSort`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdsSort {
    /// `sku`/`name`/`createdAt`/`updatedAt`.
    Column { column: &'static str, desc: bool },
    CustomField {
        key: String,
        field_type: String,
        desc: bool,
    },
}

/// Kolom sort biasa → nama kolom SQL (literal).
pub fn regular_sort_column(sort_by: Option<&str>) -> &'static str {
    match sort_by {
        Some("sku") => "sku",
        Some("createdAt") => r#""createdAt""#,
        Some("updatedAt") => r#""updatedAt""#,
        _ => "name",
    }
}

fn push_scope(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    candidate_ids: &[i32],
    filters: &[Containment],
) {
    qb.push(r#" WHERE "companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND "deletedAt" IS NULL AND id = ANY("#)
        .push_bind(candidate_ids.to_vec())
        .push(")");
    if !filters.is_empty() {
        let mut fragment = Map::new();
        for f in filters {
            fragment.insert(f.key.clone(), f.value.clone());
        }
        qb.push(r#" AND "customFields" @> "#)
            .push_bind(Value::Object(fragment).to_string())
            .push("::jsonb");
    }
}

/// `listProductIds(input)`: halaman id dalam urutan akhir + total.
pub async fn list_product_ids<'c>(
    exec: impl PgExecutor<'c> + Copy,
    company_id: i32,
    candidate_ids: &[i32],
    filters: &[Containment],
    sort: &IdsSort,
    skip: i64,
    take: i64,
) -> ApiResult<(Vec<i32>, i64)> {
    if candidate_ids.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT id FROM product");
    push_scope(&mut qb, company_id, candidate_ids, filters);
    qb.push(" ORDER BY ");
    match sort {
        IdsSort::Column { column, desc } => {
            qb.push(*column).push(if *desc { " DESC" } else { " ASC" });
        }
        IdsSort::CustomField {
            key,
            field_type,
            desc,
        } => {
            qb.push(r#"("customFields" ->> "#)
                .push_bind(key.clone())
                .push(")");
            match sort_cast(field_type) {
                Some("numeric") => {
                    qb.push("::numeric");
                }
                Some("boolean") => {
                    qb.push("::boolean");
                }
                _ => {}
            }
            qb.push(if *desc { " DESC" } else { " ASC" });
        }
    }
    qb.push(", id ASC LIMIT ")
        .push_bind(take)
        .push(" OFFSET ")
        .push_bind(skip);
    let ids: Vec<i32> = qb.build_query_scalar().fetch_all(exec).await?;
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM product");
    push_scope(&mut cq, company_id, candidate_ids, filters);
    let total: i64 = cq.build_query_scalar().fetch_one(exec).await?;
    Ok((ids, total))
}

/// `customFieldSummary(companyId, key, type)`: `(nilai teks, jumlah)` urut jumlah turun lalu nilai.
/// `multiselect` dihitung per elemen.
pub async fn custom_field_summary<'c>(
    exec: impl PgExecutor<'c>,
    company_id: i32,
    key: &str,
    field_type: &str,
) -> ApiResult<Vec<(Option<String>, i64)>> {
    let value_expr = if field_type == "multiselect" {
        r#"jsonb_array_elements_text("customFields" -> $2)"#
    } else {
        r#"("customFields" ->> $2)"#
    };
    let sql = format!(
        r#"SELECT {value_expr} AS "value", count(*) AS "count" FROM product
           WHERE "companyId" = $1 AND "deletedAt" IS NULL AND "customFields" ? $2
           GROUP BY "value" ORDER BY "count" DESC, "value" ASC LIMIT {CUSTOM_FIELD_SUMMARY_MAX_VALUES}"#
    );
    Ok(sqlx::query_as(&sql)
        .bind(company_id)
        .bind(key)
        .fetch_all(exec)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // catalog-query.readonly.spec.ts — hanya SELECT.
    #[test]
    fn hanya_select() {
        let src = include_str!("catalog_query.rs");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        for kw in ["INSERT ", "UPDATE ", "DELETE ", "TRUNCATE "] {
            assert!(!body.contains(kw), "{kw} di catalog_query");
        }
        assert_eq!(regular_sort_column(Some("sku")), "sku");
        assert_eq!(regular_sort_column(None), "name");
    }
}
