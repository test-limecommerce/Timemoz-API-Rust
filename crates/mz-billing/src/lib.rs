//! Pustaka domain billing bersama (tahap A port billing, `plan/BILLING_AGENT_BRIEF.md`).
//!
//! Isi (rujukan Node di tiap fungsi `pub`; ringkasan di `plan/billing-mz-billing-api.md`):
//! - [`invoice_number`] — `generateInvoiceNumber`, `invoicePdfPath`.
//! - [`pdf`] + [`invoice_pdf_layout`] — penulis PDF ala pdfkit dan generator PDF invoice
//!   (`invoice-pdf.generator.ts`).
//! - [`invoice_pdf`] — `generateInvoicePdf`, `streamInvoicePdf`, `streamInvoicePdfForOperator`,
//!   `regenerateInvoicePdf`, `ensureInvoicePdfKey` + helper akses/versi/label pajak.
//! - [`free_tier`] — `activateFreeTier`, `resolveFreeTierFor`, `autoDowngradeToFree`, helper masa Free.
//! - [`screenshots`] — `cleanupCompressedScreenshots`.
//! - [`manual_billing`] — `assertNotManualBilling`.
//! - [`routes`] — `GET /admin-invoices/{uuid}/pdf`, `POST /admin-invoices/{uuid}/regenerate-pdf`.
//!
//! Deviasi: `plan/deviations/mz-billing.md`.

#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used
    )
)]

pub mod free_tier;
pub mod invoice_number;
pub mod invoice_pdf;
pub mod invoice_pdf_layout;
pub mod manual_billing;
pub mod pdf;
pub mod routes;
pub mod rows;
pub mod screenshots;

pub use free_tier::{
    FreeTier, FreeTierGuard, FreeTierOutcome, FreeTierSubscription, PaypalSubscriptionCanceller,
    activate_free_tier, auto_downgrade_to_free, free_forever_end_date, free_subscription_end_date,
    is_zero_priced_tier, resolve_free_tier_for,
};
pub use invoice_number::{generate_invoice_number, invoice_pdf_path};
pub use invoice_pdf::{
    INVOICE_PDF_CACHE_CONTROL, INVOICE_PDF_LAYOUT_VERSION, InvoiceForPdf, RegeneratedPdf,
    can_read_invoice_pdf, ensure_invoice_pdf_key, generate_invoice_pdf_response,
    is_current_invoice_pdf_key, load_invoice_for_pdf, manual_payment_instructions,
    manual_tax_label, regenerate_invoice_pdf, stream_invoice_pdf, stream_invoice_pdf_for_operator,
};
pub use invoice_pdf_layout::{
    InvoiceData, InvoiceItem, format_invoice_currency, generate as generate_pdf,
    invoice_provider_label,
};
pub use manual_billing::{assert_not_manual_billing, is_manual_running, manual_billing_error};
pub use routes::router;
pub use screenshots::cleanup_compressed_screenshots;

#[cfg(test)]
mod golden_tests;
