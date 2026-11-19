//! Port `src/orders/order-export.generator.ts` + `OrdersService.exportOrders` (§9.1 F9, FB10,
//! specs/003 R10): `level=order` (header + kolom custom field non-sensitif + kolom delivery) dan
//! `level=item` (satu baris per item). xlsx dibangun di memori (plafon `XLSX_EXPORT_MAX_ROWS`), csv
//! di-stream per batch (plafon `CSV_EXPORT_MAX_ROWS`). Tanpa kuota export bulanan.

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDate, NaiveDateTime};
use mz_core::company::require_company_id;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use mz_store_visits::export::{
    CSV_EXPORT_MAX_ROWS, Cell, XLSX_EXPORT_MAX_ROWS, build_xlsx, csv_header_line, csv_line,
    format_date_tz,
};
use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::registry::FieldVisibilityMode;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::common::{bad_request, date_str, internal};
use crate::dto::ListQuery;
use crate::pricing::{dstr, sub};
use crate::service::{Where, build_where, order_by_sql, push_where};

const BATCH_SIZE: i64 = 500;
const LINE_ORDER_BATCH_SIZE: i64 = 50;

/// Baris export level order (`ORDER_EXPORT_SELECT`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "orderNumber")]
    pub order_number: String,
    pub status: String,
    pub source: String,
    pub ot_code: Option<String>,
    pub ot_name: Option<String>,
    pub c_code: Option<String>,
    pub c_name: Option<String>,
    pub s_code: Option<String>,
    pub s_name: Option<String>,
    pub owner_name: Option<String>,
    #[sqlx(rename = "orderDate")]
    pub order_date: NaiveDate,
    #[sqlx(rename = "requestedDeliveryDate")]
    pub requested_delivery_date: Option<NaiveDate>,
    #[sqlx(rename = "dueDate")]
    pub due_date: Option<NaiveDate>,
    pub subtotal: Decimal,
    #[sqlx(rename = "discountTotal")]
    pub discount_total: Decimal,
    #[sqlx(rename = "taxTotal")]
    pub tax_total: Decimal,
    #[sqlx(rename = "grandTotal")]
    pub grand_total: Decimal,
    #[sqlx(rename = "itemCount")]
    pub item_count: i32,
    #[sqlx(rename = "customFields")]
    pub custom_fields: Value,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "deliveredAt")]
    pub delivered_at: Option<NaiveDateTime>,
    #[sqlx(rename = "finalSubtotal")]
    pub final_subtotal: Option<Decimal>,
    #[sqlx(rename = "finalDiscountTotal")]
    pub final_discount_total: Option<Decimal>,
    #[sqlx(rename = "finalTaxTotal")]
    pub final_tax_total: Option<Decimal>,
    #[sqlx(rename = "finalGrandTotal")]
    pub final_grand_total: Option<Decimal>,
}

const EXPORT_SELECT: &str = r#"SELECT o.id, o.uuid::text AS uuid, o."orderNumber", o.status::text AS status,
    o.source::text AS source, ot.code AS ot_code, ot.name AS ot_name, c.code AS c_code, c.name AS c_name,
    s.code AS s_code, s.name AS s_name, ow.name AS owner_name, o."orderDate", o."requestedDeliveryDate", o."dueDate",
    o.subtotal, o."discountTotal", o."taxTotal", o."grandTotal", o."itemCount", o."customFields", o."createdAt",
    o."deliveredAt", o."finalSubtotal", o."finalDiscountTotal", o."finalTaxTotal", o."finalGrandTotal"
  FROM "order" o
  LEFT JOIN "orderType" ot ON ot.id = o."orderTypeId"
  LEFT JOIN customer c ON c.id = o."customerId"
  LEFT JOIN store s ON s.id = o."storeId"
  LEFT JOIN users ow ON ow.id = o."ownerId""#;

/// Satu baris item untuk `level=item`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LineRow {
    #[sqlx(rename = "orderId")]
    pub order_id: i32,
    #[sqlx(rename = "lineNo")]
    pub line_no: i32,
    #[sqlx(rename = "productSku")]
    pub product_sku: String,
    #[sqlx(rename = "productName")]
    pub product_name: String,
    #[sqlx(rename = "uomCode")]
    pub uom_code: String,
    pub qty: Decimal,
    #[sqlx(rename = "unitPrice")]
    pub unit_price: Decimal,
    #[sqlx(rename = "lineTotal")]
    pub line_total: Decimal,
    #[sqlx(rename = "deliveredQty")]
    pub delivered_qty: Option<Decimal>,
    #[sqlx(rename = "finalLineTotal")]
    pub final_line_total: Option<Decimal>,
    #[sqlx(rename = "deliveryNote")]
    pub delivery_note: Option<String>,
    pub r_code: Option<String>,
    pub r_name: Option<String>,
}

fn s(v: impl Into<String>) -> Cell {
    Cell::Text(v.into())
}

fn money(v: Option<&Decimal>) -> Cell {
    v.map_or_else(Cell::blank, |d| s(dstr(*d)))
}

fn ref_label(code: Option<&String>, name: Option<&String>) -> String {
    match (code, name) {
        (Some(c), Some(n)) => format!("{c} — {n}"),
        _ => String::new(),
    }
}

fn date_only(d: Option<NaiveDate>) -> String {
    d.map(date_str).unwrap_or_default()
}

fn date_tz(d: Option<NaiveDateTime>, tz: &str) -> String {
    format_date_tz(d.map(JsDate), tz)
}

/// `formatCustomFieldValue`.
pub fn format_custom_field_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(b)) => if *b { "Yes" } else { "No" }.to_owned(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                Value::Null => "null".into(),
                other => mz_visit_forms::pdf::js_string(other),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Object(_)) => v.map(Value::to_string).unwrap_or_default(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => mz_visit_forms::pdf::js_string(other),
    }
}

/// `ExportColumn`: judul, kunci registry (+ kunci lain yang ikut menyensor), sel.
pub struct Column<R: 'static> {
    pub header: &'static str,
    pub keys: &'static [&'static str],
    pub cell: fn(&R, &str) -> Cell,
}

fn is_hidden<R>(c: &Column<R>, censored: &CensoredColumns) -> bool {
    c.keys
        .iter()
        .any(|k| censored.get(*k) == Some(&FieldVisibilityMode::Hidden))
}

fn is_censored<R>(c: &Column<R>, censored: &CensoredColumns) -> bool {
    c.keys.iter().any(|k| censored.contains_key(*k))
}

/// `columnsFor`: `hidden` membuang kolom, mode lain mengosongkan sel.
fn columns_for<'a, R>(cols: &'a [Column<R>], censored: &CensoredColumns) -> Vec<&'a Column<R>> {
    cols.iter().filter(|c| !is_hidden(c, censored)).collect()
}

fn to_cells<R>(cols: &[&Column<R>], row: &R, tz: &str, censored: &CensoredColumns) -> Vec<Cell> {
    cols.iter()
        .map(|c| {
            if is_censored(c, censored) {
                Cell::blank()
            } else {
                (c.cell)(row, tz)
            }
        })
        .collect()
}

static FIXED: &[Column<ExportRow>] = &[
    Column {
        header: "id",
        keys: &[],
        cell: |r, _| s(r.uuid.clone()),
    },
    Column {
        header: "orderNumber",
        keys: &["orderNumber"],
        cell: |r, _| s(r.order_number.clone()),
    },
    Column {
        header: "status",
        keys: &["status"],
        cell: |r, _| s(r.status.clone()),
    },
    Column {
        header: "source",
        keys: &[],
        cell: |r, _| s(r.source.clone()),
    },
    Column {
        header: "orderType",
        keys: &[],
        cell: |r, _| s(ref_label(r.ot_code.as_ref(), r.ot_name.as_ref())),
    },
    Column {
        header: "customer",
        keys: &[],
        cell: |r, _| s(ref_label(r.c_code.as_ref(), r.c_name.as_ref())),
    },
    Column {
        header: "store",
        keys: &[],
        cell: |r, _| s(ref_label(r.s_code.as_ref(), r.s_name.as_ref())),
    },
    Column {
        header: "owner",
        keys: &[],
        cell: |r, _| s(r.owner_name.clone().unwrap_or_default()),
    },
    Column {
        header: "orderDate",
        keys: &[],
        cell: |r, _| s(date_str(r.order_date)),
    },
    Column {
        header: "requestedDeliveryDate",
        keys: &[],
        cell: |r, _| s(date_only(r.requested_delivery_date)),
    },
    Column {
        header: "dueDate",
        keys: &[],
        cell: |r, _| s(date_only(r.due_date)),
    },
    Column {
        header: "subtotal",
        keys: &["subtotal"],
        cell: |r, _| s(dstr(r.subtotal)),
    },
    Column {
        header: "discountTotal",
        keys: &["discountTotal"],
        cell: |r, _| s(dstr(r.discount_total)),
    },
    Column {
        header: "taxTotal",
        keys: &["taxTotal"],
        cell: |r, _| s(dstr(r.tax_total)),
    },
    Column {
        header: "grandTotal",
        keys: &["grandTotal"],
        cell: |r, _| s(dstr(r.grand_total)),
    },
    Column {
        header: "itemCount",
        keys: &[],
        cell: |r, _| Cell::Num(f64::from(r.item_count)),
    },
    Column {
        header: "createdAt",
        keys: &[],
        cell: |r, tz| s(date_tz(Some(r.created_at), tz)),
    },
];

static DELIVERY: &[Column<ExportRow>] = &[
    Column {
        header: "deliveredAt",
        keys: &[],
        cell: |r, tz| s(date_tz(r.delivered_at, tz)),
    },
    Column {
        header: "finalSubtotal",
        keys: &["finalSubtotal"],
        cell: |r, _| money(r.final_subtotal.as_ref()),
    },
    Column {
        header: "finalDiscountTotal",
        keys: &["finalDiscountTotal"],
        cell: |r, _| money(r.final_discount_total.as_ref()),
    },
    Column {
        header: "finalTaxTotal",
        keys: &["finalTaxTotal"],
        cell: |r, _| money(r.final_tax_total.as_ref()),
    },
    Column {
        header: "finalGrandTotal",
        keys: &["finalGrandTotal"],
        cell: |r, _| money(r.final_grand_total.as_ref()),
    },
    Column {
        header: "shortDeliveredValue",
        keys: &["finalGrandTotal", "grandTotal"],
        cell: |r, _| {
            r.final_grand_total
                .map_or_else(Cell::blank, |f| s(dstr(sub(r.grand_total, f))))
        },
    },
];

/// Satu baris `level=item` (order + item).
pub struct LineExport {
    pub order: ExportRow,
    pub item: LineRow,
}

static LINE_COLUMNS: &[Column<LineExport>] = &[
    Column {
        header: "orderNumber",
        keys: &[],
        cell: |r, _| s(r.order.order_number.clone()),
    },
    Column {
        header: "status",
        keys: &[],
        cell: |r, _| s(r.order.status.clone()),
    },
    Column {
        header: "orderDate",
        keys: &[],
        cell: |r, _| s(date_str(r.order.order_date)),
    },
    Column {
        header: "customer",
        keys: &[],
        cell: |r, _| s(ref_label(r.order.c_code.as_ref(), r.order.c_name.as_ref())),
    },
    Column {
        header: "store",
        keys: &[],
        cell: |r, _| s(ref_label(r.order.s_code.as_ref(), r.order.s_name.as_ref())),
    },
    Column {
        header: "owner",
        keys: &[],
        cell: |r, _| s(r.order.owner_name.clone().unwrap_or_default()),
    },
    Column {
        header: "lineNo",
        keys: &[],
        cell: |r, _| Cell::Num(f64::from(r.item.line_no)),
    },
    Column {
        header: "productSku",
        keys: &[],
        cell: |r, _| s(r.item.product_sku.clone()),
    },
    Column {
        header: "productName",
        keys: &[],
        cell: |r, _| s(r.item.product_name.clone()),
    },
    Column {
        header: "uomCode",
        keys: &[],
        cell: |r, _| s(r.item.uom_code.clone()),
    },
    Column {
        header: "qty",
        keys: &[],
        cell: |r, _| s(dstr(r.item.qty)),
    },
    Column {
        header: "deliveredQty",
        keys: &[],
        cell: |r, _| money(r.item.delivered_qty.as_ref()),
    },
    Column {
        header: "shortQty",
        keys: &[],
        cell: |r, _| {
            r.item
                .delivered_qty
                .map_or_else(Cell::blank, |d| s(dstr(sub(r.item.qty, d))))
        },
    },
    Column {
        header: "unitPrice",
        keys: &["unitPrice"],
        cell: |r, _| s(dstr(r.item.unit_price)),
    },
    Column {
        header: "lineTotal",
        keys: &["lineTotal"],
        cell: |r, _| s(dstr(r.item.line_total)),
    },
    Column {
        header: "finalLineTotal",
        keys: &["finalLineTotal"],
        cell: |r, _| money(r.item.final_line_total.as_ref()),
    },
    Column {
        header: "deliveryReason",
        keys: &[],
        cell: |r, _| s(ref_label(r.item.r_code.as_ref(), r.item.r_name.as_ref())),
    },
    Column {
        header: "deliveryNote",
        keys: &[],
        cell: |r, _| s(r.item.delivery_note.clone().unwrap_or_default()),
    },
    Column {
        header: "deliveredAt",
        keys: &[],
        cell: |r, tz| s(date_tz(r.order.delivered_at, tz)),
    },
];

/// Judul + sel satu baris level order.
struct HeaderColumns<'a> {
    fixed: Vec<&'a Column<ExportRow>>,
    delivery: Vec<&'a Column<ExportRow>>,
}

fn header_titles(cols: &HeaderColumns<'_>, cf: &[(String, String)]) -> Vec<String> {
    cols.fixed
        .iter()
        .map(|c| c.header.to_owned())
        .chain(cf.iter().map(|(_, label)| label.clone()))
        .chain(cols.delivery.iter().map(|c| c.header.to_owned()))
        .collect()
}

fn row_cells(
    cols: &HeaderColumns<'_>,
    row: &ExportRow,
    tz: &str,
    censored: &CensoredColumns,
    cf: &[(String, String)],
) -> Vec<Cell> {
    let mut cells = to_cells(&cols.fixed, row, tz, censored);
    cells.extend(
        cf.iter()
            .map(|(k, _)| s(format_custom_field_value(row.custom_fields.get(k)))),
    );
    cells.extend(to_cells(&cols.delivery, row, tz, censored));
    cells
}

async fn order_batch(
    db: &PgPool,
    w: &Where,
    order_by: &str,
    off: i64,
    limit: Option<i64>,
) -> ApiResult<Vec<ExportRow>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(EXPORT_SELECT);
    push_where(&mut qb, w)?;
    qb.push(order_by);
    if let Some(l) = limit {
        qb.push(" LIMIT ")
            .push_bind(l)
            .push(" OFFSET ")
            .push_bind(off);
    }
    Ok(qb.build_query_as().fetch_all(db).await?)
}

async fn lines_of(db: &PgPool, orders: Vec<ExportRow>) -> ApiResult<Vec<LineExport>> {
    let ids: Vec<i32> = orders.iter().map(|o| o.id).collect();
    let items: Vec<LineRow> = sqlx::query_as(
        r#"SELECT i."orderId", i."lineNo", i."productSku", i."productName", i."uomCode", i.qty, i."unitPrice",
                  i."lineTotal", i."deliveredQty", i."finalLineTotal", i."deliveryNote", r.code AS r_code, r.name AS r_name
             FROM "orderItem" i LEFT JOIN "orderReason" r ON r.id = i."deliveryReasonId"
            WHERE i."orderId" = ANY($1) ORDER BY i."lineNo" ASC"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut out = Vec::new();
    for o in orders {
        for it in items.iter().filter(|i| i.order_id == o.id) {
            out.push(LineExport {
                order: o.clone(),
                item: it.clone(),
            });
        }
    }
    Ok(out)
}

fn file(content_type: &str, filename: &str, body: Body) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// Express menambahkan `charset=utf-8` pada `text/csv`.
const CSV: &str = "text/csv; charset=utf-8";

/// Node: src/orders/orders.service.ts:1199 `exportOrders`.
pub async fn export_orders(
    state: &AppState,
    user_id: i32,
    q: &ListQuery,
    censored: &CensoredColumns,
    censored_items: &CensoredColumns,
) -> ApiResult<Response> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let tz = crate::common::company_time_zone(&state.db, company_id).await?;
    let w = build_where(state, company_id, &q.filter).await?;
    let order_by = order_by_sql(&q.sort_by, &q.sort_dir);
    let item_level = q.level == "item";
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new(if item_level {
        r#"SELECT COUNT(*) FROM "orderItem" i JOIN "order" o ON o.id = i."orderId""#
    } else {
        r#"SELECT COUNT(*) FROM "order" o"#
    });
    push_where(&mut cq, &w)?;
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let xlsx = q.format != "csv";
    if xlsx && total > XLSX_EXPORT_MAX_ROWS {
        return Err(bad_request(
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "xlsx export is limited to {XLSX_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters or use format=csv"
            ),
        ));
    }
    if total > CSV_EXPORT_MAX_ROWS {
        return Err(bad_request(
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "Export is limited to {CSV_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters"
            ),
        ));
    }

    if item_level {
        let cols: Vec<&'static Column<LineExport>> = columns_for(LINE_COLUMNS, censored_items);
        let headers: Vec<&str> = cols.iter().map(|c| c.header).collect();
        if xlsx {
            let orders = order_batch(&state.db, &w, &order_by, 0, None).await?;
            let lines = lines_of(&state.db, orders).await?;
            let rows: Vec<Vec<Cell>> = lines
                .iter()
                .map(|l| to_cells(&cols, l, &tz, censored_items))
                .collect();
            let bytes =
                build_xlsx("Order Lines", &headers, &rows).map_err(|e| internal(&e.to_string()))?;
            return Ok(file(XLSX, "order-lines-export.xlsx", Body::from(bytes)));
        }
        let header = csv_header_line(&headers);
        let censored_items = censored_items.clone();
        let db = state.db.clone();
        let stream = futures::stream::unfold(Some(None::<i64>), move |step| {
            let (db, w, order_by, tz, cols, censored_items, header) = (
                db.clone(),
                w.clone(),
                order_by.clone(),
                tz.clone(),
                cols.clone(),
                censored_items.clone(),
                header.clone(),
            );
            async move {
                let step = step?;
                let Some(off) = step else {
                    return Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from(header)),
                        Some(Some(0)),
                    ));
                };
                let orders =
                    match order_batch(&db, &w, &order_by, off, Some(LINE_ORDER_BATCH_SIZE)).await {
                        Ok(o) => o,
                        Err(e) => {
                            tracing::error!("orders csv export batch failed: {e:?}");
                            return Some((Err(std::io::Error::other("export failed")), None));
                        }
                    };
                if orders.is_empty() {
                    return None;
                }
                let n = i64::try_from(orders.len()).unwrap_or(0);
                let lines = match lines_of(&db, orders).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("orders csv export lines failed: {e:?}");
                        return Some((Err(std::io::Error::other("export failed")), None));
                    }
                };
                let chunk: String = lines
                    .iter()
                    .map(|l| csv_line(&to_cells(&cols, l, &tz, &censored_items)))
                    .collect();
                let next = (n >= LINE_ORDER_BATCH_SIZE).then_some(Some(off + n));
                Some((Ok(Bytes::from(chunk)), next))
            }
        });
        return Ok(file(
            CSV,
            "order-lines-export.csv",
            // `.fuse()`: CompressionLayer bisa mem-poll body lagi sesudah stream selesai; `unfold` panic bila itu terjadi.
            Body::from_stream(futures::StreamExt::fuse(stream)),
        ));
    }

    let cf = crate::custom_fields::export_columns(&state.db, company_id).await?;
    let cols = HeaderColumns {
        fixed: columns_for(FIXED, censored),
        delivery: columns_for(DELIVERY, censored),
    };
    let titles = header_titles(&cols, &cf);
    let headers: Vec<&str> = titles.iter().map(String::as_str).collect();
    if xlsx {
        let rows = order_batch(&state.db, &w, &order_by, 0, None).await?;
        let cells: Vec<Vec<Cell>> = rows
            .iter()
            .map(|r| row_cells(&cols, r, &tz, censored, &cf))
            .collect();
        let bytes = build_xlsx("Orders", &headers, &cells).map_err(|e| internal(&e.to_string()))?;
        return Ok(file(XLSX, "orders-export.xlsx", Body::from(bytes)));
    }
    let header = csv_header_line(&headers);
    let censored = censored.clone();
    let db = state.db.clone();
    let stream = futures::stream::unfold(Some(None::<i64>), move |step| {
        let (db, w, order_by, tz, censored, cf, header) = (
            db.clone(),
            w.clone(),
            order_by.clone(),
            tz.clone(),
            censored.clone(),
            cf.clone(),
            header.clone(),
        );
        async move {
            let step = step?;
            let Some(off) = step else {
                return Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(header)),
                    Some(Some(0)),
                ));
            };
            let rows = match order_batch(&db, &w, &order_by, off, Some(BATCH_SIZE)).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("orders csv export batch failed: {e:?}");
                    return Some((Err(std::io::Error::other("export failed")), None));
                }
            };
            if rows.is_empty() {
                return None;
            }
            let cols = HeaderColumns {
                fixed: columns_for(FIXED, &censored),
                delivery: columns_for(DELIVERY, &censored),
            };
            let n = i64::try_from(rows.len()).unwrap_or(0);
            let chunk: String = rows
                .iter()
                .map(|r| csv_line(&row_cells(&cols, r, &tz, &censored, &cf)))
                .collect();
            let next = (n >= BATCH_SIZE).then_some(Some(off + n));
            Some((Ok(Bytes::from(chunk)), next))
        }
    });
    // `.fuse()`: CompressionLayer bisa mem-poll body lagi sesudah stream selesai; `unfold` panic bila itu terjadi.
    Ok(file(
        CSV,
        "orders-export.csv",
        Body::from_stream(futures::StreamExt::fuse(stream)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row() -> ExportRow {
        ExportRow {
            id: 1,
            uuid: "u".into(),
            order_number: "ORD-202601-00001".into(),
            status: "delivered".into(),
            source: "web".into(),
            ot_code: Some("REG".into()),
            ot_name: Some("Regular".into()),
            c_code: Some("C1".into()),
            c_name: Some("PT A".into()),
            s_code: None,
            s_name: None,
            owner_name: Some("Budi".into()),
            order_date: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            requested_delivery_date: None,
            due_date: None,
            subtotal: Decimal::new(10000, 0),
            discount_total: Decimal::ZERO,
            tax_total: Decimal::new(1100, 0),
            grand_total: Decimal::new(11100, 0),
            item_count: 1,
            custom_fields: json!({"po": "PO-1", "flags": ["a", "b"], "ok": true}),
            created_at: NaiveDateTime::default(),
            delivered_at: None,
            final_subtotal: Some(Decimal::new(5000, 0)),
            final_discount_total: Some(Decimal::ZERO),
            final_tax_total: Some(Decimal::new(550, 0)),
            final_grand_total: Some(Decimal::new(555000, 2)),
        }
    }

    // Node: src/orders/order-export.generator.spec.ts (kolom, sensor, nilai)
    #[test]
    fn header_and_censoring() {
        let mut censored = CensoredColumns::new();
        censored.insert("grandTotal".into(), FieldVisibilityMode::Hidden);
        censored.insert("subtotal".into(), FieldVisibilityMode::Masked);
        let cols = HeaderColumns {
            fixed: columns_for(FIXED, &censored),
            delivery: columns_for(DELIVERY, &censored),
        };
        let cf = vec![
            ("po".to_owned(), "PO Number".to_owned()),
            ("flags".to_owned(), "Flags".to_owned()),
        ];
        let titles = header_titles(&cols, &cf);
        assert!(!titles.contains(&"grandTotal".to_owned()));
        assert!(!titles.contains(&"shortDeliveredValue".to_owned()));
        assert!(titles.contains(&"PO Number".to_owned()));
        let cells = row_cells(&cols, &row(), "Asia/Jakarta", &censored, &cf);
        let idx = titles.iter().position(|t| t == "subtotal").unwrap();
        assert_eq!(cells[idx], Cell::blank());
        let po = titles.iter().position(|t| t == "PO Number").unwrap();
        assert_eq!(cells[po], s("PO-1"));
        assert_eq!(cells[po + 1], s("a, b"));
    }

    #[test]
    fn short_delivered_value() {
        let cols = HeaderColumns {
            fixed: columns_for(FIXED, &CensoredColumns::new()),
            delivery: columns_for(DELIVERY, &CensoredColumns::new()),
        };
        let titles = header_titles(&cols, &[]);
        let cells = row_cells(&cols, &row(), "UTC", &CensoredColumns::new(), &[]);
        let i = titles
            .iter()
            .position(|t| t == "shortDeliveredValue")
            .unwrap();
        assert_eq!(cells[i], s("5550"));
        assert_eq!(format_custom_field_value(Some(&json!(false))), "No");
        assert_eq!(format_custom_field_value(Some(&json!(12.5))), "12.5");
    }
}
