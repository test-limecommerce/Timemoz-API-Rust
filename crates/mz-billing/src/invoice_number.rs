//! Port `src/invoices/invoice-number.ts` + `invoice-pdf-path.ts` (Mudah-API).

use mz_files::encode::encode_uri_component;

/// Pindah ke [`mz_core::invoice::generate_invoice_number`] (satu sumber untuk register, invoice
/// manual admin, dan billing); diekspor ulang di sini untuk pemanggil lama.
pub use mz_core::invoice::generate_invoice_number;

/// Node: invoices/invoice-pdf-path.ts:6 (`invoicePdfPath`). Path proxy ber-JWT PDF invoice;
/// pengecekan company pemilik dan pembuatan PDF terjadi di endpoint, bukan di sini.
pub fn invoice_pdf_path(invoice_uuid: &str) -> String {
    format!("/invoices/{}/pdf", encode_uri_component(invoice_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_pdf() {
        assert_eq!(
            invoice_pdf_path("0b9c5d2e-1111-4222-8333-444455556666"),
            "/invoices/0b9c5d2e-1111-4222-8333-444455556666/pdf"
        );
        assert_eq!(invoice_pdf_path("a b/c"), "/invoices/a%20b%2Fc/pdf");
    }
}
