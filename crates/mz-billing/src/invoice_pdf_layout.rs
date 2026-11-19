//! Port `src/invoices/invoice-pdf.generator.ts` (Mudah-API): PDF invoice ala pdfkit.
//!
//! Setiap method Node (`drawHeader`, `drawBillingInfo`, `drawItemsTable`, `drawSummary`,
//! `drawPaymentInfo`, `drawFooter`) ada padanannya di bawah dengan koordinat yang sama;
//! lihat `crate::pdf` untuk perilaku pdfkit yang ditiru (kerning, pembungkusan, halaman baru).

use chrono::{Datelike, NaiveDateTime};
use mz_core::jsfmt::{num_string, to_fixed};

use crate::pdf::{Align, Doc, Font, MARGIN, PAGE_H, PAGE_W, Rgb, TextOpts, logo};

/// Node: invoice-pdf.generator.ts:20 (`InvoiceItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceItem {
    pub description: String,
    pub quantity: f64,
    pub unit_price: f64,
    pub total_price: f64,
}

/// Node: invoice-pdf.generator.ts:26 (`InvoiceData`). `Option<String>` kosong = "falsy" di JS.
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceData {
    pub invoice_number: String,
    pub status: String,
    pub created_at: NaiveDateTime,
    pub expired_at: NaiveDateTime,
    pub customer_name: String,
    pub customer_email: String,
    pub company_name: String,
    pub company_phone: String,
    pub company_address: Option<String>,
    pub items: Vec<InvoiceItem>,
    pub sub_price: f64,
    pub tax_price: f64,
    pub fee_price: f64,
    pub total_price: f64,
    pub payment_provider: String,
    /// ISO 4217; `None`/kosong = tebak dari provider (invoice lama).
    pub currency: Option<String>,
    pub billing_interval: Option<String>,
    pub quantity: Option<f64>,
    pub price_per_user: Option<f64>,
    pub payment_url: Option<String>,
    pub payment_reference: Option<String>,
    pub paid_at: Option<NaiveDateTime>,
    pub period_start: Option<NaiveDateTime>,
    pub period_end: Option<NaiveDateTime>,
    /// Label baris pajak; bawaan `Tax (11%)`.
    pub tax_label: Option<String>,
    pub payment_instructions: Option<String>,
}

/// Node: invoice-pdf.generator.ts:72 (`formatInvoiceCurrency`). IDR tanpa desimal dengan
/// pemisah titik (`Rp 15.000.000`); selain itu `$12.50`. Tanpa `currency`: `paypal` = USD,
/// lainnya = IDR.
pub fn format_invoice_currency(
    amount: f64,
    currency: Option<&str>,
    provider: Option<&str>,
) -> String {
    let code = currency
        .filter(|c| !c.is_empty())
        .unwrap_or(if provider == Some("paypal") {
            "USD"
        } else {
            "IDR"
        })
        .to_uppercase();
    if code == "IDR" {
        // `Math.abs(Math.round(amount)).toString()`
        let rounded = (amount + 0.5).floor().abs();
        let digits = format!("{rounded:.0}");
        let n = digits.chars().count();
        let mut whole = String::new();
        for (k, ch) in digits.chars().enumerate() {
            if k > 0 && (n - k) % 3 == 0 {
                whole.push('.');
            }
            whole.push(ch);
        }
        if amount < 0.0 {
            whole = format!("-{whole}");
        }
        return format!("Rp {whole}");
    }
    format!("${}", to_fixed(amount, 2))
}

/// Node: invoice-pdf.generator.ts:94 (`invoiceProviderLabel`).
pub fn invoice_provider_label(provider: &str) -> String {
    if provider.eq_ignore_ascii_case("manual") {
        return "Bank transfer / manual".to_owned();
    }
    provider.to_uppercase()
}

struct Colors;

impl Colors {
    fn primary() -> Rgb {
        Rgb::hex("#1a56db")
    }
    fn secondary() -> Rgb {
        Rgb::hex("#6b7280")
    }
    fn success() -> Rgb {
        Rgb::hex("#059669")
    }
    fn warning() -> Rgb {
        Rgb::hex("#d97706")
    }
    fn danger() -> Rgb {
        Rgb::hex("#dc2626")
    }
    fn dark() -> Rgb {
        Rgb::hex("#1f2937")
    }
    fn light() -> Rgb {
        Rgb::hex("#f3f4f6")
    }
    fn white() -> Rgb {
        Rgb::hex("#ffffff")
    }
}

const CONTENT_WIDTH: f64 = PAGE_W - MARGIN * 2.0;

/// `getStatusColor`.
fn status_color(status: &str) -> Rgb {
    match status.to_uppercase().as_str() {
        "PAID" => Colors::success(),
        "UNPAID" => Colors::warning(),
        "CANCELLED" | "EXPIRED" => Colors::danger(),
        _ => Colors::secondary(),
    }
}

/// `formatDate`: `DD/MM/YYYY` (tanggal UTC; produksi TZ=UTC).
fn format_date(d: NaiveDateTime) -> String {
    format!("{:02}/{:02}/{}", d.day(), d.month(), d.year())
}

fn fmt(amount: f64, d: &InvoiceData) -> String {
    format_invoice_currency(amount, d.currency.as_deref(), Some(&d.payment_provider))
}

fn truthy(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.is_empty())
}

/// Node: invoice-pdf.generator.ts:128 (`InvoicePdfGenerator.generate`).
pub fn generate(data: &InvoiceData) -> Vec<u8> {
    let mut doc = Doc::new();
    let mut current_y = MARGIN;
    current_y = draw_header(&mut doc, data, current_y);
    current_y = draw_billing_info(&mut doc, data, current_y);
    current_y = draw_items_table(&mut doc, data, current_y);
    current_y = draw_summary(&mut doc, data, current_y);
    let _ = draw_payment_info(&mut doc, data, current_y);
    draw_footer(&mut doc);
    doc.finish()
}

fn draw_header(doc: &mut Doc, data: &InvoiceData, start_y: f64) -> f64 {
    let right_width = 220.0;
    let right_x = PAGE_W - MARGIN - right_width;

    // Kiri: logo + kontak Timemoz
    if let Some(img) = logo() {
        doc.image(img, MARGIN, start_y + 2.0, 140.0);
    } else {
        doc.text_at(
            "Timemoz",
            MARGIN,
            start_y,
            Font::Bold,
            22.0,
            Colors::primary(),
        );
    }
    doc.text_at(
        "www.timemoz.com  |  support@timemoz.com",
        MARGIN,
        start_y + 42.0,
        Font::Regular,
        8.5,
        Colors::secondary(),
    );

    // Kanan: judul, nomor, status, tanggal
    doc.text(
        "INVOICE",
        right_x,
        start_y,
        Font::Bold,
        22.0,
        Colors::dark(),
        &TextOpts::aligned(right_width, Align::Right),
    );
    doc.text(
        &format!("#{}", data.invoice_number),
        right_x,
        start_y + 27.0,
        Font::Regular,
        10.0,
        Colors::secondary(),
        &TextOpts::aligned(right_width, Align::Right),
    );

    let badge_width = 90.0;
    let badge_x = PAGE_W - MARGIN - badge_width;
    doc.fill_round_rect(
        badge_x,
        start_y + 45.0,
        badge_width,
        20.0,
        4.0,
        status_color(&data.status),
    );
    doc.text(
        &data.status,
        badge_x,
        start_y + 51.0,
        Font::Bold,
        9.0,
        Colors::white(),
        &TextOpts::aligned(badge_width, Align::Center),
    );

    let mut meta_y = start_y + 76.0;
    let mut meta = |doc: &mut Doc, label: &str, value: String| {
        doc.text(
            label,
            right_x,
            meta_y,
            Font::Regular,
            9.0,
            Colors::secondary(),
            &TextOpts::aligned(right_width - 85.0, Align::Right),
        );
        doc.text(
            &value,
            right_x + right_width - 80.0,
            meta_y,
            Font::Bold,
            9.0,
            Colors::dark(),
            &TextOpts::aligned(80.0, Align::Right),
        );
        meta_y += 14.0;
    };
    meta(doc, "Issue date", format_date(data.created_at));
    if data.status != "PAID" {
        meta(doc, "Due date", format_date(data.expired_at));
    }

    // Divider line
    let current_y = meta_y.max(start_y + 100.0) + 8.0;
    doc.line(
        MARGIN,
        current_y,
        PAGE_W - MARGIN,
        current_y,
        Colors::light(),
        1.0,
    );
    current_y + 20.0
}

fn draw_billing_info(doc: &mut Doc, data: &InvoiceData, start_y: f64) -> f64 {
    let col_width = CONTENT_WIDTH / 2.0;

    // Bill To
    doc.text_at(
        "BILL TO",
        MARGIN,
        start_y,
        Font::Bold,
        9.0,
        Colors::secondary(),
    );
    doc.text_at(
        &data.customer_name,
        MARGIN,
        start_y + 15.0,
        Font::Bold,
        11.0,
        Colors::dark(),
    );
    doc.text_at(
        &data.customer_email,
        MARGIN,
        start_y + 30.0,
        Font::Regular,
        10.0,
        Colors::secondary(),
    );

    // Company Info (right side)
    let right_x = MARGIN + col_width + 20.0;
    doc.text_at(
        "COMPANY",
        right_x,
        start_y,
        Font::Bold,
        9.0,
        Colors::secondary(),
    );
    doc.text_at(
        &data.company_name,
        right_x,
        start_y + 15.0,
        Font::Bold,
        11.0,
        Colors::dark(),
    );
    let phone = if data.company_phone.is_empty() {
        "-"
    } else {
        &data.company_phone
    };
    doc.text_at(
        phone,
        right_x,
        start_y + 30.0,
        Font::Regular,
        10.0,
        Colors::secondary(),
    );
    if let Some(addr) = truthy(&data.company_address) {
        doc.text(
            addr,
            right_x,
            start_y + 45.0,
            Font::Regular,
            10.0,
            Colors::secondary(),
            &TextOpts::width(col_width - 20.0),
        );
    }

    // Payment provider badge
    if !data.payment_provider.is_empty() {
        let badge_y = start_y + 55.0;
        let provider_text = invoice_provider_label(&data.payment_provider);
        let badge_w = if provider_text.encode_utf16().count() > 8 {
            130.0
        } else {
            60.0
        };
        doc.fill_round_rect(MARGIN, badge_y, badge_w, 18.0, 3.0, Colors::primary());
        doc.text(
            &provider_text,
            MARGIN,
            badge_y + 5.0,
            Font::Bold,
            8.0,
            Colors::white(),
            &TextOpts::aligned(badge_w, Align::Center),
        );

        // Billing interval if present
        if let Some(interval) = truthy(&data.billing_interval) {
            let interval_text = if interval == "YEAR" {
                "YEARLY"
            } else {
                "MONTHLY"
            };
            doc.fill_round_rect(
                MARGIN + badge_w + 5.0,
                badge_y,
                60.0,
                18.0,
                3.0,
                Colors::success(),
            );
            doc.text(
                interval_text,
                MARGIN + badge_w + 5.0,
                badge_y + 5.0,
                Font::Bold,
                8.0,
                Colors::white(),
                &TextOpts::aligned(60.0, Align::Center),
            );
        }
    }

    start_y + 90.0
}

fn draw_items_table(doc: &mut Doc, data: &InvoiceData, start_y: f64) -> f64 {
    let w_desc = CONTENT_WIDTH * 0.5;
    let w_qty = CONTENT_WIDTH * 0.15;
    let w_unit = CONTENT_WIDTH * 0.175;
    let w_total = CONTENT_WIDTH * 0.175;

    // Table header
    doc.fill_rect(MARGIN, start_y, CONTENT_WIDTH, 35.0, Colors::dark());
    let mut header_x = MARGIN + 10.0;
    let (hf, hs, hc) = (Font::Bold, 9.0, Colors::white());
    doc.text_at("DESCRIPTION", header_x, start_y + 13.0, hf, hs, hc);
    header_x += w_desc;
    doc.text(
        "QTY",
        header_x,
        start_y + 13.0,
        hf,
        hs,
        hc,
        &TextOpts::aligned(w_qty, Align::Center),
    );
    header_x += w_qty;
    doc.text(
        "UNIT PRICE",
        header_x,
        start_y + 13.0,
        hf,
        hs,
        hc,
        &TextOpts::aligned(w_unit, Align::Right),
    );
    header_x += w_unit;
    doc.text(
        "TOTAL",
        header_x,
        start_y + 13.0,
        hf,
        hs,
        hc,
        &TextOpts::aligned(w_total - 20.0, Align::Right),
    );

    // Table rows
    let mut row_y = start_y + 35.0;
    for (index, item) in data.items.iter().enumerate() {
        if index % 2 == 0 {
            doc.fill_rect(MARGIN, row_y, CONTENT_WIDTH, 40.0, Colors::light());
        }
        let mut cell_x = MARGIN + 10.0;
        let (f, s, c) = (Font::Regular, 10.0, Colors::dark());

        // Description - support multi-line
        doc.text(
            &item.description,
            cell_x,
            row_y + 12.0,
            f,
            s,
            c,
            &TextOpts {
                width: Some(w_desc - 10.0),
                height: Some(30.0),
                ..TextOpts::default()
            },
        );
        cell_x += w_desc;
        doc.text(
            &num_string(item.quantity),
            cell_x,
            row_y + 12.0,
            f,
            s,
            c,
            &TextOpts::aligned(w_qty, Align::Center),
        );
        cell_x += w_qty;
        doc.text(
            &fmt(item.unit_price, data),
            cell_x,
            row_y + 12.0,
            f,
            s,
            c,
            &TextOpts::aligned(w_unit, Align::Right),
        );
        cell_x += w_unit;
        doc.text(
            &fmt(item.total_price, data),
            cell_x,
            row_y + 12.0,
            Font::Bold,
            s,
            c,
            &TextOpts::aligned(w_total - 20.0, Align::Right),
        );
        row_y += 40.0;
    }

    row_y + 10.0
}

fn draw_summary(doc: &mut Doc, data: &InvoiceData, start_y: f64) -> f64 {
    let summary_width = 220.0;
    let summary_x = PAGE_W - MARGIN - summary_width;

    // Summary box: tinggi mengikuti jumlah baris (subtotal, pajak, fee?, total)
    let rows = if data.fee_price > 0.0 { 3.0 } else { 2.0 };
    let box_height = 15.0 + rows * 22.0 + 12.0 + 30.0;
    doc.fill_rect(
        summary_x,
        start_y,
        summary_width,
        box_height,
        Colors::light(),
    );

    let mut row_y = start_y + 15.0;
    let label_x = summary_x + 15.0;
    let value_opts = TextOpts::aligned(summary_width - 30.0, Align::Right);
    let (f, s) = (Font::Regular, 10.0);

    let line = |doc: &mut Doc, label: &str, value: f64, y: f64| {
        doc.text(
            label,
            label_x,
            y,
            f,
            s,
            Colors::secondary(),
            &TextOpts::default(),
        );
        doc.text(
            &fmt(value, data),
            label_x,
            y,
            f,
            s,
            Colors::dark(),
            &value_opts,
        );
    };
    line(doc, "Subtotal", data.sub_price, row_y);
    row_y += 22.0;
    line(
        doc,
        truthy(&data.tax_label).unwrap_or("Tax (11%)"),
        data.tax_price,
        row_y,
    );
    row_y += 22.0;
    if data.fee_price > 0.0 {
        line(doc, "Fee", data.fee_price, row_y);
        row_y += 22.0;
    }

    // Total line
    doc.line(
        label_x,
        row_y,
        summary_x + summary_width - 15.0,
        row_y,
        Colors::primary(),
        2.0,
    );
    row_y += 12.0;

    // Total
    doc.text_at("TOTAL", label_x, row_y, Font::Bold, 12.0, Colors::primary());
    doc.text(
        &fmt(data.total_price, data),
        label_x,
        row_y - 2.0,
        Font::Bold,
        14.0,
        Colors::primary(),
        &value_opts,
    );

    // Quantity info (left side)
    if let (Some(q), Some(p)) = (data.quantity, data.price_per_user)
        && q != 0.0
        && p != 0.0
    {
        let yearly = data.billing_interval.as_deref() == Some("YEAR");
        doc.text_at(
            "Subscription Details:",
            MARGIN,
            start_y,
            Font::Regular,
            10.0,
            Colors::secondary(),
        );
        doc.text_at(
            &format!(
                "{} user(s) @ {}/user{}",
                num_string(q),
                fmt(p, data),
                if yearly { "/month" } else { "" }
            ),
            MARGIN,
            start_y + 18.0,
            Font::Bold,
            11.0,
            Colors::dark(),
        );
        if yearly {
            doc.text_at(
                "Billed annually - Save up to 40%!",
                MARGIN,
                start_y + 35.0,
                Font::Regular,
                9.0,
                Colors::success(),
            );
        }
    }

    start_y + box_height.max(60.0) + 20.0
}

/// `label` (regular, abu) lalu ` value` (bold, gelap) pada baris yang sama
/// (`text(label, x, y, { continued: true })` + `.text(' value')`).
fn continued(doc: &mut Doc, label: &str, value: &str, x: f64, y: f64) {
    let lw = doc.text(
        label,
        x,
        y,
        Font::Regular,
        9.0,
        Colors::secondary(),
        &TextOpts::default(),
    );
    doc.text(
        &format!(" {value}"),
        x,
        y,
        Font::Bold,
        9.0,
        Colors::dark(),
        &TextOpts {
            indent_first: lw,
            ..TextOpts::default()
        },
    );
}

fn draw_payment_info(doc: &mut Doc, data: &InvoiceData, current_y: f64) -> f64 {
    if data.status == "PAID" {
        // Paid stamp
        let stamp_x = MARGIN + 30.0;
        let stamp_y = current_y;
        doc.save();
        doc.rotate(-15.0, stamp_x + 50.0, stamp_y + 25.0);
        doc.stroke_round_rect(stamp_x, stamp_y, 100.0, 50.0, 5.0, 3.0, Colors::success());
        doc.text(
            "PAID",
            stamp_x,
            stamp_y + 15.0,
            Font::Bold,
            20.0,
            Colors::success(),
            &TextOpts::aligned(100.0, Align::Center),
        );
        doc.restore();

        // Detail pembayaran (invoice manual): tanggal bayar, referensi, periode.
        let detail_x = MARGIN + 160.0;
        let mut detail_y = stamp_y;
        if let Some(paid) = data.paid_at {
            continued(doc, "Paid on:", &format_date(paid), detail_x, detail_y);
            detail_y += 14.0;
        }
        if let Some(r) = truthy(&data.payment_reference) {
            continued(doc, "Payment reference:", r, detail_x, detail_y);
            detail_y += 14.0;
        }
        if let (Some(a), Some(b)) = (data.period_start, data.period_end) {
            continued(
                doc,
                "Covered period:",
                &format!("{} - {}", format_date(a), format_date(b)),
                detail_x,
                detail_y,
            );
        }
    } else if let Some(instructions) = truthy(&data.payment_instructions) {
        // Invoice manual belum dibayar: periode tagihan + cara membayar.
        let info_y = current_y + 10.0;
        let mut line_y = info_y;
        if let (Some(a), Some(b)) = (data.period_start, data.period_end) {
            continued(
                doc,
                "Billing period:",
                &format!("{} - {}", format_date(a), format_date(b)),
                MARGIN,
                line_y,
            );
            line_y += 16.0;
        }
        doc.text_at(
            "Payment instructions:",
            MARGIN,
            line_y,
            Font::Regular,
            10.0,
            Colors::secondary(),
        );
        doc.text(
            instructions,
            MARGIN,
            line_y + 15.0,
            Font::Regular,
            9.0,
            Colors::dark(),
            &TextOpts::width(CONTENT_WIDTH),
        );
    } else if let Some(url) = truthy(&data.payment_url) {
        // Payment button area
        let btn_y = current_y + 10.0;
        doc.text_at(
            "Complete your payment:",
            MARGIN,
            btn_y,
            Font::Regular,
            10.0,
            Colors::secondary(),
        );
        doc.text(
            url,
            MARGIN,
            btn_y + 15.0,
            Font::Regular,
            9.0,
            Colors::primary(),
            &TextOpts {
                width: Some(CONTENT_WIDTH),
                link: Some(url.to_owned()),
                underline: true,
                ..TextOpts::default()
            },
        );
    }

    current_y + 80.0
}

/// Footer di dasar halaman terakhir; margin bawah dinolkan sementara (lihat komentar Node).
fn draw_footer(doc: &mut Doc) {
    let footer_y = PAGE_H - 72.0;
    let bottom = doc.set_margin_bottom(0.0);
    doc.line(
        MARGIN,
        footer_y,
        PAGE_W - MARGIN,
        footer_y,
        Colors::light(),
        1.0,
    );
    let mut line = |text: &str, y: f64, size: f64| {
        doc.text(
            text,
            MARGIN,
            y,
            Font::Regular,
            size,
            Colors::secondary(),
            &TextOpts::aligned(CONTENT_WIDTH, Align::Center),
        );
    };
    line("Thank you for your business!", footer_y + 12.0, 9.0);
    line(
        "This is a computer-generated document. No signature is required.",
        footer_y + 27.0,
        8.0,
    );
    line(
        "Timemoz | www.timemoz.com | support@timemoz.com",
        footer_y + 41.0,
        8.0,
    );
    doc.set_margin_bottom(bottom);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_dan_usd() {
        assert_eq!(
            format_invoice_currency(15_000_000.0, Some("IDR"), None),
            "Rp 15.000.000"
        );
        assert_eq!(format_invoice_currency(999.0, Some("IDR"), None), "Rp 999");
        assert_eq!(format_invoice_currency(12.5, Some("USD"), None), "$12.50");
        assert_eq!(format_invoice_currency(5.0, None, Some("paypal")), "$5.00");
        assert_eq!(
            format_invoice_currency(5000.0, None, Some("midtrans")),
            "Rp 5.000"
        );
        assert_eq!(
            format_invoice_currency(-1234.5, Some("idr"), None),
            "Rp -1.234"
        );
        assert_eq!(format_invoice_currency(0.125, Some("USD"), None), "$0.13");
        assert_eq!(
            format_invoice_currency(1234.0, Some(""), Some("manual")),
            "Rp 1.234"
        );
    }

    #[test]
    fn label_provider() {
        assert_eq!(invoice_provider_label("manual"), "Bank transfer / manual");
        assert_eq!(invoice_provider_label("paypal"), "PAYPAL");
    }
}
