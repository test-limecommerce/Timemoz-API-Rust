//! Uji paritas tata letak PDF invoice terhadap keluaran `pdfkit` 0.17.2 ASLI.
//!
//! `tests/golden/invoice_pdf_pdfkit.json` berisi masukan `InvoiceData` dan unsur halaman yang
//! diekstrak dari PDF hasil `InvoicePdfGenerator` Node (`invoice-pdf.generator.ts` dikompilasi
//! apa adanya): teks (isi, font, ukuran, x, baseline) dan persegi isi (`re`), per halaman.
//! Uji ini membangun PDF yang sama lewat Rust dan membandingkan unsur yang sama (toleransi 0,02 pt;
//! byte boleh beda). Cara memperbarui golden: lihat `plan/deviations/mz-billing.md`.

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::invoice_pdf_layout::{InvoiceData, InvoiceItem, generate};

fn dt(v: &Value) -> Option<NaiveDateTime> {
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.naive_utc())
}

fn st(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn data_from(c: &Value) -> InvoiceData {
    let f = |k: &str| c[k].as_f64().unwrap_or(0.0);
    InvoiceData {
        invoice_number: st(&c["invoiceNumber"]).unwrap_or_default(),
        status: st(&c["status"]).unwrap_or_default(),
        created_at: dt(&c["createdAt"]).unwrap_or_default(),
        expired_at: dt(&c["expiredAt"]).unwrap_or_default(),
        customer_name: st(&c["customerName"]).unwrap_or_default(),
        customer_email: st(&c["customerEmail"]).unwrap_or_default(),
        company_name: st(&c["companyName"]).unwrap_or_default(),
        company_phone: st(&c["companyPhone"]).unwrap_or_default(),
        company_address: st(&c["companyAddress"]),
        items: c["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|i| InvoiceItem {
                        description: st(&i["description"]).unwrap_or_default(),
                        quantity: i["quantity"].as_f64().unwrap_or(0.0),
                        unit_price: i["unitPrice"].as_f64().unwrap_or(0.0),
                        total_price: i["totalPrice"].as_f64().unwrap_or(0.0),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        sub_price: f("subPrice"),
        tax_price: f("taxPrice"),
        fee_price: f("feePrice"),
        total_price: f("totalPrice"),
        payment_provider: st(&c["paymentProvider"]).unwrap_or_default(),
        currency: st(&c["currency"]),
        billing_interval: st(&c["billingInterval"]),
        quantity: c["quantity"].as_f64(),
        price_per_user: c["pricePerUser"].as_f64(),
        payment_url: st(&c["paymentUrl"]),
        payment_reference: st(&c["paymentReference"]),
        paid_at: dt(&c["paidAt"]),
        period_start: dt(&c["periodStart"]),
        period_end: dt(&c["periodEnd"]),
        tax_label: st(&c["taxLabel"]),
        payment_instructions: st(&c["paymentInstructions"]),
    }
}

/// Unsur halaman dari aliran konten tak terkompresi: `("T", page, text, font, size, x, y)` dan
/// `("RECT", page, x, y, w, h)`.
fn extract(pdf: &[u8]) -> (usize, Vec<Value>) {
    let text = String::from_utf8_lossy(pdf).into_owned();
    // Peta font: /BaseFont per objek, lalu /F1, /F2.
    let mut items = Vec::new();
    let mut page = 0usize;
    let mut pages = 0usize;
    for chunk in text.split("endstream") {
        let Some(start) = chunk.find("stream\n") else {
            continue;
        };
        let body = &chunk[start + 7..];
        if !body.contains("1 0 0 -1 0 841.89 cm") {
            continue;
        }
        pages += 1;
        let lines: Vec<&str> = body.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let l = lines[i];
            if let Some(rest) = l.strip_suffix(" Tm") {
                let n: Vec<&str> = rest.split(' ').collect();
                let (x, y) = (n[4].parse::<f64>().unwrap(), n[5].parse::<f64>().unwrap());
                let tf: Vec<&str> = lines[i + 1].split(' ').collect();
                let font = if tf[0] == "/F1" {
                    "Helvetica"
                } else {
                    "Helvetica-Bold"
                };
                let size: f64 = tf[1].parse().unwrap();
                let tj = lines[i + 2];
                let mut s = String::new();
                let mut rest = tj;
                while let Some(a) = rest.find('<') {
                    let b = rest[a..].find('>').unwrap();
                    let hex = &rest[a + 1..a + b];
                    for k in (0..hex.len()).step_by(2) {
                        s.push(u8::from_str_radix(&hex[k..k + 2], 16).unwrap() as char);
                    }
                    rest = &rest[a + b + 1..];
                }
                items.push(serde_json::json!(["T", page, s, font, size, x, y]));
                i += 3;
                continue;
            }
            if let Some(rest) = l.strip_suffix(" re") {
                let n: Vec<f64> = rest.split(' ').map(|v| v.parse().unwrap()).collect();
                items.push(serde_json::json!(["RECT", page, n[0], n[1], n[2], n[3]]));
            }
            i += 1;
        }
        page += 1;
    }
    (pages, items)
}

fn close(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() <= 0.02,
        _ => a == b,
    }
}

#[test]
fn tata_letak_sama_dengan_pdfkit() {
    let golden: Value =
        serde_json::from_str(include_str!("../tests/golden/invoice_pdf_pdfkit.json"))
            .expect("golden JSON");
    let cases = golden.as_object().expect("objek");
    assert!(cases.len() >= 6);
    for (name, case) in cases {
        let pdf = generate(&data_from(&case["input"]));
        let (pages, got) = extract(&pdf);
        assert_eq!(
            Some(pages as u64),
            case["pages"].as_u64(),
            "{name}: jumlah halaman"
        );
        let want = case["expected"].as_array().expect("expected");
        // Python menaruh semua teks lebih dulu lalu persegi per halaman; bandingkan per jenis.
        for kind in ["T", "RECT"] {
            let pick = |v: &[Value]| -> Vec<Vec<Value>> {
                v.iter()
                    .filter_map(|x| x.as_array().filter(|a| a[0] == kind).cloned())
                    .collect()
            };
            let (g, w) = (pick(&got), pick(want));
            assert_eq!(g.len(), w.len(), "{name}/{kind}: jumlah unsur");
            for (idx, (g, w)) in g.iter().zip(&w).enumerate() {
                assert_eq!(g.len(), w.len(), "{name}/{kind}[{idx}] {w:?}");
                for (gv, wv) in g.iter().zip(w) {
                    assert!(
                        close(gv, wv),
                        "{name}/{kind}[{idx}]: rust={g:?} pdfkit={w:?}"
                    );
                }
            }
        }
    }
}
