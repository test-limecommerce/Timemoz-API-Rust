//! Port bagian PDF `src/invoices/invoices.service.ts` (Mudah-API): `generateInvoicePdf`,
//! `streamInvoicePdf`, `streamInvoicePdfForOperator`, `regenerateInvoicePdf`,
//! `ensureInvoicePdfKey` + helper (`canReadInvoicePdf`, `manualTaxLabel`,
//! `manualPaymentInstructions`, `isCurrentInvoicePdfKey`).
//!
//! Kunci GCS ber-versi identik dengan Node: `invoices/<invoiceNumber>-<unix>.v<N>.pdf`, kolom
//! `invoices."invoicePathPdf"`; PDF dengan versi lain dianggap basi dan dibuat ulang saat dibuka.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::NaiveDateTime;
use mz_core::enums::Role;
use mz_core::jsfmt::num_string;
use mz_core::{ApiError, ApiResult, AppState};
use mz_files::{FileRequester, StreamObjectOptions, load_file_requester};
use serde_json::{Value, json};
use sqlx::FromRow;
use uuid::Uuid;

use crate::invoice_number::invoice_pdf_path;
use crate::invoice_pdf_layout::{InvoiceData, InvoiceItem, generate};

/// Node: invoices.service.ts:63. Versi layout PDF invoice, ikut di nama file (`….v2.pdf`).
/// Naikkan setiap tampilan PDF berubah.
pub const INVOICE_PDF_LAYOUT_VERSION: u32 = 2;

/// Node: invoices.service.ts:65.
pub const INVOICE_PDF_CACHE_CONTROL: &str = "private, no-cache";

/// Node: invoices.service.ts:67 (`isCurrentInvoicePdfKey`).
pub fn is_current_invoice_pdf_key(key: &str) -> bool {
    key.ends_with(&format!(".v{INVOICE_PDF_LAYOUT_VERSION}.pdf"))
}

/// Node: invoices.service.ts:72 (`manualTaxLabel`). `Tax (11%)` dari nominal invoice manual;
/// `Tax` saja bila tanpa pajak.
pub fn manual_tax_label(sub_price: f64, tax_price: f64) -> String {
    // JS: `!(subPrice > 0) || !(taxPrice > 0)` (NaN → true).
    let positive = |x: f64| x > 0.0;
    if !positive(sub_price) || !positive(tax_price) {
        return "Tax".to_owned();
    }
    // Math.round(x * 10000) / 100
    let percent = ((tax_price / sub_price) * 10000.0 + 0.5).floor() / 100.0;
    format!("Tax ({}%)", num_string(percent))
}

const DEFAULT_PAYMENT_INSTRUCTIONS: &str = "Please transfer the total amount before the due date and include the invoice number as the payment reference.";

/// Node: invoices.service.ts:83 (`manualPaymentInstructions`). Dari
/// `MANUAL_BILLING_PAYMENT_INSTRUCTIONS`; `\n` (dua karakter) di env dibaca sebagai baris baru.
pub fn manual_payment_instructions(state: &AppState) -> String {
    manual_payment_instructions_from(state.config.var("MANUAL_BILLING_PAYMENT_INSTRUCTIONS"))
}

fn manual_payment_instructions_from(raw: Option<String>) -> String {
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => raw.replace("\\n", "\n"),
        None => DEFAULT_PAYMENT_INSTRUCTIONS.to_owned(),
    }
}

/// Node: invoices.service.ts:102 (`canReadInvoicePdf`). SYSTEM_ADMIN semua company;
/// ADMIN/SUPER_ADMIN hanya company pemilik invoice; selain itu tidak.
pub fn can_read_invoice_pdf(
    requester: &FileRequester,
    subscription_company_id: Option<i32>,
) -> bool {
    let role = requester.role_code.as_deref();
    if role == Some(Role::SYSTEM_ADMIN.as_str()) {
        return true;
    }
    if role != Some(Role::ADMIN.as_str()) && role != Some(Role::SUPER_ADMIN.as_str()) {
        return false;
    }
    matches!(
        (subscription_company_id, requester.company_id),
        (Some(a), Some(b)) if a == b
    )
}

/// Padanan `INVOICE_PDF_INCLUDE` (invoice + items + subscription + company + user default).
#[derive(Debug, Clone, FromRow)]
pub struct InvoiceForPdf {
    pub id: i32,
    pub uuid: Uuid,
    #[sqlx(rename = "invoiceNumber")]
    pub invoice_number: String,
    pub status: String,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "expiredAt")]
    pub expired_at: NaiveDateTime,
    #[sqlx(rename = "subPrice")]
    pub sub_price: f64,
    #[sqlx(rename = "taxPrice")]
    pub tax_price: f64,
    #[sqlx(rename = "feePrice")]
    pub fee_price: f64,
    #[sqlx(rename = "totalPrice")]
    pub total_price: f64,
    #[sqlx(rename = "paymentProvider")]
    pub payment_provider: String,
    pub currency: String,
    #[sqlx(rename = "paymentReference")]
    pub payment_reference: Option<String>,
    #[sqlx(rename = "paidAt")]
    pub paid_at: Option<NaiveDateTime>,
    #[sqlx(rename = "periodStart")]
    pub period_start: Option<NaiveDateTime>,
    #[sqlx(rename = "periodEnd")]
    pub period_end: Option<NaiveDateTime>,
    #[sqlx(rename = "paymentUrl")]
    pub payment_url: String,
    #[sqlx(rename = "invoicePathPdf")]
    pub invoice_path_pdf: String,
    /// `subscription.companyId`.
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "billingInterval")]
    pub billing_interval: String,
    pub quantity: i32,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "companyName")]
    pub company_name: Option<String>,
    #[sqlx(rename = "companyTelp")]
    pub company_telp: Option<String>,
    #[sqlx(rename = "companyAddress")]
    pub company_address: Option<String>,
    /// `company.userCompany[isDefault=true][0].user`.
    #[sqlx(rename = "userName")]
    pub user_name: Option<String>,
    #[sqlx(rename = "userUsername")]
    pub user_username: Option<String>,
    #[sqlx(skip)]
    pub items: Vec<InvoiceItem>,
}

#[derive(FromRow)]
struct ItemRow {
    description: String,
    quantity: f64,
    #[sqlx(rename = "unitPrice")]
    unit_price: f64,
    #[sqlx(rename = "totalPrice")]
    total_price: f64,
}

/// `prisma.invoices.findFirst({ where: { uuid }, include: INVOICE_PDF_INCLUDE })`.
pub async fn load_invoice_for_pdf(
    state: &AppState,
    invoice_uuid: &Uuid,
) -> sqlx::Result<Option<InvoiceForPdf>> {
    let row: Option<InvoiceForPdf> = sqlx::query_as(
        r#"SELECT i.id, i.uuid, i."invoiceNumber", i.status::text AS status,
                  i."createdAt", i."expiredAt", i."subPrice", i."taxPrice", i."feePrice",
                  i."totalPrice", i."paymentProvider", i.currency, i."paymentReference",
                  i."paidAt", i."periodStart", i."periodEnd", i."paymentUrl", i."invoicePathPdf",
                  s."companyId", s."billingInterval", s.quantity, s."pricePerUser",
                  c.name AS "companyName", c.telp AS "companyTelp", c.address AS "companyAddress",
                  u.name AS "userName", u.username AS "userUsername"
             FROM invoices i
             JOIN subscriptions s ON s.id = i."subscriptionId"
             LEFT JOIN company c ON c.id = s."companyId"
             LEFT JOIN LATERAL (
                 SELECT us.name, us.username
                   FROM "userCompany" uc JOIN users us ON us.id = uc."userId"
                  WHERE uc."companyId" = c.id AND uc."isDefault" = true
                  ORDER BY uc.id LIMIT 1
             ) u ON true
            WHERE i.uuid = $1
            LIMIT 1"#,
    )
    .bind(invoice_uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(mut invoice) = row else {
        return Ok(None);
    };
    let items: Vec<ItemRow> = sqlx::query_as(
        r#"SELECT description, quantity, "unitPrice", "totalPrice"
             FROM "invoiceItems" WHERE "invoiceId" = $1 ORDER BY id"#,
    )
    .bind(invoice.id)
    .fetch_all(&state.db)
    .await?;
    invoice.items = items
        .into_iter()
        .map(|i| InvoiceItem {
            description: i.description,
            quantity: i.quantity,
            unit_price: i.unit_price,
            total_price: i.total_price,
        })
        .collect();
    Ok(Some(invoice))
}

fn non_empty(s: Option<&String>) -> Option<String> {
    s.filter(|v| !v.is_empty()).cloned()
}

/// Data PDF dari baris invoice (blok `pdfData` di `ensureInvoicePdfKey`).
pub fn build_pdf_data(state: &AppState, invoice: &InvoiceForPdf) -> InvoiceData {
    let is_manual = invoice.payment_provider == "manual";
    let provider = if invoice.payment_provider.is_empty() {
        "midtrans".to_owned()
    } else {
        invoice.payment_provider.clone()
    };
    InvoiceData {
        invoice_number: invoice.invoice_number.clone(),
        status: invoice.status.clone(),
        created_at: invoice.created_at,
        expired_at: invoice.expired_at,
        customer_name: non_empty(invoice.user_name.as_ref()).unwrap_or_else(|| "-".to_owned()),
        customer_email: non_empty(invoice.user_username.as_ref()).unwrap_or_else(|| "-".to_owned()),
        company_name: non_empty(invoice.company_name.as_ref()).unwrap_or_else(|| "-".to_owned()),
        company_phone: non_empty(invoice.company_telp.as_ref()).unwrap_or_else(|| "-".to_owned()),
        company_address: non_empty(invoice.company_address.as_ref()),
        items: invoice.items.clone(),
        sub_price: invoice.sub_price,
        tax_price: invoice.tax_price,
        fee_price: invoice.fee_price,
        total_price: invoice.total_price,
        // Invoice midtrans lama tersimpan dengan default kolom `USD`; nominalnya IDR.
        currency: Some(if invoice.payment_provider == "midtrans" {
            "IDR".to_owned()
        } else {
            invoice.currency.clone()
        }),
        payment_provider: provider,
        payment_reference: invoice.payment_reference.clone(),
        paid_at: invoice.paid_at,
        period_start: invoice.period_start,
        period_end: invoice.period_end,
        // Invoice manual merinci kuota × bulan × harga kontrak di item-nya; blok
        // "user @ harga" PayPal disembunyikan.
        billing_interval: (!is_manual).then(|| {
            if invoice.billing_interval.is_empty() {
                "MONTH".to_owned()
            } else {
                invoice.billing_interval.clone()
            }
        }),
        quantity: (!is_manual).then(|| {
            if invoice.quantity == 0 {
                1.0
            } else {
                f64::from(invoice.quantity)
            }
        }),
        price_per_user: (!is_manual).then_some(invoice.price_per_user),
        tax_label: is_manual.then(|| manual_tax_label(invoice.sub_price, invoice.tax_price)),
        payment_instructions: (is_manual && invoice.status == "UNPAID")
            .then(|| manual_payment_instructions(state)),
        payment_url: non_empty(Some(&invoice.payment_url)),
    }
}

/// Node: invoices.service.ts:1897 (`ensureInvoicePdfKey`). Key PDF invoice di bucket; dibuat +
/// diunggah bila belum ada atau bila PDF tersimpan memakai layout lama.
///
/// TOCTOU: dua request pertama yang bersamaan sama-sama membuat PDF. Penulisan key bersyarat
/// (key masih sama dengan yang dibaca); yang kalah menghapus object-nya sendiri dan memakai key
/// pemenang, supaya tidak ada object yatim.
pub async fn ensure_invoice_pdf_key(
    state: &AppState,
    invoice: &InvoiceForPdf,
) -> ApiResult<String> {
    let previous_key = invoice.invoice_path_pdf.clone();
    if !previous_key.is_empty() && is_current_invoice_pdf_key(&previous_key) {
        return Ok(previous_key);
    }

    let pdf = generate(&build_pdf_data(state, invoice));

    // Upload to GCS
    let unix_time = chrono::Utc::now().timestamp();
    let file_name = format!(
        "invoices/{}-{unix_time}.v{INVOICE_PDF_LAYOUT_VERSION}.pdf",
        invoice.invoice_number
    );
    // Tanpa `cacheControl: public` (private_file_access.md F1): bucket private.
    mz_files::service::upload_buffer(state, &file_name, pdf, "application/pdf").await?;

    // Update path in DB — bersyarat, lihat TOCTOU di atas.
    let updated = sqlx::query(
        r#"UPDATE invoices SET "invoicePathPdf" = $1 WHERE id = $2 AND "invoicePathPdf" = $3"#,
    )
    .bind(&file_name)
    .bind(invoice.id)
    .bind(&previous_key)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 1 {
        // PDF layout lama yang digantikan: hapus best-effort.
        if !previous_key.is_empty() {
            let _ = mz_files::service::delete_file(state, &previous_key).await;
        }
        return Ok(file_name);
    }

    let _ = mz_files::service::delete_file(state, &file_name).await;
    let winner: Option<String> =
        sqlx::query_scalar(r#"SELECT "invoicePathPdf" FROM invoices WHERE id = $1"#)
            .bind(invoice.id)
            .fetch_optional(&state.db)
            .await?;
    match winner.filter(|w| !w.is_empty()) {
        Some(w) => Ok(w),
        None => Err(ApiError::internal(anyhow::anyhow!(
            "Invoice PDF path vanished while generating"
        ))),
    }
}

/// Hasil [`regenerate_invoice_pdf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegeneratedPdf {
    pub invoice_id: i32,
    pub company_id: i32,
    pub url: String,
}

/// Node: invoices.service.ts:1838 (`regenerateInvoicePdf`). Buang key PDF lama lalu bangun ulang.
/// `Ok(None)` bila invoice tidak ada. Status/nominal tidak disentuh.
///
/// TOCTOU: pengosongan `invoicePathPdf` bersyarat pada key yang baru dibaca, jadi dua regenerate
/// bersamaan tak saling menimpa. Object lama dihapus best-effort sesudah key baru tertulis.
pub async fn regenerate_invoice_pdf(
    state: &AppState,
    invoice_uuid: &Uuid,
) -> ApiResult<Option<RegeneratedPdf>> {
    let Some(invoice) = load_invoice_for_pdf(state, invoice_uuid).await? else {
        return Ok(None);
    };

    let old_key = invoice.invoice_path_pdf.clone();
    if !old_key.is_empty() {
        sqlx::query(
            r#"UPDATE invoices SET "invoicePathPdf" = '' WHERE id = $1 AND "invoicePathPdf" = $2"#,
        )
        .bind(invoice.id)
        .bind(&old_key)
        .execute(&state.db)
        .await?;
    }
    let blank = InvoiceForPdf {
        invoice_path_pdf: String::new(),
        ..invoice.clone()
    };
    let key = match ensure_invoice_pdf_key(state, &blank).await {
        Ok(k) => k,
        Err(err) => {
            // Rebuild failed (PDF render or upload): put the old pointer back so the invoice
            // keeps a downloadable PDF. Conditional on the pointer still being empty, so a
            // concurrent successful regenerate is never overwritten.
            if !old_key.is_empty() {
                let _ = sqlx::query(
                    r#"UPDATE invoices SET "invoicePathPdf" = $2 WHERE id = $1 AND "invoicePathPdf" = ''"#,
                )
                .bind(invoice.id)
                .bind(&old_key)
                .execute(&state.db)
                .await;
            }
            return Err(err);
        }
    };
    if !old_key.is_empty() && old_key != key {
        let _ = mz_files::service::delete_file(state, &old_key).await;
    }
    Ok(Some(RegeneratedPdf {
        invoice_id: invoice.id,
        company_id: invoice.company_id,
        url: invoice_pdf_path(&invoice.uuid.to_string()),
    }))
}

fn stream_opts(invoice: &InvoiceForPdf) -> StreamObjectOptions {
    StreamObjectOptions {
        filename: Some(format!("{}.pdf", invoice.invoice_number)),
        // PDF bisa dibuat ulang (layout baru / setelah dibayar) di URL yang sama: browser
        // wajib revalidasi lewat ETag, bukan memakai salinan 5 menit.
        cache_control: Some(INVOICE_PDF_CACHE_CONTROL.to_owned()),
        ..StreamObjectOptions::default()
    }
}

/// Node: invoices.service.ts:1776 (`streamInvoicePdf`) — `GET /invoices/:uuid/pdf`. Hanya
/// ADMIN/SUPER_ADMIN company pemilik invoice; SYSTEM_ADMIN lintas company; selain itu 404
/// `Invoice not found`. PDF yang belum ada dibuat dulu. Key selalu dari DB.
pub async fn stream_invoice_pdf(
    state: &AppState,
    invoice_uuid: &Uuid,
    requester_id: i32,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let (invoice, requester) = tokio::try_join!(
        load_invoice_for_pdf(state, invoice_uuid),
        load_file_requester(&state.db, requester_id)
    )?;
    let Some(invoice) = invoice.filter(|i| can_read_invoice_pdf(&requester, Some(i.company_id)))
    else {
        return Err(ApiError::not_found("Invoice not found"));
    };
    let key = ensure_invoice_pdf_key(state, &invoice).await?;
    mz_files::stream_object(state, &key, Some(headers), stream_opts(&invoice)).await
}

/// Node: invoices.service.ts:1807 (`streamInvoicePdfForOperator`). Untuk dashboard operator
/// (semua company, tanpa cek requester). 404 `{code:"INVOICE_NOT_FOUND"}`.
pub async fn stream_invoice_pdf_for_operator(
    state: &AppState,
    invoice_uuid: &Uuid,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let Some(invoice) = load_invoice_for_pdf(state, invoice_uuid).await? else {
        return Err(invoice_not_found());
    };
    let key = ensure_invoice_pdf_key(state, &invoice).await?;
    mz_files::stream_object(state, &key, Some(headers), stream_opts(&invoice)).await
}

/// `NotFoundException({ code: 'INVOICE_NOT_FOUND', message: 'Invoice not found' })`.
pub fn invoice_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "INVOICE_NOT_FOUND",
        "Invoice not found",
    )
}

/// Node: invoices.service.ts:1728 (`generateInvoicePdf`) — isi `GET /invoices/download/pdf/:uuid`.
/// `data.url` = path proxy ber-JWT; PDF dibuat saat path itu dibuka. Invoice company lain dijawab
/// sama dengan invoice yang tidak ada. Tidak pernah melempar (galat → `{success:false,message}`).
pub async fn generate_invoice_pdf_response(
    state: &AppState,
    invoice_uuid: &str,
    requester_id: i32,
) -> Value {
    let not_found = || json!({ "success": false, "message": "Invoice not found" });
    let Ok(uuid) = Uuid::try_parse(invoice_uuid) else {
        // Prisma menolak uuid rusak (P2023) → catch Node.
        return json!({ "success": false, "message": "Failed to generate invoice PDF" });
    };
    let run = async {
        let invoice: Option<(String, i32)> = sqlx::query_as(
            r#"SELECT i.uuid::text, s."companyId" FROM invoices i
                 JOIN subscriptions s ON s.id = i."subscriptionId" WHERE i.uuid = $1 LIMIT 1"#,
        )
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
        let requester = load_file_requester(&state.db, requester_id).await?;
        Ok::<_, sqlx::Error>((invoice, requester))
    };
    match run.await {
        Ok((Some((uuid, company_id)), requester))
            if can_read_invoice_pdf(&requester, Some(company_id)) =>
        {
            json!({
                "success": true,
                "message": "Success get invoice PDF url",
                "data": { "url": invoice_pdf_path(&uuid) },
            })
        }
        Ok(_) => not_found(),
        Err(err) => {
            tracing::error!(error = %err, "PDF_GEN_ERROR");
            json!({ "success": false, "message": err.to_string() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versi_key() {
        assert!(is_current_invoice_pdf_key(
            "invoices/INV-1-1763366400.v2.pdf"
        ));
        assert!(!is_current_invoice_pdf_key(
            "invoices/INV-1-1763366400.v1.pdf"
        ));
        assert!(!is_current_invoice_pdf_key("invoices/INV-1-1763366400.pdf"));
        assert!(!is_current_invoice_pdf_key(""));
    }

    #[test]
    fn label_pajak_manual() {
        assert_eq!(manual_tax_label(1_800_000.0, 198_000.0), "Tax (11%)");
        assert_eq!(manual_tax_label(100.0, 0.0), "Tax");
        assert_eq!(manual_tax_label(0.0, 10.0), "Tax");
        assert_eq!(manual_tax_label(1000.0, 105.0), "Tax (10.5%)");
        assert_eq!(manual_tax_label(3.0, 1.0), "Tax (33.33%)");
    }

    #[test]
    fn instruksi_pembayaran() {
        assert_eq!(
            manual_payment_instructions_from(None),
            DEFAULT_PAYMENT_INSTRUCTIONS
        );
        assert_eq!(
            manual_payment_instructions_from(Some("  ".into())),
            DEFAULT_PAYMENT_INSTRUCTIONS
        );
        assert_eq!(
            manual_payment_instructions_from(Some(" BCA 123\\nA/N PT X ".into())),
            "BCA 123\nA/N PT X"
        );
    }

    fn requester(role: Option<&str>, company: Option<i32>) -> FileRequester {
        FileRequester {
            user_id: 1,
            company_id: company,
            role_code: role.map(str::to_owned),
        }
    }

    #[test]
    fn akses_pdf() {
        assert!(can_read_invoice_pdf(
            &requester(Some("SYSTEM_ADMIN"), None),
            Some(9)
        ));
        assert!(can_read_invoice_pdf(
            &requester(Some("ADMIN"), Some(9)),
            Some(9)
        ));
        assert!(can_read_invoice_pdf(
            &requester(Some("SUPER_ADMIN"), Some(9)),
            Some(9)
        ));
        assert!(!can_read_invoice_pdf(
            &requester(Some("ADMIN"), Some(8)),
            Some(9)
        ));
        assert!(!can_read_invoice_pdf(
            &requester(Some("ADMIN"), None),
            Some(9)
        ));
        assert!(!can_read_invoice_pdf(
            &requester(Some("ADMIN"), Some(9)),
            None
        ));
        assert!(!can_read_invoice_pdf(
            &requester(Some("MANAGER"), Some(9)),
            Some(9)
        ));
        assert!(!can_read_invoice_pdf(&requester(None, Some(9)), Some(9)));
    }

    #[tokio::test]
    async fn pdf_data_midtrans_dan_manual() {
        let Ok(state) = mz_core::AppState::for_test() else {
            return;
        };
        let mut inv = InvoiceForPdf {
            id: 1,
            uuid: Uuid::nil(),
            invoice_number: "INV-1".into(),
            status: "UNPAID".into(),
            created_at: NaiveDateTime::default(),
            expired_at: NaiveDateTime::default(),
            sub_price: 100.0,
            tax_price: 11.0,
            fee_price: 0.0,
            total_price: 111.0,
            payment_provider: "midtrans".into(),
            currency: "USD".into(),
            payment_reference: None,
            paid_at: None,
            period_start: None,
            period_end: None,
            payment_url: String::new(),
            invoice_path_pdf: String::new(),
            company_id: 1,
            billing_interval: String::new(),
            quantity: 0,
            price_per_user: 0.0,
            company_name: None,
            company_telp: None,
            company_address: Some(String::new()),
            user_name: None,
            user_username: None,
            items: vec![],
        };
        let d = build_pdf_data(&state, &inv);
        assert_eq!(d.currency.as_deref(), Some("IDR"));
        assert_eq!(d.customer_name, "-");
        assert_eq!(d.company_address, None);
        assert_eq!(d.billing_interval.as_deref(), Some("MONTH"));
        assert_eq!(d.quantity, Some(1.0));
        assert_eq!(d.payment_url, None);
        assert_eq!(d.tax_label, None);
        inv.payment_provider = "manual".into();
        let d = build_pdf_data(&state, &inv);
        assert_eq!(d.currency.as_deref(), Some("USD"));
        assert_eq!(d.billing_interval, None);
        assert_eq!(d.tax_label.as_deref(), Some("Tax (11%)"));
        assert!(d.payment_instructions.is_some());
        inv.status = "PAID".into();
        assert!(build_pdf_data(&state, &inv).payment_instructions.is_none());
    }
}
