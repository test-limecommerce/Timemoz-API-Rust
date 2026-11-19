//! Baris Prisma lengkap `invoices` dan `subscriptions` (semua kolom skalar, urutan kunci = urutan
//! field `schema.prisma`) untuk respons `include: { subscription: true }`.
//!
//! Dipakai `mz-invoices` (`GET /invoices`, `GET /invoices/:uuid`: satu query JOIN, kolom
//! subscription berawalan `s_`) dan `mz-paypal-api` (`GET /paypal/subscription/status`: dua query
//! tanpa awalan). Satu daftar kolom per model dipakai untuk SELECT, dekode, dan JSON, jadi tidak
//! bisa lagi menyimpang antar-crate.

use chrono::NaiveDateTime;
use mz_core::jsfmt::num;
use mz_core::time::JsDate;
use serde_json::{Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, Row};

fn plain<T: Clone + Into<Value>>(x: &T) -> Value {
    x.clone().into()
}

fn number(x: &f64) -> Value {
    num(*x)
}

fn opt_number(x: &Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

fn date(d: &NaiveDateTime) -> Value {
    Value::String(JsDate(*d).to_js_string())
}

fn opt_date(d: &Option<NaiveDateTime>) -> Value {
    d.as_ref().map_or(Value::Null, date)
}

/// Satu definisi per model: `field: Tipe = "namaPrisma", "ekspresi SQL", konversi JSON;`.
macro_rules! prisma_row {
    ($(#[$doc:meta])* $name:ident { $($field:ident : $ty:ty = $col:literal, $sql:literal, $conv:ident;)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone)]
        pub struct $name {
            $(pub $field: $ty,)*
        }

        impl $name {
            /// `(ekspresi SQL tanpa alias tabel, nama kolom keluaran)`.
            const COLUMNS: &'static [(&'static str, &'static str)] = &[$(($sql, $col)),*];

            /// Daftar kolom SELECT: `{alias}{ekspresi} AS "{prefix}{nama}"`. `alias` mis. `"i."`
            /// atau `""`; `prefix` mis. `"s_"` untuk JOIN.
            pub fn select(alias: &str, prefix: &str) -> String {
                Self::COLUMNS
                    .iter()
                    .map(|(sql, col)| format!("{alias}{sql} AS \"{prefix}{col}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            }

            /// Dekode dari baris yang kolomnya dibuat [`Self::select`] dengan `prefix` yang sama.
            pub fn from_prefixed(row: &PgRow, prefix: &str) -> sqlx::Result<Self> {
                Ok(Self {
                    $($field: row.try_get(format!("{prefix}{}", $col).as_str())?,)*
                })
            }

            /// Objek JSON Prisma (urutan kunci = `schema.prisma`).
            pub fn to_json(&self) -> Map<String, Value> {
                let mut m = Map::new();
                $(m.insert($col.into(), $conv(&self.$field));)*
                m
            }
        }

        impl<'r> FromRow<'r, PgRow> for $name {
            fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
                Self::from_prefixed(row, "")
            }
        }
    };
}

prisma_row! {
    /// Model `invoices` (semua kolom skalar).
    InvoiceRow {
        id: i32 = "id", "id", plain;
        uuid: String = "uuid", "uuid::text", plain;
        subscription_id: i32 = "subscriptionId", "\"subscriptionId\"", plain;
        invoice_number: String = "invoiceNumber", "\"invoiceNumber\"", plain;
        payment_url: String = "paymentUrl", "\"paymentUrl\"", plain;
        sub_price: f64 = "subPrice", "\"subPrice\"", number;
        tax_price: f64 = "taxPrice", "\"taxPrice\"", number;
        fee_price: f64 = "feePrice", "\"feePrice\"", number;
        total_price: f64 = "totalPrice", "\"totalPrice\"", number;
        status: String = "status", "status::text", plain;
        invoice_type: String = "invoiceType", "\"invoiceType\"::text", plain;
        payment_provider: String = "paymentProvider", "\"paymentProvider\"", plain;
        paypal_payment_id: Option<String> = "paypalPaymentId", "\"paypalPaymentId\"", plain;
        created_at: NaiveDateTime = "createdAt", "\"createdAt\"", date;
        updated_at: NaiveDateTime = "updatedAt", "\"updatedAt\"", date;
        expired_at: NaiveDateTime = "expiredAt", "\"expiredAt\"", date;
        invoice_path_pdf: String = "invoicePathPdf", "\"invoicePathPdf\"", plain;
        currency: String = "currency", "currency", plain;
        payment_method: Option<String> = "paymentMethod", "\"paymentMethod\"", plain;
        payment_reference: Option<String> = "paymentReference", "\"paymentReference\"", plain;
        paid_at: Option<NaiveDateTime> = "paidAt", "\"paidAt\"", opt_date;
        period_start: Option<NaiveDateTime> = "periodStart", "\"periodStart\"", opt_date;
        period_end: Option<NaiveDateTime> = "periodEnd", "\"periodEnd\"", opt_date;
        note: Option<String> = "note", "note", plain;
        recorded_by_operator_id: Option<i32> = "recordedByOperatorId", "\"recordedByOperatorId\"", plain;
        proration_deactivated_user_company_ids: Vec<i32> = "prorationDeactivatedUserCompanyIds", "\"prorationDeactivatedUserCompanyIds\"", plain;
    }
}

prisma_row! {
    /// Model `subscriptions` (semua kolom skalar).
    SubscriptionRow {
        id: i32 = "id", "id", plain;
        uuid: String = "uuid", "uuid::text", plain;
        company_id: i32 = "companyId", "\"companyId\"", plain;
        pricing_tier_id: i32 = "pricingTierId", "\"pricingTierId\"", plain;
        start_date: NaiveDateTime = "startDate", "\"startDate\"", date;
        end_date: NaiveDateTime = "endDate", "\"endDate\"", date;
        status: String = "status", "status::text", plain;
        max_users: i32 = "maxUsers", "\"maxUsers\"", plain;
        extend_users: i32 = "extendUsers", "\"extendUsers\"", plain;
        quantity: i32 = "quantity", "quantity", plain;
        price_per_user: f64 = "pricePerUser", "\"pricePerUser\"", number;
        billing_interval: String = "billingInterval", "\"billingInterval\"", plain;
        paypal_subscription_id: Option<String> = "paypalSubscriptionId", "\"paypalSubscriptionId\"", plain;
        paypal_plan_id: Option<String> = "paypalPlanId", "\"paypalPlanId\"", plain;
        paypal_status: Option<String> = "paypalStatus", "\"paypalStatus\"", plain;
        auto_renew: bool = "autoRenew", "\"autoRenew\"", plain;
        next_invoice_date: Option<NaiveDateTime> = "nextInvoiceDate", "\"nextInvoiceDate\"", opt_date;
        last_billed_quantity: i32 = "lastBilledQuantity", "\"lastBilledQuantity\"", plain;
        payment_failed_at: Option<NaiveDateTime> = "paymentFailedAt", "\"paymentFailedAt\"", opt_date;
        d1_reminder_sent_at: Option<NaiveDateTime> = "d1ReminderSentAt", "\"d1ReminderSentAt\"", opt_date;
        pending_pricing_tier_id: Option<i32> = "pendingPricingTierId", "\"pendingPricingTierId\"", plain;
        pending_paypal_subscription_id: Option<String> = "pendingPaypalSubscriptionId", "\"pendingPaypalSubscriptionId\"", plain;
        pending_paypal_plan_id: Option<String> = "pendingPaypalPlanId", "\"pendingPaypalPlanId\"", plain;
        billing_source: String = "billingSource", "\"billingSource\"::text", plain;
        manual_reminder_for_end_date: Option<NaiveDateTime> = "manualReminderForEndDate", "\"manualReminderForEndDate\"", opt_date;
        contract_price_per_user: Option<f64> = "contractPricePerUser", "\"contractPricePerUser\"", opt_number;
        contract_currency: Option<String> = "contractCurrency", "\"contractCurrency\"", plain;
        contract_period_months: Option<i32> = "contractPeriodMonths", "\"contractPeriodMonths\"", plain;
        contract_tax_percent: f64 = "contractTaxPercent", "\"contractTaxPercent\"", number;
        created_at: NaiveDateTime = "createdAt", "\"createdAt\"", date;
        updated_at: Option<NaiveDateTime> = "updatedAt", "\"updatedAt\"", opt_date;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_memakai_alias_dan_awalan() {
        let inv = InvoiceRow::select("i.", "");
        assert!(inv.starts_with(
            r#"i.id AS "id", i.uuid::text AS "uuid", i."subscriptionId" AS "subscriptionId""#
        ));
        assert!(inv.ends_with(
            r#"i."prorationDeactivatedUserCompanyIds" AS "prorationDeactivatedUserCompanyIds""#
        ));
        assert_eq!(inv.matches(" AS ").count(), 26);
        let sub = SubscriptionRow::select("s.", "s_");
        assert!(sub.contains(r#"s.status::text AS "s_status""#));
        assert!(sub.contains(r#"s."billingSource"::text AS "s_billingSource""#));
        assert_eq!(sub.matches(" AS ").count(), 31);
        assert!(SubscriptionRow::select("", "").starts_with(r#"id AS "id", uuid::text AS "uuid""#));
    }
}
