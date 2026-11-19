//! Tulisan `subscriptions` / `invoices` / `invoiceItems` (model tanpa audit) ala Prisma:
//! `update({ where: { id }, data })` — kolom `undefined` tidak ditulis, baris hilang = galat P2025.

use chrono::NaiveDateTime;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::util::Fail;

/// Nilai kolom bertipe.
#[derive(Debug, Clone)]
pub enum V {
    Str(String),
    /// `NULL` atau teks.
    OptStr(Option<String>),
    I32(i32),
    F64(f64),
    Bool(bool),
    Ts(NaiveDateTime),
    OptTs(Option<NaiveDateTime>),
    /// Enum Postgres: (nilai, nama tipe).
    Enum(&'static str, &'static str),
}

pub fn sub_status(s: &'static str) -> V {
    V::Enum(s, "SubscriptionStatus")
}

pub fn invoice_status(s: &'static str) -> V {
    V::Enum(s, "InvoiceStatus")
}

pub fn paypal_source() -> V {
    V::Enum("PAYPAL", "SubscriptionBillingSource")
}

fn push_value(qb: &mut QueryBuilder<'_, Postgres>, v: &V) {
    match v {
        V::Str(s) => {
            qb.push_bind(s.clone());
        }
        V::OptStr(s) => {
            qb.push_bind(s.clone());
        }
        V::I32(x) => {
            qb.push_bind(*x);
        }
        V::F64(x) => {
            qb.push_bind(*x);
        }
        V::Bool(x) => {
            qb.push_bind(*x);
        }
        V::Ts(x) => {
            qb.push_bind(*x);
        }
        V::OptTs(x) => {
            qb.push_bind(*x);
        }
        V::Enum(val, ty) => {
            qb.push_bind(*val).push(format!("::\"{ty}\""));
        }
    }
}

/// `prisma.<table>.update({ where: { id }, data })`. `sets` kosong = hanya cek keberadaan.
pub async fn update_by_id(
    conn: &mut PgConnection,
    table: &str,
    id: i32,
    sets: &[(&str, V)],
) -> Result<(), Fail> {
    let n = if sets.is_empty() {
        sqlx::query(&format!(r#"SELECT 1 FROM "{table}" WHERE id = $1"#))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .map_or(0, |_| 1)
    } else {
        let mut qb = QueryBuilder::<Postgres>::new(format!(r#"UPDATE "{table}" SET "#));
        for (i, (col, v)) in sets.iter().enumerate() {
            if i > 0 {
                qb.push(", ");
            }
            qb.push(format!("\"{col}\" = "));
            push_value(&mut qb, v);
        }
        qb.push(" WHERE id = ").push_bind(id);
        qb.build().execute(&mut *conn).await?.rows_affected()
    };
    if n == 1 {
        Ok(())
    } else {
        Err(Fail::Other(format!(
            "{table} {id}: Record to update not found."
        )))
    }
}

/// `invoices.updateMany({ where: { subscriptionId, status: UNPAID }, data: { status: CANCELLED } })`.
pub async fn cancel_unpaid_invoices(
    conn: &mut PgConnection,
    subscription_id: i32,
) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        r#"UPDATE invoices SET status = 'CANCELLED'::"InvoiceStatus"
            WHERE "subscriptionId" = $1 AND status = 'UNPAID'::"InvoiceStatus""#,
    )
    .bind(subscription_id)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Data `invoices.create` (kolom lain = default skema).
pub struct NewInvoice<'a> {
    pub subscription_id: i32,
    pub invoice_number: &'a str,
    pub payment_url: &'a str,
    pub sub_price: f64,
    pub tax_price: f64,
    pub total_price: f64,
    pub expired_at: NaiveDateTime,
    /// `status` eksplisit (`changeActivePlan`) atau default skema `UNPAID`.
    pub status: Option<&'static str>,
}

/// `invoices.create({ data })` → id.
pub async fn create_invoice(conn: &mut PgConnection, inv: &NewInvoice<'_>) -> sqlx::Result<i32> {
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "taxPrice",
               "totalPrice", "expiredAt", "paymentProvider""#,
    );
    if inv.status.is_some() {
        qb.push(", status");
    }
    qb.push(") VALUES (")
        .push_bind(inv.subscription_id)
        .push(", ")
        .push_bind(inv.invoice_number)
        .push(", ")
        .push_bind(inv.payment_url)
        .push(", ")
        .push_bind(inv.sub_price)
        .push(", ")
        .push_bind(inv.tax_price)
        .push(", ")
        .push_bind(inv.total_price)
        .push(", ")
        .push_bind(inv.expired_at)
        .push(", 'paypal'");
    if let Some(s) = inv.status {
        qb.push(", ").push_bind(s).push(r#"::"InvoiceStatus""#);
    }
    qb.push(") RETURNING id");
    qb.build_query_scalar::<i32>().fetch_one(&mut *conn).await
}

/// `invoiceItems.create({ data: { invoiceId, description, quantity, unitPrice, totalPrice } })`.
pub async fn create_item(
    conn: &mut PgConnection,
    invoice_id: i32,
    description: &str,
    quantity: f64,
    unit_price: f64,
    total_price: f64,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(invoice_id)
    .bind(description)
    .bind(quantity)
    .bind(unit_price)
    .bind(total_price)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
