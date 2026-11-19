//! Port `src/orders/order-invoice-pdf.generator.ts` — PDF invoice sales order (A4, pdfkit).
//! Penulis PDF = `mz_visit_forms::pdf::writer` (tiruan pdfkit 0.17: metrik AFM Helvetica, pembungkus
//! baris, koordinat dan operator yang sama). Masukan = detail order yang SUDAH disensor (kolom
//! tersensor = `null` → dicetak "-"). Order `delivered` ditagih dengan kolom `final*`.

use chrono::{DateTime, NaiveDate, Utc};
use mz_visit_forms::pdf::writer::{Align, Doc, Font, PAGE_H, PAGE_W, Rgb, TextOpts};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use std::str::FromStr;

/// `InvoicePdfItem`.
#[derive(Debug, Clone, Default)]
pub struct InvoiceItem {
    pub line_no: i64,
    pub product_sku: String,
    pub product_name: String,
    pub uom_code: String,
    pub qty: String,
    pub delivered_qty: Option<String>,
    pub unit_price: Option<String>,
    pub discount_value: Option<String>,
    pub tax_amount: Option<String>,
    pub line_total: Option<String>,
    pub is_free_good: bool,
}

/// `InvoicePdfData`.
#[derive(Debug, Clone, Default)]
pub struct InvoiceData {
    pub order_number: String,
    pub status: String,
    pub order_date: String,
    pub due_date: Option<String>,
    pub delivered_at: Option<i64>,
    pub currency_code: String,
    pub time_zone: String,
    pub customer_name: String,
    pub store_name: Option<String>,
    pub payment_term_name: Option<String>,
    pub payment_method_name: Option<String>,
    pub note: Option<String>,
    pub subtotal: Option<String>,
    pub discount_total: Option<String>,
    pub tax_total: Option<String>,
    pub grand_total: Option<String>,
    pub company_name: String,
    pub company_address: Option<String>,
    pub items: Vec<InvoiceItem>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn name_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|o| o.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Node: src/orders/order-invoice-pdf.generator.ts:323 `buildInvoicePdfData` — order `delivered`
/// beralih ke kolom `final*`; kolom tersensor tetap `null` (tidak jatuh ke sisi lain).
pub fn build_invoice_data(
    order: &Value,
    company_name: &str,
    company_address: Option<&str>,
    time_zone: &str,
) -> InvoiceData {
    let status = s(order, "status").unwrap_or_default();
    let delivered = status == "delivered";
    let pick = |a: &str, b: &str| s(order, if delivered { b } else { a });
    let items = order
        .get("items")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|i| InvoiceItem {
                    line_no: i.get("lineNo").and_then(Value::as_i64).unwrap_or(0),
                    product_sku: s(i, "productSku").unwrap_or_default(),
                    product_name: s(i, "productName").unwrap_or_default(),
                    uom_code: s(i, "uomCode").unwrap_or_default(),
                    qty: s(i, "qty").unwrap_or_default(),
                    delivered_qty: s(i, "deliveredQty"),
                    unit_price: s(i, "unitPrice"),
                    discount_value: s(
                        i,
                        if delivered {
                            "finalDiscountValue"
                        } else {
                            "discountValue"
                        },
                    ),
                    tax_amount: s(
                        i,
                        if delivered {
                            "finalTaxAmount"
                        } else {
                            "taxAmount"
                        },
                    ),
                    line_total: s(
                        i,
                        if delivered {
                            "finalLineTotal"
                        } else {
                            "lineTotal"
                        },
                    ),
                    is_free_good: i
                        .get("isFreeGood")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    InvoiceData {
        order_number: s(order, "orderNumber").unwrap_or_default(),
        status,
        order_date: s(order, "orderDate").unwrap_or_default(),
        due_date: s(order, "dueDate"),
        delivered_at: order.get("deliveredAt").and_then(Value::as_i64),
        currency_code: s(order, "currencyCode").unwrap_or_default(),
        time_zone: time_zone.to_owned(),
        customer_name: name_of(order, "customer").unwrap_or_default(),
        store_name: name_of(order, "store"),
        payment_term_name: name_of(order, "orderPaymentTerm"),
        payment_method_name: name_of(order, "orderPaymentMethod"),
        note: s(order, "note"),
        subtotal: pick("subtotal", "finalSubtotal"),
        discount_total: pick("discountTotal", "finalDiscountTotal"),
        tax_total: pick("taxTotal", "finalTaxTotal"),
        grand_total: pick("grandTotal", "finalGrandTotal"),
        company_name: company_name.to_owned(),
        company_address: company_address.map(str::to_owned),
        items,
    }
}

// ───────────────────────── format ─────────────────────────

/// `new Intl.NumberFormat('id-ID', { maximumFractionDigits }).format(Number(value))`.
pub fn id_number(value: &str, max_fraction: u32) -> Option<String> {
    let d = Decimal::from_str(value.trim()).ok()?;
    let r = d
        .round_dp_with_strategy(max_fraction, RoundingStrategy::MidpointAwayFromZero)
        .normalize();
    let neg = r.is_sign_negative() && !r.is_zero();
    let text = r.abs().to_string();
    let (int, frac) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&grouped);
    if !frac.is_empty() {
        out.push(',');
        out.push_str(frac);
    }
    Some(out)
}

/// `money(value, currency)`.
fn money(v: Option<&str>, currency: &str) -> String {
    v.and_then(|x| id_number(x, 2))
        .map_or_else(|| "-".to_owned(), |n| format!("{currency} {n}"))
}

/// `qty(value)`.
fn qty(v: Option<&str>) -> String {
    v.and_then(|x| id_number(x, 4))
        .unwrap_or_else(|| "-".to_owned())
}

/// `date(value)` — string `YYYY-MM-DD` apa adanya, epoch-ms di `timeZone`; format `DD MMM YYYY`.
fn date_str(v: Option<&str>) -> String {
    v.and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .map_or_else(|| "-".to_owned(), |d| d.format("%d %b %Y").to_string())
}

fn date_ms(v: i64, tz: &str) -> String {
    DateTime::<Utc>::from_timestamp_millis(v).map_or_else(
        || "-".to_owned(),
        |d| {
            d.with_timezone(&mz_core::tz::zone_or_utc(Some(tz)))
                .format("%d %b %Y")
                .to_string()
        },
    )
}

// ───────────────────────── tata letak ─────────────────────────

const MARGIN: f64 = 40.0;
const CONTENT_WIDTH: f64 = PAGE_W - MARGIN * 2.0;
const ROW_PADDING: f64 = 6.0;
const FOOTER_RESERVE: f64 = 40.0;

struct Colors;
impl Colors {
    fn primary() -> Rgb {
        Rgb::hex("#1a56db")
    }
    fn secondary() -> Rgb {
        Rgb::hex("#6b7280")
    }
    fn dark() -> Rgb {
        Rgb::hex("#1f2937")
    }
    fn light() -> Rgb {
        Rgb::hex("#f3f4f6")
    }
    fn line() -> Rgb {
        Rgb::hex("#e5e7eb")
    }
    fn white() -> Rgb {
        Rgb::hex("#ffffff")
    }
}

/// `COLS` — lebar kolom menjumlah `CONTENT_WIDTH` (515.28).
const COLS: [(&str, f64, Align); 6] = [
    ("No", 24.0, Align::Left),
    ("Item", 187.0, Align::Left),
    ("Qty", 58.0, Align::Right),
    ("Harga", 78.0, Align::Right),
    ("Diskon", 60.0, Align::Right),
    ("Total", 108.28, Align::Right),
];

fn opts(font: Font, size: f64, color: Rgb, width: f64, align: Align) -> TextOpts {
    TextOpts {
        font,
        size,
        color,
        width,
        align,
        line_gap: 0.0,
        height: None,
        ellipsis: false,
    }
}

/// Lebar bawaan pdfkit bila `width` tidak diberikan: `page.width - x - margins.right`.
fn default_width(x: f64) -> f64 {
    PAGE_W - x - MARGIN
}

struct Gen {
    doc: Doc,
    y: f64,
}

impl Gen {
    fn hline(&mut self) {
        self.doc.move_to(MARGIN, self.y);
        self.doc.line_to(PAGE_W - MARGIN, self.y);
        self.doc.stroke_color(Colors::line());
        self.doc.stroke();
    }

    fn header(&mut self, d: &InvoiceData) {
        let y = self.y;
        self.doc.text(
            "INVOICE",
            MARGIN,
            y,
            &opts(
                Font::Bold,
                22.0,
                Colors::primary(),
                default_width(MARGIN),
                Align::Left,
            ),
        );
        self.doc.text(
            &format!("#{}", d.order_number),
            MARGIN,
            y + 28.0,
            &opts(
                Font::Regular,
                10.0,
                Colors::secondary(),
                default_width(MARGIN),
                Align::Left,
            ),
        );
        let mut doc_y = self.doc.text(
            &d.company_name,
            MARGIN,
            y,
            &opts(
                Font::Bold,
                12.0,
                Colors::dark(),
                CONTENT_WIDTH,
                Align::Right,
            ),
        );
        if let Some(addr) = d.company_address.as_deref().filter(|a| !a.is_empty()) {
            doc_y = self.doc.text(
                addr,
                PAGE_W - MARGIN - 220.0,
                doc_y + 2.0,
                &opts(Font::Regular, 9.0, Colors::secondary(), 220.0, Align::Right),
            );
        }
        self.y = doc_y.max(y + 48.0) + 10.0;
        self.hline();
        self.y += 12.0;
    }

    fn field(&mut self, label: &str, value: &str, x: f64, y: f64, half: f64) -> f64 {
        let y1 = self.doc.text(
            &label.to_uppercase(),
            x,
            y,
            &opts(Font::Regular, 8.0, Colors::secondary(), half, Align::Left),
        );
        let y2 = self.doc.text(
            value,
            x,
            y1 + 1.0,
            &opts(Font::Bold, 10.0, Colors::dark(), half, Align::Left),
        );
        y2 + 6.0
    }

    fn parties(&mut self, d: &InvoiceData) {
        let half = CONTENT_WIDTH / 2.0 - 10.0;
        let top = self.y;
        let mut left = self.field("Ditagihkan kepada", &d.customer_name, MARGIN, top, half);
        if let Some(store) = d.store_name.as_deref().filter(|s| !s.is_empty()) {
            left = self.field("Toko", store, MARGIN, left, half);
        }
        let rx = MARGIN + half + 20.0;
        let mut right = self.field(
            "Tanggal order",
            &date_str(Some(&d.order_date)),
            rx,
            top,
            half,
        );
        right = self.field(
            "Jatuh tempo",
            &date_str(d.due_date.as_deref()),
            rx,
            right,
            half,
        );
        if let Some(at) = d.delivered_at {
            right = self.field("Tanggal kirim", &date_ms(at, &d.time_zone), rx, right, half);
        }
        if let Some(t) = d.payment_term_name.as_deref().filter(|s| !s.is_empty()) {
            right = self.field("Termin pembayaran", t, rx, right, half);
        }
        if let Some(m) = d.payment_method_name.as_deref().filter(|s| !s.is_empty()) {
            right = self.field("Metode pembayaran", m, rx, right, half);
        }
        self.y = left.max(right) + 8.0;
    }

    fn table_header(&mut self) {
        self.doc.rect(MARGIN, self.y, CONTENT_WIDTH, 20.0);
        self.doc.fill(Colors::primary());
        let mut x = MARGIN;
        for (label, width, align) in COLS {
            self.doc.text(
                label,
                x + 4.0,
                self.y + 6.0,
                &opts(Font::Bold, 9.0, Colors::white(), width - 8.0, align),
            );
            x += width;
        }
        self.y += 20.0;
    }

    fn table(&mut self, d: &InvoiceData) {
        let delivered = d.status == "delivered";
        self.table_header();
        for (idx, item) in d.items.iter().enumerate() {
            let qty_text = if delivered {
                format!(
                    "{} {}",
                    qty(item.delivered_qty.as_deref().or(Some(&item.qty))),
                    item.uom_code
                )
            } else {
                format!("{} {}", qty(Some(&item.qty)), item.uom_code)
            };
            let name = if item.is_free_good {
                format!("{} (Gratis)", item.product_name)
            } else {
                item.product_name.clone()
            };
            let texts = [
                item.line_no.to_string(),
                format!("{name}\n{}", item.product_sku),
                qty_text,
                money(item.unit_price.as_deref(), &d.currency_code),
                money(item.discount_value.as_deref(), &d.currency_code),
                money(item.line_total.as_deref(), &d.currency_code),
            ];
            let mut height: f64 = f64::NEG_INFINITY;
            for ((_, width, _), t) in COLS.iter().zip(texts.iter()) {
                height =
                    height.max(
                        self.doc
                            .height_of_string(t, Font::Regular, 9.0, width - 8.0, 0.0),
                    );
            }
            let height = height + ROW_PADDING * 2.0;
            if self.y + height > PAGE_H - MARGIN - FOOTER_RESERVE {
                self.doc.add_page();
                self.y = MARGIN;
                self.table_header();
            }
            if idx % 2 == 1 {
                self.doc.rect(MARGIN, self.y, CONTENT_WIDTH, height);
                self.doc.fill(Colors::light());
            }
            let mut x = MARGIN;
            for ((_, width, align), t) in COLS.iter().zip(texts.iter()) {
                self.doc.text(
                    t,
                    x + 4.0,
                    self.y + ROW_PADDING,
                    &opts(Font::Regular, 9.0, Colors::dark(), width - 8.0, *align),
                );
                x += width;
            }
            self.y += height;
        }
        self.hline();
        self.y += 12.0;
    }

    fn totals(&mut self, d: &InvoiceData) {
        let rows = [
            (
                "Subtotal",
                money(d.subtotal.as_deref(), &d.currency_code),
                false,
            ),
            (
                "Diskon",
                money(d.discount_total.as_deref(), &d.currency_code),
                false,
            ),
            (
                "Pajak",
                money(d.tax_total.as_deref(), &d.currency_code),
                false,
            ),
            (
                "Total",
                money(d.grand_total.as_deref(), &d.currency_code),
                true,
            ),
        ];
        if self.y + 4.0 * 20.0 > PAGE_H - MARGIN - FOOTER_RESERVE {
            self.doc.add_page();
            self.y = MARGIN;
        }
        let label_x = PAGE_W - MARGIN - 260.0;
        for (label, value, bold) in rows {
            let (font, size) = if bold {
                (Font::Bold, 11.0)
            } else {
                (Font::Regular, 10.0)
            };
            self.doc.text(
                label,
                label_x,
                self.y,
                &opts(font, size, Colors::dark(), 100.0, Align::Left),
            );
            self.doc.text(
                &value,
                label_x + 100.0,
                self.y,
                &opts(font, size, Colors::dark(), 160.0, Align::Right),
            );
            self.y += if bold { 22.0 } else { 17.0 };
        }
    }

    fn note(&mut self, d: &InvoiceData) {
        let Some(note) = d.note.as_deref().filter(|n| !n.is_empty()) else {
            return;
        };
        self.y += 8.0;
        if self.y + 50.0 > PAGE_H - MARGIN - FOOTER_RESERVE {
            self.doc.add_page();
            self.y = MARGIN;
        }
        let y = self.doc.text(
            "CATATAN",
            MARGIN,
            self.y,
            &opts(
                Font::Bold,
                9.0,
                Colors::secondary(),
                default_width(MARGIN),
                Align::Left,
            ),
        );
        self.doc.text(
            note,
            MARGIN,
            y + 2.0,
            &opts(
                Font::Regular,
                9.0,
                Colors::dark(),
                CONTENT_WIDTH,
                Align::Left,
            ),
        );
    }

    /// Nomor halaman dicap sesudah tata letak final (`bufferPages`).
    fn footers(&mut self) {
        let count = self.doc.page_count();
        for i in 0..count {
            self.doc.switch_to_page(i);
            self.doc.set_page_margins(MARGIN, 0.0);
            self.doc.text(
                &format!("Halaman {} dari {count}", i + 1),
                MARGIN,
                PAGE_H - 30.0,
                &opts(
                    Font::Regular,
                    8.0,
                    Colors::secondary(),
                    CONTENT_WIDTH,
                    Align::Center,
                ),
            );
            self.doc.set_page_margins(MARGIN, MARGIN);
        }
    }
}

/// Node: src/orders/order-invoice-pdf.generator.ts:77 `generate`.
pub fn generate(d: &InvoiceData) -> Vec<u8> {
    let mut doc = Doc::new(MARGIN, MARGIN);
    doc.set_info("Creator", "PDFKit".to_owned());
    doc.set_info(
        "CreationDate",
        format!("D:{}Z", Utc::now().format("%Y%m%d%H%M%S")),
    );
    let mut g = Gen { doc, y: MARGIN };
    g.header(d);
    g.parties(d);
    g.table(d);
    g.totals(d);
    g.note(d);
    g.footers();
    g.doc.finish(true)
}

/// `invoice-<orderNumber>.pdf` (karakter selain `[A-Za-z0-9._-]` → `_`).
pub fn filename(order_number: &str) -> String {
    let safe: String = order_number
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("invoice-{safe}.pdf")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(n: i64, over: Value) -> Value {
        let mut base = json!({
            "lineNo": n, "productSku": format!("SKU-{n}"), "productName": format!("Produk {n}"), "uomCode": "PCS",
            "qty": "10", "deliveredQty": null, "unitPrice": "1000", "discountValue": "0", "taxAmount": "100",
            "lineTotal": "10100", "finalDiscountValue": null, "finalTaxAmount": null, "finalLineTotal": null,
            "isFreeGood": false,
        });
        if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn order(over: Value) -> Value {
        let mut base = json!({
            "orderNumber": "SO/2026/0001", "status": "approved", "orderDate": "2026-10-01", "dueDate": "2026-10-31",
            "deliveredAt": null, "currencyCode": "IDR", "customer": {"name": "PT Maju"}, "store": null,
            "orderPaymentTerm": null, "orderPaymentMethod": null, "note": null, "subtotal": "10000",
            "discountTotal": "0", "taxTotal": "100", "grandTotal": "10100", "finalSubtotal": "5000",
            "finalDiscountTotal": "0", "finalTaxTotal": "50", "finalGrandTotal": "5050", "items": [item(1, json!({}))],
        });
        if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    // Node: src/orders/order-invoice-pdf.generator.spec.ts
    #[test]
    fn ordered_vs_final_amounts() {
        assert_eq!(
            build_invoice_data(&order(json!({})), "Acme", None, "Asia/Jakarta")
                .grand_total
                .as_deref(),
            Some("10100")
        );
        let delivered = build_invoice_data(
            &order(
                json!({"status": "delivered", "items": [item(1, json!({"finalLineTotal": "5050"}))]}),
            ),
            "Acme",
            None,
            "Asia/Jakarta",
        );
        assert_eq!(delivered.grand_total.as_deref(), Some("5050"));
        assert_eq!(delivered.items[0].line_total.as_deref(), Some("5050"));
        assert_eq!(
            build_invoice_data(&order(json!({"grandTotal": null})), "Acme", None, "UTC")
                .grand_total,
            None
        );
    }

    #[test]
    fn renders_single_and_multi_page() {
        let small = generate(&build_invoice_data(
            &order(json!({})),
            "Acme",
            None,
            "Asia/Jakarta",
        ));
        assert_eq!(&small[..5], b"%PDF-");
        let items: Vec<Value> = (1..=120)
            .map(|i| item(i, json!({"unitPrice": null, "lineTotal": null})))
            .collect();
        let big_data = build_invoice_data(
            &order(json!({"items": items, "note": "Catatan"})),
            "Acme",
            Some("Jl. A"),
            "Asia/Jakarta",
        );
        let big = generate(&big_data);
        assert_eq!(&big[..5], b"%PDF-");
        assert!(big.len() > small.len());
    }

    #[test]
    fn formats() {
        assert_eq!(id_number("12500.5", 2).as_deref(), Some("12.500,5"));
        assert_eq!(id_number("1234567", 2).as_deref(), Some("1.234.567"));
        assert_eq!(id_number("0.125", 2).as_deref(), Some("0,13"));
        assert_eq!(id_number("2.33333", 4).as_deref(), Some("2,3333"));
        assert_eq!(money(None, "IDR"), "-");
        assert_eq!(money(Some("10100"), "IDR"), "IDR 10.100");
        assert_eq!(date_str(Some("2026-10-01")), "01 Oct 2026");
        assert_eq!(filename("SO/2026/0001"), "invoice-SO_2026_0001.pdf");
    }

    /// Pembanding manual dengan pdfkit: `MZ_ORDERS_PDF_IN=<order.json> MZ_ORDERS_PDF_OUT=<out.pdf>
    /// cargo test -p mz-orders --lib dump_pdf -- --ignored`.
    #[test]
    #[ignore = "alat banding manual"]
    fn dump_pdf() {
        let (Ok(input), Ok(out)) = (
            std::env::var("MZ_ORDERS_PDF_IN"),
            std::env::var("MZ_ORDERS_PDF_OUT"),
        ) else {
            return;
        };
        let order: Value = serde_json::from_str(&std::fs::read_to_string(input).unwrap()).unwrap();
        let data = build_invoice_data(
            &order,
            "Acme Distribusi",
            Some("Jl. Sudirman No. 1\nJakarta Pusat"),
            "Asia/Jakarta",
        );
        std::fs::write(out, generate(&data)).unwrap();
    }
}
