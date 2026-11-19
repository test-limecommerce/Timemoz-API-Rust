//! Baris Prisma lengkap `invoices` + `include: { subscription: true }` (+ `has_payment_link`)
//! untuk `GET /invoices` dan `GET /invoices/:uuid`. Urutan kunci = urutan field `schema.prisma`.
//! Definisi kolom bersama: [`mz_billing::rows`] (juga dipakai `mz-paypal-api`).

use mz_billing::rows::{InvoiceRow, SubscriptionRow};
use serde_json::Value;
use sqlx::FromRow;
use sqlx::postgres::PgRow;

/// Kolom SELECT invoice (alias `i`) lalu subscription (alias `s`, berawalan `s_`).
pub fn select_cols() -> String {
    format!(
        "{}, {}",
        InvoiceRow::select("i.", ""),
        SubscriptionRow::select("s.", "s_")
    )
}

#[derive(Debug, Clone)]
pub struct InvoiceWithSub {
    pub invoice: InvoiceRow,
    pub subscription: SubscriptionRow,
}

impl<'r> FromRow<'r, PgRow> for InvoiceWithSub {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            invoice: InvoiceRow::from_prefixed(row, "")?,
            subscription: SubscriptionRow::from_prefixed(row, "s_")?,
        })
    }
}

impl InvoiceWithSub {
    /// `{ ...invoice, has_payment_link: status === 'UNPAID' && !!paymentUrl }`.
    pub fn to_json(&self) -> Value {
        let mut m = self.invoice.to_json();
        m.insert(
            "subscription".into(),
            Value::Object(self.subscription.to_json()),
        );
        m.insert(
            "has_payment_link".into(),
            (self.invoice.status == "UNPAID" && !self.invoice.payment_url.is_empty()).into(),
        );
        Value::Object(m)
    }
}
