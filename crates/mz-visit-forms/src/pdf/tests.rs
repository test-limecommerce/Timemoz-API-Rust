//! Port `form-submission-pdf.generator.spec.ts` + uji teks tercetak (content stream tanpa
//! kompresi, lewat [`super::render_doc`]).

use super::writer::extract_fragments;
use super::*;
use chrono::TimeZone;
use serde_json::json;

/// PNG 1×1 abu-abu + alfa (sama dengan `TINY_PNG` spec Node).
fn tiny_png() -> Vec<u8> {
    const B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    decode_b64(B64)
}

fn decode_b64(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let Some(v) = T.iter().position(|t| *t == c) else {
            continue;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

fn media(png: &[u8]) -> PdfMedia {
    PdfMedia {
        buffer: png.to_vec(),
        content_type: "image/png".to_owned(),
    }
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn full_schema() -> Value {
    json!({
      "schemaVersion": 1,
      "fields": [
        { "key": "judul", "type": "heading", "label": "Laporan Kunjungan" },
        { "key": "pengantar", "type": "paragraph", "label": "Isi form berikut dengan lengkap." },
        { "key": "pemisah", "type": "divider" },
        { "key": "identitas", "type": "section", "label": "Identitas", "fields": [
            { "key": "nama", "type": "text", "label": "Nama", "required": true },
            { "key": "surel", "type": "email", "label": "Email" },
            { "key": "telepon", "type": "phone", "label": "Telepon" },
            { "key": "situs", "type": "url", "label": "Situs" },
            { "key": "catatan", "type": "textarea", "label": "Catatan" },
            { "key": "kode", "type": "barcode", "label": "Kode" }
        ]},
        { "key": "angka", "type": "group", "label": "Angka", "fields": [
            { "key": "jumlah", "type": "number", "label": "Jumlah" },
            { "key": "harga", "type": "currency", "label": "Harga", "config": { "currencyCode": "IDR" } },
            { "key": "diskon", "type": "percent", "label": "Diskon" },
            { "key": "rating", "type": "rating", "label": "Rating", "config": { "max": 5 } }
        ]},
        { "key": "waktu", "type": "group", "label": "Waktu", "fields": [
            { "key": "tanggal", "type": "date", "label": "Tanggal" },
            { "key": "jam", "type": "time", "label": "Jam" },
            { "key": "stempel", "type": "datetime", "label": "Stempel" }
        ]},
        { "key": "pilihan", "type": "group", "label": "Pilihan", "fields": [
            { "key": "status", "type": "select", "label": "Status", "options": { "source": "static", "items": [
                { "value": "ok", "label": "OK" }, { "value": "perlu_tindak_lanjut", "label": "Perlu tindak lanjut" } ] } },
            { "key": "grade", "type": "radio", "label": "Grade", "options": { "source": "static", "items": [
                { "value": "a", "label": "A" }, { "value": "b", "label": "B" } ] } },
            { "key": "tag", "type": "multiselect", "label": "Tag", "options": { "source": "static", "items": [
                { "value": "x", "label": "X" }, { "value": "y", "label": "Y" } ] } },
            { "key": "centang", "type": "checkbox", "label": "Centang", "options": { "source": "static", "items": [
                { "value": "p", "label": "P" }, { "value": "q", "label": "Q" } ] } },
            { "key": "setuju", "type": "boolean", "label": "Setuju" }
        ]},
        { "key": "media_konteks", "type": "group", "label": "Media & konteks", "fields": [
            { "key": "ttd", "type": "signature", "label": "Tanda tangan" },
            { "key": "foto", "type": "photo", "label": "Foto" },
            { "key": "foto_hilang", "type": "photo", "label": "Foto hilang" },
            { "key": "lampiran", "type": "file", "label": "Lampiran" },
            { "key": "lokasi", "type": "location", "label": "Lokasi" }
        ]},
        { "key": "gate", "type": "boolean", "label": "Ada catatan tambahan?" },
        { "key": "catatan_tambahan", "type": "textarea", "label": "Catatan tambahan",
          "visibleIf": { "all": [ { "field": "gate", "op": "eq", "value": true } ] } },
        { "key": "unit_list", "type": "repeater", "label": "Unit", "fields": [
            { "key": "unit", "type": "text", "label": "Unit" },
            { "key": "qty", "type": "number", "label": "Qty" },
            { "key": "foto_unit", "type": "photo", "label": "Foto unit" }
        ]}
      ]
    })
}

fn full_answers() -> Map<String, Value> {
    json!({
      "nama": "Yudha Pratama",
      "surel": "yudha@example.com",
      "telepon": "081234567890",
      "situs": "https://example.com",
      "catatan": "Kunjungan berjalan lancar.",
      "kode": "BC-12345",
      "angka": { "jumlah": 42, "harga": 150000, "diskon": 10, "rating": 4 },
      "waktu": { "tanggal": "2026-08-20", "jam": "14:30", "stempel": 1_787_236_200_000_i64 },
      "pilihan": { "status": "ok", "grade": "a", "tag": ["x", "y"], "centang": ["p"], "setuju": true },
      "media_konteks": {
        "ttd": { "fileUuid": "signature-1" },
        "foto": { "fileUuid": "photo-1" },
        "foto_hilang": { "fileUuid": "missing-photo" },
        "lampiran": { "fileUuid": "file-1", "contentType": "application/pdf", "sizeBytes": 20480 },
        "lokasi": { "lat": -6.2, "lng": 106.8166, "accuracy": 12 }
      },
      "gate": true,
      "catatan_tambahan": "Detail tambahan yang cukup panjang untuk membungkus beberapa baris di kolomnya.",
      "unit_list": [
        { "unit": "Unit A", "qty": 3, "foto_unit": { "fileUuid": "photo-1" } },
        { "unit": "Unit B", "qty": 0, "foto_unit": null }
      ]
    })
    .as_object()
    .cloned()
    .unwrap()
}

fn make_data() -> SubmissionPdfData {
    let png = tiny_png();
    SubmissionPdfData {
        company_name: "PT Shieldtag Teknologi Indonesia".into(),
        template_name: "Laporan Kunjungan Lengkap".into(),
        template_version: Some(3),
        status: PdfStatus::Submitted,
        submitted_at: Some(utc("2026-08-20T07:45:00Z")),
        created_at: utc("2026-08-20T07:00:00Z"),
        sales_name: "Budi Santoso".into(),
        store_name: "Toko Makmur (saat kunjungan)".into(),
        store_code: Some("TKM-01".into()),
        visit_number: "V-2026-001".into(),
        check_in_at: utc("2026-08-20T07:00:00Z"),
        check_out_at: Some(utc("2026-08-20T07:40:00Z")),
        revision_count: 0,
        last_edited_at: None,
        last_edited_by_name: None,
        schema: FormDocument::from_value(&full_schema()),
        answers: full_answers(),
        media: HashMap::from([
            ("signature-1".to_owned(), media(&png)),
            ("photo-1".to_owned(), media(&png)),
        ]),
        layout_media: None,
        time_zone: "Asia/Jakarta".into(),
        generated_at: utc("2026-08-20T08:00:00Z"),
        generated_by: "Sari".into(),
    }
}

fn count_pages(pdf: &[u8]) -> usize {
    let raw = String::from_utf8_lossy(pdf);
    raw.match_indices("/Type /Page")
        .filter(|(i, _)| raw.as_bytes().get(i + 11) != Some(&b's'))
        .count()
}

fn count_images(pdf: &[u8]) -> usize {
    String::from_utf8_lossy(pdf)
        .matches("/Subtype /Image")
        .count()
}

/// Teks per halaman (urut gambar).
fn page_texts(data: &SubmissionPdfData) -> Vec<Vec<String>> {
    let doc = render_doc(data);
    doc.page_contents()
        .iter()
        .map(|c| {
            extract_fragments(c)
                .into_iter()
                .map(|(_, _, t)| t)
                .collect()
        })
        .collect()
}

fn joined(data: &SubmissionPdfData) -> String {
    page_texts(data)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn minimal_schema_valid_pdf() {
    let mut data = make_data();
    data.schema = FormDocument::from_value(
        &json!({"schemaVersion": 1, "fields": [{"key": "catatan", "type": "text", "label": "Catatan"}]}),
    );
    data.answers = json!({"catatan": "Selesai tanpa kendala."})
        .as_object()
        .cloned()
        .unwrap();
    data.media.clear();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 1024);
    let text = joined(&data);
    assert!(text.contains("Catatan|Selesai tanpa kendala."), "{text}");
}

#[test]
fn renders_all_28_types_with_missing_photo() {
    let data = make_data();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(count_pages(&pdf) >= 1);
    let text = joined(&data);
    for expected in [
        "PT Shieldtag Teknologi Indonesia",
        "Laporan Kunjungan Lengkap",
        "v3  \u{b7}  Submitted",
        "Toko Makmur (saat kunjungan) (TKM-01)",
        "20 Aug 2026 14:00",
        "Laporan Kunjungan",
        "Identitas",
        "Nama *",
        "https://example.com",
        "42",
        "IDR 150,000",
        "10%",
        "4 / 5",
        "20 Aug 2026",
        "14:30",
        "20 Aug 2026 21:30",
        "OK",
        "X  \u{b7}  Y",
        "Yes",
        "Image unavailable",
        "application/pdf  \u{b7}  20.0 KB",
        "-6.200000, 106.816600 (\u{b1}12 m)",
        "UNIT",
        "FOTO UNIT",
        "Unit A",
        "Generated 20 Aug 2026 15:00 by Sari",
        "Page 1 of",
    ] {
        assert!(text.contains(expected), "hilang {expected:?} di {text}");
    }
    // Tautan untuk field url.
    assert!(
        String::from_utf8_lossy(&generate_submission_pdf(&data))
            .contains("/URI (https://example.com)")
    );
}

#[test]
fn hidden_field_not_printed() {
    let mut data = make_data();
    data.answers.insert("gate".into(), json!(false));
    let text = joined(&data);
    assert!(!text.contains("Catatan tambahan"), "{text}");
    assert!(text.contains("No"));
}

#[test]
fn repeater_30_rows_breaks_pages() {
    let rows: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "unit": format!("Unit {}", i + 1),
                "qty": i,
                "foto_unit": if i % 2 == 0 { json!({"fileUuid": "photo-1"}) } else { Value::Null },
            })
        })
        .collect();
    let mut data = make_data();
    data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
        {"key": "unit_list", "type": "repeater", "label": "Unit", "fields": [
            {"key": "unit", "type": "text", "label": "Unit"},
            {"key": "qty", "type": "number", "label": "Qty"},
            {"key": "foto_unit", "type": "photo", "label": "Foto"}
        ]}
    ]}));
    data.answers = json!({"unit_list": rows}).as_object().cloned().unwrap();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(count_pages(&pdf) > 1);
    // Header tabel diulang di halaman baru.
    let pages = page_texts(&data);
    assert!(pages.len() > 1);
    assert!(pages[1].iter().any(|t| t == "UNIT"), "{:?}", pages[1]);
    assert!(pages.iter().flatten().any(|t| t == "Unit 30"));
    // Gambar yang sama ditanam sekali.
    assert_eq!(count_images(&pdf), 2);
}

#[test]
fn draft_with_revisions_draws_both_banners() {
    let mut data = make_data();
    data.status = PdfStatus::Draft;
    data.submitted_at = None;
    data.revision_count = 2;
    data.last_edited_at = Some(utc("2026-08-21T01:00:00Z"));
    data.last_edited_by_name = Some("Sari".into());
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    let text = joined(&data);
    assert!(
        text.contains("DRAFT \u{2014} this form was never submitted;"),
        "{text}"
    );
    assert!(
        text.contains("Corrected 2\u{d7} \u{2014} last edited 21 Aug 2026 08:00 by Sari."),
        "{text}"
    );
    assert!(text.contains("v3  \u{b7}  Draft"));
}

#[test]
fn empty_schema_single_page() {
    let mut data = make_data();
    data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": []}));
    data.answers = Map::new();
    data.media.clear();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(count_pages(&pdf), 1);
}

#[test]
fn unicode_long_and_empty_values() {
    let mut data = make_data();
    data.company_name = "PT Sejahtera Abadi \u{2014} Cabang Yogyakarta (\u{2162})".into();
    data.generated_by = "Rifqi \u{201c}Ade\u{201d} Nugroho".into();
    data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
        {"key": "nama", "type": "text", "label": "Nama Sangat Panjang Sekali Sampai Berpotensi Terpotong"},
        {"key": "catatan", "type": "textarea", "label": "Catatan"},
        {"key": "kosong", "type": "number", "label": "Kosong"},
        {"key": "nol", "type": "number", "label": "Nol"}
    ]}));
    data.answers = json!({
        "nama": "PT Sejahtera Abadi \u{2014} Cabang Yogyakarta (\u{2162}) \u{201c}Ade\u{201d}",
        "catatan": "Catatan panjang. ".repeat(200),
        "nol": 0
    })
    .as_object()
    .cloned()
    .unwrap();
    data.media.clear();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 500);
    let pages = page_texts(&data);
    let all: Vec<&String> = pages.iter().flatten().collect();
    // `0` tercetak, kosong → em dash.
    let nol = all.iter().position(|t| *t == "Nol").unwrap();
    assert_eq!(all[nol + 1], "0");
    let kosong = all.iter().position(|t| *t == "Kosong").unwrap();
    assert_eq!(all[kosong + 1], "\u{2014}");
    // 3400 karakter muat di satu halaman: tanpa elipsis.
    assert!(!all.iter().any(|t| t.ends_with('\u{2026}')), "{all:?}");
    // Di luar WinAnsi → `?`.
    assert!(all.iter().any(|t| t.contains("Yogyakarta (?)")));
}

#[test]
fn group_row_all_justify_values() {
    for justify in ["stretch", "start", "center", "end", "between", "around"] {
        let sig = |key: &str, span: i64| json!({"key": key, "type": "signature", "label": key, "layout": {"colSpan": span}});
        let mut data = make_data();
        data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
            {"key": "ttd", "type": "group", "label": "Tanda tangan",
             "config": {"direction": "row", "justify": justify}, "fields": [sig("a", 3), sig("b", 3)]},
            {"key": "ttd_penuh", "type": "group", "label": "Melebihi 12 kolom",
             "config": {"direction": "row", "justify": justify}, "fields": [sig("c", 8), sig("d", 8)]}
        ]}));
        data.answers = json!({
            "ttd": {"a": {"fileUuid": "signature-1"}, "b": {"fileUuid": "signature-1"}},
            "ttd_penuh": {"c": {"fileUuid": "signature-1"}}
        })
        .as_object()
        .cloned()
        .unwrap();
        let pdf = generate_submission_pdf(&data);
        assert!(pdf.starts_with(b"%PDF-"), "{justify}");
    }
}

#[test]
fn place_slots_matches_node() {
    // 2 × colSpan 3 di lebar 120, `between`: sisa 60 di tengah.
    let s = place_slots(&[3.0, 3.0], 0.0, 120.0, "between");
    assert_eq!(
        s,
        vec![
            Slot {
                x: 4.0,
                width: 22.0
            },
            Slot {
                x: 94.0,
                width: 22.0
            }
        ]
    );
    let s = place_slots(&[8.0, 8.0], 0.0, 120.0, "start");
    assert_eq!(s[1].x, 64.0);
    let s = place_slots(&[3.0, 3.0], 0.0, 120.0, "stretch");
    assert_eq!(
        s[1],
        Slot {
            x: 60.0,
            width: 52.0
        }
    );
}

const KOP_ASSET: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
const KOP_ASSET_2: &str = "3c9d8e7f-6a5b-4c4d-9e3f-2a1b0c9d8e7f";

fn with_layout(data: &mut SubmissionPdfData) {
    let mut schema = full_schema();
    schema["headers"] = json!([
        {"key": "logo", "type": "image", "source": "upload", "assetUuid": KOP_ASSET,
         "layout": {"colSpan": 3}, "config": {"height": 36}},
        {"key": "unggahan", "type": "image", "source": "upload", "assetUuid": KOP_ASSET_2,
         "layout": {"colSpan": 3}, "config": {"height": 36, "align": "right"}},
        {"key": "judul", "type": "text", "text": "LAPORAN \u{2014} {{storeName}} ({{storeCode}}) {{tidakDikenal}}",
         "layout": {"colSpan": 6},
         "config": {"size": "xl", "bold": true, "italic": true, "align": "right", "color": "#1D4ED8"}},
        {"key": "garis", "type": "divider", "config": {"thickness": 2, "style": "dashed"}},
        {"key": "jeda", "type": "spacer", "config": {"height": 6}}
    ]);
    schema["footers"] = json!([
        {"key": "catatan", "type": "text",
         "text": "Halaman {{page}}/{{pageCount}} \u{b7} {{companyName}} \u{b7} {{generatedAt}}",
         "config": {"size": "xs"}}
    ]);
    data.schema = FormDocument::from_value(&schema);
}

#[test]
fn custom_header_footer_every_page() {
    let mut data = make_data();
    with_layout(&mut data);
    let png = tiny_png();
    data.layout_media = Some(HashMap::from([
        (KOP_ASSET.to_owned(), media(&png)),
        (KOP_ASSET_2.to_owned(), media(&png)),
    ]));
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    let pages = page_texts(&data);
    let n = pages.len();
    for (i, page) in pages.iter().enumerate() {
        let t = page.join("|");
        let flat = page.concat();
        assert!(
            flat.contains(
                "LAPORAN \u{2014} Toko Makmur (saat kunjungan) (TKM-01) {{tidakDikenal}}"
            ),
            "{flat}"
        );
        assert!(
            t.contains(&format!(
                "Halaman {}/{n} \u{b7} PT Shieldtag Teknologi Indonesia \u{b7} 20 Aug 2026 15:00",
                i + 1
            )),
            "{t}"
        );
        assert!(t.contains(&format!("Page {} of {n}", i + 1)), "{t}");
        // Kop bawaan tidak tercetak.
        assert!(!t.contains("v3  \u{b7}  Submitted"), "{t}");
    }
    let raw = String::from_utf8_lossy(&render_doc(&data).finish(false)).into_owned();
    assert!(raw.contains("[4 3] 0 d"), "garis putus-putus");
    assert!(raw.contains("/Helvetica-BoldOblique"));
}

#[test]
fn header_image_embedded_once() {
    let png = tiny_png();
    let render = |catatan: String| {
        let mut data = make_data();
        data.schema = FormDocument::from_value(&json!({"schemaVersion": 1,
            "fields": [{"key": "catatan", "type": "textarea", "label": "Catatan"}],
            "headers": [{"key": "logo", "type": "image", "source": "upload", "assetUuid": KOP_ASSET}]}));
        data.answers = json!({ "catatan": catatan }).as_object().cloned().unwrap();
        data.media.clear();
        data.layout_media = Some(HashMap::from([(KOP_ASSET.to_owned(), media(&png))]));
        generate_submission_pdf(&data)
    };
    let single = render("Pendek.".into());
    let multi = render("Catatan panjang sekali. ".repeat(1500));
    assert_eq!(count_pages(&single), 1);
    // Nilai dipotong elipsis setinggi area isi, jadi baris itu pindah ke halaman 2.
    assert!(count_pages(&multi) > 1);
    assert!(count_images(&single) > 0);
    assert_eq!(count_images(&multi), count_images(&single));
}

#[test]
fn value_taller_than_page_is_ellipsized() {
    let mut data = make_data();
    data.schema = FormDocument::from_value(
        &json!({"schemaVersion": 1, "fields": [{"key": "catatan", "type": "textarea", "label": "Catatan"}]}),
    );
    data.answers = json!({"catatan": "Catatan panjang sekali. ".repeat(1500)})
        .as_object()
        .cloned()
        .unwrap();
    let pages = page_texts(&data);
    assert_eq!(pages.len(), 2);
    // Dipotong di `usableHeight - 40`. pdfkit hanya menambah elipsis bila
    // `y + 2·lineHeight > maxY` (tanpa lineGap), sedangkan berhentinya memakai `y` yang
    // sudah ditambah lineGap — dengan lineGap 1,5 baris terakhir tercetak TANPA elipsis
    // (dibandingkan langsung dengan keluaran pdfkit: identik).
    let lines = pages[1]
        .iter()
        .filter(|t| t.starts_with("Catatan panjang"))
        .count();
    assert_eq!(lines, 50, "pdfkit: 50 baris");
}

#[test]
fn missing_or_broken_logo_is_skipped() {
    let mut data = make_data();
    with_layout(&mut data);
    data.layout_media = Some(HashMap::new());
    data.media.clear();
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(count_images(&pdf), 0);

    data.layout_media = Some(HashMap::from([(
        KOP_ASSET.to_owned(),
        PdfMedia {
            buffer: b"bukan gambar".to_vec(),
            content_type: "image/png".into(),
        },
    )]));
    let pdf = generate_submission_pdf(&data);
    assert!(pdf.starts_with(b"%PDF-"));
}

#[test]
fn tall_header_shrinks_content_area() {
    let fields: Vec<Value> = (0..60)
        .map(|i| json!({"key": format!("f{i}"), "type": "text", "label": format!("Field {i}")}))
        .collect();
    let answers: Map<String, Value> = (0..60).map(|i| (format!("f{i}"), json!("nilai"))).collect();
    let mut plain = make_data();
    plain.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": fields}));
    plain.answers = answers.clone();
    plain.media.clear();
    let mut tall = plain.clone();
    tall.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": fields,
        "headers": [
            {"key": "a", "type": "spacer", "config": {"height": 60}},
            {"key": "b", "type": "spacer", "config": {"height": 60}},
            {"key": "c", "type": "spacer", "config": {"height": 60}}
        ],
        "footers": [{"key": "d", "type": "spacer", "config": {"height": 60}}]}));
    let (p, t) = (
        generate_submission_pdf(&plain),
        generate_submission_pdf(&tall),
    );
    assert!(count_pages(&t) > count_pages(&p));
}

#[test]
fn header_row_block_all_justify() {
    let png = tiny_png();
    for justify in ["stretch", "start", "center", "end", "between", "around"] {
        let mut data = make_data();
        let mut schema = full_schema();
        schema["headers"] = json!([{"key": "baris", "type": "row", "config": {"justify": justify},
        "blocks": [
            {"key": "logo", "type": "image", "source": "upload", "assetUuid": KOP_ASSET, "layout": {"colSpan": 3}},
            {"key": "judul", "type": "text", "text": "{{templateName}}", "layout": {"colSpan": 6}}
        ]}]);
        data.schema = FormDocument::from_value(&schema);
        data.layout_media = Some(HashMap::from([(KOP_ASSET.to_owned(), media(&png))]));
        let pdf = generate_submission_pdf(&data);
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(count_images(&pdf) > 0, "{justify}");
        assert!(joined(&data).contains("Laporan Kunjungan Lengkap"));
    }
}

#[test]
fn empty_headers_use_default() {
    let mut data = make_data();
    let mut schema = full_schema();
    schema["headers"] = json!([]);
    schema["footers"] = json!([]);
    data.schema = FormDocument::from_value(&schema);
    let text = joined(&data);
    assert!(text.contains("v3  \u{b7}  Submitted"));
    assert!(text.contains("Generated 20 Aug 2026 15:00 by Sari"));
}

#[test]
fn jpeg_photo_embedded_as_dct() {
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 4, image::Rgb([200, 10, 10])))
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let mut data = make_data();
    data.media.insert(
        "photo-1".into(),
        PdfMedia {
            buffer: buf,
            content_type: "image/jpeg".into(),
        },
    );
    let pdf = String::from_utf8_lossy(&generate_submission_pdf(&data)).into_owned();
    assert!(pdf.contains("/Filter /DCTDecode"));
    let _ = Utc.timestamp_millis_opt(0);
}

/// Dump fragmen (x, y, teks) per halaman untuk dibandingkan dengan pdfkit:
/// `MZ_PDF_DUMP=/path cargo test -p mz-visit-forms --lib pdf::tests::dump -- --ignored`.
#[test]
#[ignore]
fn dump() {
    let Ok(path) = std::env::var("MZ_PDF_DUMP") else {
        return;
    };
    let mut data = make_data();
    let case = std::env::var("MZ_PDF_CASE").unwrap_or_default();
    if case == "long" {
        data.schema = FormDocument::from_value(
            &json!({"schemaVersion": 1, "fields": [{"key": "catatan", "type": "textarea", "label": "Catatan"}]}),
        );
        data.answers = json!({"catatan": "Catatan panjang sekali. ".repeat(1500)})
            .as_object()
            .cloned()
            .unwrap();
    }
    if case == "draft" {
        data.status = PdfStatus::Draft;
        data.submitted_at = None;
        data.revision_count = 2;
        data.last_edited_at = Some(utc("2026-08-21T01:00:00Z"));
        data.last_edited_by_name = Some("Sari".into());
    }
    if case == "repeater" {
        let rows: Vec<Value> = (0..30)
            .map(|i| {
                json!({"unit": format!("Unit {}", i + 1), "qty": i,
                       "foto_unit": if i % 2 == 0 { json!({"fileUuid": "photo-1"}) } else { Value::Null }})
            })
            .collect();
        data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
            {"key": "unit_list", "type": "repeater", "label": "Unit", "fields": [
                {"key": "unit", "type": "text", "label": "Unit"},
                {"key": "qty", "type": "number", "label": "Qty"},
                {"key": "foto_unit", "type": "photo", "label": "Foto"}]}]}));
        data.answers = json!({"unit_list": rows}).as_object().cloned().unwrap();
    }
    if case == "unicode" {
        data.company_name = "PT Sejahtera Abadi \u{2014} Cabang Yogyakarta".into();
        data.generated_by = "Rifqi \u{201c}Ade\u{201d} Nugroho".into();
        data.schema = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
            {"key": "nama", "type": "text", "label": "Nama Sangat Panjang Sekali Sampai Berpotensi Terpotong"},
            {"key": "catatan", "type": "textarea", "label": "Catatan"},
            {"key": "kosong", "type": "number", "label": "Kosong"},
            {"key": "nol", "type": "number", "label": "Nol"}]}));
        data.answers = json!({"nama": "x/y-z well-known a\nb", "catatan": "Catatan panjang. ".repeat(200), "nol": 0})
            .as_object()
            .cloned()
            .unwrap();
        data.media.clear();
    }
    if case == "layout" {
        with_layout(&mut data);
        let png = tiny_png();
        data.layout_media = Some(HashMap::from([
            (KOP_ASSET.to_owned(), media(&png)),
            (KOP_ASSET_2.to_owned(), media(&png)),
        ]));
    }
    let doc = render_doc(&data);
    let mut out = String::new();
    for (i, c) in doc.page_contents().iter().enumerate() {
        for (x, y, t) in extract_fragments(c) {
            out.push_str(&format!("{}\t{x:.2}\t{y:.2}\t{t}\n", i + 1));
        }
    }
    std::fs::write(&path, out).unwrap();
    std::fs::write(
        format!("{path}.raw"),
        doc.page_contents().join("\n=====PAGE=====\n"),
    )
    .unwrap();
}
