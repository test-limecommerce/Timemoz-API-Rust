//! Nomor invoice bersama (dipakai `mz-auth` register, `mz-admin-company` manual, `mz-billing`
//! dan semua turunannya). Satu sumber supaya format tidak bisa menyimpang antar-crate.

use chrono::{DateTime, Utc};

/// Node: invoices/invoice-number.ts:8 (`generateInvoiceNumber`). Pola `INV-DDMMYYYY-UNIXTIME`;
/// tanggal memakai zona waktu lokal proses (produksi TZ=UTC), detik unix dari `now`.
/// Salinan inline Node (`auth.service` register, `admin-company` manual) memakai pola yang sama.
pub fn generate_invoice_number(now: DateTime<Utc>) -> String {
    format!("INV-{}-{}", now.format("%d%m%Y"), now.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn nomor_invoice_ddmmyyyy_unix() {
        let t = Utc.with_ymd_and_hms(2026, 11, 7, 8, 0, 5).single();
        let Some(t) = t else { return };
        assert_eq!(
            generate_invoice_number(t),
            format!("INV-07112026-{}", t.timestamp())
        );
        let at = DateTime::from_timestamp(1_763_280_000, 0).unwrap_or_default();
        assert_eq!(generate_invoice_number(at), "INV-16112025-1763280000");
    }
}
