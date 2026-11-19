//! `findAll` (`GET /invoices`) dan `findOne` (`GET /invoices/:uuid`).

use mz_core::qs::{self, QVal};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::rows::{InvoiceWithSub, select_cols};

const INVOICE_STATUSES: &[&str] = &["UNPAID", "PAID", "CANCELLED", "EXPIRED"];

/// `PaginatorDto` tervalidasi (pipe global `transform: true`, tanpa whitelist). Hanya yang
/// dipakai `findAll`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Paginator {
    /// `@Transform(parseInt)`; `None` = tidak dikirim.
    pub page_number: Option<i64>,
    pub items_per_page: Option<i64>,
}

/// `PaginatorDto`: `@IsNumber @IsOptional @Transform(parseInt)` untuk dua angka, `@IsString
/// @IsOptional` untuk `orderBy`/`orderColumn`/`search`; `status` tanpa dekorator.
///
/// DUPLIKAT dari `mz-admin::dto::validate_paginator` — sengaja dibiarkan (dicek 2026-11-18): di sini
/// `parseInt` → `i64` (`jsfmt::parse_int`, digit > i64 = galat validasi), di mz-admin → `f64` tanpa
/// luapan; mz-admin juga mengembalikan `search`.
pub fn validate_paginator(q: &[(String, QVal)]) -> ApiResult<Paginator> {
    let mut errors = Vec::new();
    let mut out = Paginator::default();
    for (name, slot) in [
        ("pageNumber", &mut out.page_number),
        ("itemsPerPage", &mut out.items_per_page),
    ] {
        match qs::get(q, name) {
            None => {}
            Some(QVal::One(s)) => match mz_core::jsfmt::parse_int(s) {
                Some(n) => *slot = Some(n),
                None => errors.push(format!(
                    "{name} must be a number conforming to the specified constraints"
                )),
            },
            Some(_) => errors.push(format!(
                "{name} must be a number conforming to the specified constraints"
            )),
        }
    }
    for name in ["orderBy", "orderColumn", "search"] {
        if matches!(qs::get(q, name), Some(QVal::Many(_) | QVal::Obj)) {
            errors.push(format!("{name} must be a string"));
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(ApiError::Validation(errors))
    }
}

/// Galat Prisma (argumen `skip`/`take`/`status` tidak sah) di blok `try` `findAll`.
fn find_all_failed(detail: &str) -> Value {
    tracing::warn!(detail, "GET /invoices: argumen Prisma tidak sah");
    json!({ "status": false, "message": "Failed to get all invoices data" })
}

/// Node: invoices.service.ts:123 (`findAll`). Galat apa pun di dalam `try` → 200
/// `{ status: false, message }` (bukan `success`).
pub async fn find_all(state: &AppState, user_id: i32, raw_query: &str) -> ApiResult<Value> {
    let q = qs::parse(raw_query);
    let p = validate_paginator(&q)?;

    // `user.userCompany[0].companyId`.
    let user_exists: Option<i32> = match sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
    {
        Ok(v) => v,
        Err(e) => return Ok(find_all_failed(&e.to_string())),
    };
    if user_exists.is_none() {
        return Ok(json!({
            "status": false,
            "message": "Cannot read properties of null (reading 'userCompany')",
        }));
    }
    let company_id = match mz_core::company::find_company_id(&state.db, user_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Ok(json!({
                "status": false,
                "message": "Cannot read properties of undefined (reading 'companyId')",
            }));
        }
        Err(e) => return Ok(find_all_failed(&e.to_string())),
    };

    // `if (query.search)` / `if (query.status)`: string kosong = tanpa filter.
    let search = qs::get(&q, "search")
        .and_then(QVal::as_str)
        .filter(|s| !s.is_empty());
    let status = match qs::get(&q, "status") {
        None => None,
        Some(QVal::One(s)) if s.is_empty() => None,
        Some(QVal::One(s)) if INVOICE_STATUSES.contains(&s.as_str()) => Some(s.as_str()),
        Some(_) => return Ok(find_all_failed("status bukan InvoiceStatus")),
    };

    // `skip: (parseInt(pageNumber) - 1) * parseInt(itemsPerPage)`, `take: parseInt(itemsPerPage)`.
    let (Some(page), Some(items)) = (p.page_number, p.items_per_page) else {
        return Ok(find_all_failed("skip/take NaN"));
    };
    let Some(skip) = page.checked_sub(1).and_then(|x| x.checked_mul(items)) else {
        return Ok(find_all_failed("skip overflow"));
    };
    if skip < 0 {
        return Ok(find_all_failed("skip negatif"));
    }

    let filter = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>| {
        qb.push(r#" WHERE s."companyId" = "#).push_bind(company_id);
        if let Some(s) = search {
            // Prisma `contains` + `mode: insensitive`: ILIKE tanpa meloloskan `%`/`_`.
            qb.push(r#" AND i."invoiceNumber" ILIKE "#)
                .push_bind(format!("%{s}%"));
        }
        if let Some(st) = status {
            qb.push(r#" AND i.status = "#)
                .push_bind(st.to_owned())
                .push(r#"::"InvoiceStatus""#);
        }
    };

    let mut list = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT {} FROM invoices i JOIN subscriptions s ON s.id = i.\"subscriptionId\"",
        select_cols()
    ));
    filter(&mut list);
    // `take` negatif (Prisma): urutan dibalik, ambil |take| sesudah `skip`, lalu dibalik lagi.
    let reverse = items < 0;
    list.push(if reverse {
        r#" ORDER BY i."createdAt" ASC"#
    } else {
        r#" ORDER BY i."createdAt" DESC"#
    });
    list.push(" LIMIT ")
        .push_bind(items.saturating_abs())
        .push(" OFFSET ")
        .push_bind(skip);

    let mut count = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT COUNT(*) FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId""#,
    );
    filter(&mut count);

    let rows = list.build_query_as::<InvoiceWithSub>().fetch_all(&state.db);
    let total = count.build_query_scalar::<i64>().fetch_one(&state.db);
    let (mut rows, total) = match tokio::try_join!(rows, total) {
        Ok(v) => v,
        Err(e) => return Ok(find_all_failed(&e.to_string())),
    };
    if reverse {
        rows.reverse();
    }

    Ok(json!({
        "success": true,
        "message": "Success get all data invoices",
        "data": {
            "total": total,
            "itemsPerPage": items,
            "pageNumber": page,
            "result": rows.iter().map(InvoiceWithSub::to_json).collect::<Vec<_>>(),
        },
    }))
}

/// Node: invoices.service.ts:188 (`findOne`). Invoice company lain = tidak ada (F6.2):
/// `{ status: false, message: 'Data not found' }`.
pub async fn find_one(state: &AppState, uuid: &Uuid, requester_id: i32) -> ApiResult<Value> {
    let sql = format!(
        r#"SELECT {} FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
            WHERE i.uuid = $1 LIMIT 1"#,
        select_cols()
    );
    let invoice = sqlx::query_as::<_, InvoiceWithSub>(&sql)
        .bind(uuid)
        .fetch_optional(&state.db);
    let requester = mz_files::load_file_requester(&state.db, requester_id);
    let (invoice, requester) = match tokio::try_join!(invoice, requester) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "GET /invoices/:uuid gagal");
            return Ok(json!({ "status": false, "message": "Failed to get invoices data" }));
        }
    };
    let Some(invoice) = invoice
        .filter(|i| mz_billing::can_read_invoice_pdf(&requester, Some(i.subscription.company_id)))
    else {
        return Ok(json!({ "status": false, "message": "Data not found" }));
    };
    Ok(json!({
        "success": true,
        "message": "Success",
        "data": invoice.to_json(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(raw: &str) -> Vec<String> {
        match validate_paginator(&qs::parse(raw)) {
            Err(ApiError::Validation(m)) => m,
            Ok(_) => vec![],
            Err(other) => vec![format!("{other:?}")],
        }
    }

    #[test]
    fn paginator_seperti_class_validator() {
        assert_eq!(
            validate_paginator(&qs::parse("pageNumber=2&itemsPerPage=10abc")).ok(),
            Some(Paginator {
                page_number: Some(2),
                items_per_page: Some(10)
            })
        );
        assert_eq!(
            msgs("pageNumber=x&itemsPerPage=&search[]=a&status[]=b"),
            vec![
                "pageNumber must be a number conforming to the specified constraints",
                "itemsPerPage must be a number conforming to the specified constraints",
                "search must be a string",
            ]
        );
        assert!(msgs("").is_empty());
    }
}
