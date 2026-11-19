//! Baris Prisma lengkap (`invoices` + `include: { subscription: true }`) untuk respons
//! `GET /paypal/subscription/status`. Urutan kunci = urutan field `schema.prisma`.
//! Definisi kolom bersama: [`mz_billing::rows`] (juga dipakai `mz-invoices`).

use mz_billing::rows::{InvoiceRow, SubscriptionRow};
use serde_json::Value;
use sqlx::PgPool;

/// Invoice terbaru (`createdAt desc`) dari subscription, opsional hanya UNPAID, beserta
/// `subscription`. Hasil: `(id, status, json)`.
pub async fn latest_invoice_with_subscription(
    db: &PgPool,
    subscription_id: i32,
    only_unpaid: bool,
) -> sqlx::Result<Option<(i32, String, Value)>> {
    let sql = format!(
        r#"SELECT {} FROM invoices
            WHERE "subscriptionId" = $1 AND (NOT $2::boolean OR status = 'UNPAID'::"InvoiceStatus")
            ORDER BY "createdAt" DESC, id DESC LIMIT 1"#,
        InvoiceRow::select("", "")
    );
    let row: Option<InvoiceRow> = sqlx::query_as(&sql)
        .bind(subscription_id)
        .bind(only_unpaid)
        .fetch_optional(db)
        .await?;
    match row {
        Some(r) => Ok(Some((
            r.id,
            r.status.clone(),
            with_subscription(db, &r).await?,
        ))),
        None => Ok(None),
    }
}

/// `invoices.findFirst({ where: { id }, include: { subscription: true } })`.
pub async fn invoice_with_subscription(
    db: &PgPool,
    invoice_id: i32,
) -> sqlx::Result<Option<(String, Value)>> {
    let sql = format!(
        "SELECT {} FROM invoices WHERE id = $1",
        InvoiceRow::select("", "")
    );
    let row: Option<InvoiceRow> = sqlx::query_as(&sql)
        .bind(invoice_id)
        .fetch_optional(db)
        .await?;
    match row {
        Some(r) => Ok(Some((r.status.clone(), with_subscription(db, &r).await?))),
        None => Ok(None),
    }
}

async fn with_subscription(db: &PgPool, inv: &InvoiceRow) -> sqlx::Result<Value> {
    let sql = format!(
        "SELECT {} FROM subscriptions WHERE id = $1",
        SubscriptionRow::select("", "")
    );
    let sub: Option<SubscriptionRow> = sqlx::query_as(&sql)
        .bind(inv.subscription_id)
        .fetch_optional(db)
        .await?;
    let mut m = inv.to_json();
    m.insert(
        "subscription".into(),
        sub.as_ref()
            .map_or(Value::Null, |s| Value::Object(s.to_json())),
    );
    Ok(Value::Object(m))
}
