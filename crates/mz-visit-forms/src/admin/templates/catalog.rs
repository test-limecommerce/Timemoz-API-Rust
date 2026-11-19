//! `getCatalog()`, `submissionPdfFilename`, `previewMedia()` dan konstanta atas
//! `admin-store-visit-forms.service.ts`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::consts::{
    FORM_SCHEMA_VERSION, FormContainerContext, FormFieldFormat, FormNodeType, GRID_COLUMNS,
    GROUP_DIRECTIONS, GROUP_JUSTIFY, LAYOUT_ALIGNMENTS, LAYOUT_ASSET_CONTENT_TYPES,
    LAYOUT_BLOCK_TYPES, LAYOUT_DIVIDER_STYLES, LAYOUT_DIVIDER_THICKNESS_RANGE,
    LAYOUT_IMAGE_HEIGHT_RANGE, LAYOUT_IMAGE_SOURCES, LAYOUT_ROW_CHILD_TYPES, LAYOUT_SECTIONS,
    LAYOUT_SPACER_HEIGHT_RANGE, LAYOUT_TEXT_SIZES, LAYOUT_TEXT_VARIABLES, MAX_DESCRIPTION_LENGTH,
    MAX_KEY_LENGTH, MAX_LABEL_LENGTH, MAX_LAYOUT_ASSET_BYTES, MAX_LAYOUT_BLOCKS_PER_SECTION,
    MAX_LAYOUT_FOOTER_HEIGHT, MAX_LAYOUT_HEADER_HEIGHT, MAX_LAYOUT_TEXT_LENGTH,
    MAX_NODES_PER_TEMPLATE, MAX_OPTION_LABEL_LENGTH, MAX_OPTIONS_PER_FIELD, MAX_PLACEHOLDER_LENGTH,
    MAX_REPEATER_ROWS, OPTIONS_SOURCES, Range, VisibleIfOp, lookup_layout_block_config,
};
use crate::preview_sample::{PREVIEW_PHOTO_KEY, PREVIEW_SIGNATURE_KEY};
use crate::types::PdfMedia;
use crate::value_util::js_stringify;

/// `MAX_SCHEMA_BYTES` (admin-store-visit-forms.service.ts:108).
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;

/// `EMPTY_FIELD_MARK`.
pub const EMPTY_FIELD_MARK: &str = "—";

/// `EMPTY_FORM_SCHEMA` (admin-store-visit-forms.service.ts:95).
pub fn empty_form_schema() -> Value {
    json!({ "schemaVersion": FORM_SCHEMA_VERSION, "fields": [] })
}

/// `Buffer.byteLength(JSON.stringify(schema ?? {}), 'utf8')`.
pub fn schema_byte_size(schema: Option<&Value>) -> usize {
    match schema {
        None | Some(Value::Null) => 2,
        Some(v) => js_stringify(v).len(),
    }
}

/// `submissionPdfFilename(templateName, visitNumber, submissionUuid)`
/// (admin-store-visit-forms.service.ts:120) — dijamin `[a-z0-9-]`.
pub fn submission_pdf_filename(
    template_name: &str,
    visit_number: Option<&str>,
    submission_uuid: &str,
) -> String {
    // `toLowerCase` lalu `[^a-z0-9]+` → `-`, potong `-` di ujung.
    fn slug(value: &str) -> String {
        let lower = value.to_lowercase();
        let mut out = String::new();
        let mut dash = false;
        for c in lower.chars() {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
                dash = false;
            } else if !dash {
                out.push('-');
                dash = true;
            }
        }
        out.trim_matches('-').to_owned()
    }
    // `.slice(0, 60)` atas string ASCII hasil slug.
    let base = slug(template_name);
    let cut: String = base.chars().take(60).collect();
    let mut name = cut.trim_end_matches('-').to_owned();
    if name.is_empty() {
        name = "form".to_owned();
    }
    let mut suffix = slug(visit_number.unwrap_or_default());
    if suffix.is_empty() {
        suffix = submission_uuid.chars().take(8).collect();
    }
    format!("{name}-{suffix}.pdf")
}

fn range(r: Range) -> Value {
    json!({ "min": mz_core::jsfmt::num(r.min), "max": mz_core::jsfmt::num(r.max) })
}

/// `getCatalog()` (admin-store-visit-forms.service.ts:1261) — murni konstanta.
pub fn catalog() -> Value {
    let mut types = Map::new();
    for t in FormNodeType::ALL {
        let spec = t.spec();
        types.insert(
            t.as_str().to_owned(),
            json!({
                "category": spec.category.as_str(),
                "allowedValidation": spec.allowed_validation,
                "allowedConfig": spec.allowed_config,
                "requiresOptions": spec.requires_options,
                "ordered": spec.ordered,
                "media": spec.media,
            }),
        );
    }
    let mut child_types = Map::new();
    for ctx in [
        FormContainerContext::Root,
        FormContainerContext::Section,
        FormContainerContext::Group,
        FormContainerContext::Repeater,
    ] {
        let allowed: Vec<&str> = ctx
            .allowed_child_types()
            .into_iter()
            .map(FormNodeType::as_str)
            .collect();
        child_types.insert(ctx.as_str().to_owned(), json!(allowed));
    }
    let mut block_types = Map::new();
    for t in LAYOUT_BLOCK_TYPES {
        block_types.insert(
            (*t).to_owned(),
            json!({ "allowedConfig": lookup_layout_block_config(t).unwrap_or(&[]) }),
        );
    }
    let formats: Vec<&str> = FormFieldFormat::ALL.iter().map(|f| f.as_str()).collect();
    let ops: Vec<&str> = VisibleIfOp::ALL.iter().map(|o| o.as_str()).collect();
    json!({
        "schemaVersion": FORM_SCHEMA_VERSION,
        "types": types,
        "allowedChildTypes": child_types,
        "formats": formats,
        "visibleIfOperators": ops,
        "repeaterRenderAs": crate::consts::REPEATER_RENDER_AS,
        "groupDirections": GROUP_DIRECTIONS,
        "groupJustify": GROUP_JUSTIFY,
        "optionsSources": OPTIONS_SOURCES,
        "layout": {
            "sections": LAYOUT_SECTIONS,
            "blockTypes": block_types,
            "imageSources": LAYOUT_IMAGE_SOURCES,
            "rowChildTypes": LAYOUT_ROW_CHILD_TYPES,
            "textSizes": LAYOUT_TEXT_SIZES,
            "alignments": LAYOUT_ALIGNMENTS,
            "dividerStyles": LAYOUT_DIVIDER_STYLES,
            "textVariables": LAYOUT_TEXT_VARIABLES,
            "assetContentTypes": LAYOUT_ASSET_CONTENT_TYPES,
            "limits": {
                "maxBlocksPerSection": MAX_LAYOUT_BLOCKS_PER_SECTION,
                "maxTextLength": MAX_LAYOUT_TEXT_LENGTH,
                "imageHeight": range(LAYOUT_IMAGE_HEIGHT_RANGE),
                "spacerHeight": range(LAYOUT_SPACER_HEIGHT_RANGE),
                "dividerThickness": range(LAYOUT_DIVIDER_THICKNESS_RANGE),
                "maxHeaderHeight": mz_core::jsfmt::num(MAX_LAYOUT_HEADER_HEIGHT),
                "maxFooterHeight": mz_core::jsfmt::num(MAX_LAYOUT_FOOTER_HEIGHT),
                "maxAssetBytes": MAX_LAYOUT_ASSET_BYTES,
            },
        },
        "limits": {
            "maxNodesPerTemplate": MAX_NODES_PER_TEMPLATE,
            "maxOptionsPerField": MAX_OPTIONS_PER_FIELD,
            "maxRepeaterRows": MAX_REPEATER_ROWS,
            "maxKeyLength": MAX_KEY_LENGTH,
            "maxSchemaBytes": MAX_SCHEMA_BYTES,
            "gridColumns": GRID_COLUMNS,
            "maxLabelLength": MAX_LABEL_LENGTH,
            "maxDescriptionLength": MAX_DESCRIPTION_LENGTH,
            "maxPlaceholderLength": MAX_PLACEHOLDER_LENGTH,
            "maxOptionLabelLength": MAX_OPTION_LABEL_LENGTH,
        },
    })
}

/// `buildPreviewMedia()` (admin-store-visit-forms.service.ts:147): tanda tangan bergelombang di
/// latar transparan 300×100 dan foto abu-abu 320×200, PNG. Byte tidak identik dengan Jimp.
fn build_preview_media() -> HashMap<String, PdfMedia> {
    let mut signature = image::RgbaImage::from_pixel(300, 100, image::Rgba([0, 0, 0, 0]));
    for x in 20..280u32 {
        let y = (50.0 + 22.0 * (f64::from(x) / 16.0).sin()).round() as i64;
        for d in 0..3 {
            let yy = y + d;
            if (0..100).contains(&yy) {
                signature.put_pixel(x, yy as u32, image::Rgba([0x0f, 0x17, 0x2a, 0xff]));
            }
        }
    }
    let mut photo = image::RgbaImage::from_pixel(320, 200, image::Rgba([0xe2, 0xe8, 0xf0, 0xff]));
    for x in 110..210u32 {
        for y in 60..140u32 {
            photo.put_pixel(x, y, image::Rgba([0x94, 0xa3, 0xb8, 0xff]));
        }
    }
    let png = |img: &image::RgbaImage| {
        let mut out = std::io::Cursor::new(Vec::new());
        // Gagal encode = buffer kosong; generator PDF menganggapnya media rusak (placeholder).
        let _ = img.write_to(&mut out, image::ImageFormat::Png);
        PdfMedia {
            buffer: out.into_inner(),
            content_type: "image/png".to_owned(),
        }
    };
    HashMap::from([
        (PREVIEW_SIGNATURE_KEY.to_owned(), png(&signature)),
        (PREVIEW_PHOTO_KEY.to_owned(), png(&photo)),
    ])
}

/// `previewMedia()` — dibuat sekali per proses.
pub fn preview_media() -> &'static HashMap<String, PdfMedia> {
    static CACHE: OnceLock<HashMap<String, PdfMedia>> = OnceLock::new();
    CACHE.get_or_init(build_preview_media)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_node_golden() {
        // `JSON.stringify(service.getCatalog())` dari Node asli (urutan kunci ikut dibandingkan).
        let golden = include_str!("catalog.golden.json").trim();
        assert_eq!(serde_json::to_string(&catalog()).unwrap(), golden);
    }

    #[test]
    fn catalog_spec_cases() {
        let c = catalog();
        assert_eq!(
            c["types"].as_object().unwrap().len(),
            FormNodeType::ALL.len()
        );
        assert_eq!(
            c["types"]["text"],
            json!({
                "category": "leaf",
                "allowedValidation": ["minLength", "maxLength", "format"],
                "allowedConfig": [],
                "requiresOptions": false,
                "ordered": false,
                "media": false,
            })
        );
        assert_eq!(c["types"]["rating"]["allowedConfig"], json!(["max"]));
        assert_eq!(c["types"]["signature"]["media"], json!(true));
        assert_eq!(c["types"]["select"]["requiresOptions"], json!(true));
        let section = c["allowedChildTypes"]["section"].as_array().unwrap();
        assert!(!section.contains(&json!("section")));
        assert!(
            c["allowedChildTypes"]["root"]
                .as_array()
                .unwrap()
                .contains(&json!("section"))
        );
        assert_eq!(c["limits"]["maxSchemaBytes"], json!(262_144));
        assert!(
            c["optionsSources"]
                .as_array()
                .unwrap()
                .contains(&json!("order"))
        );
    }

    #[test]
    fn filenames_match_node() {
        // Keluaran `submissionPdfFilename` Node asli.
        assert_eq!(
            submission_pdf_filename(
                "Laporan \"Kunjungan\" Toko!!",
                Some("pratinjau"),
                "abcdef12-3456"
            ),
            "laporan-kunjungan-toko-pratinjau.pdf"
        );
        assert_eq!(
            submission_pdf_filename("---", None, "abcdef12-3456"),
            "form-abcdef12.pdf"
        );
        assert_eq!(
            submission_pdf_filename(&format!("{} b", "A".repeat(70)), Some("SV/2026/001"), "x"),
            format!("{}-sv-2026-001.pdf", "a".repeat(60))
        );
    }

    #[test]
    fn schema_size() {
        assert_eq!(schema_byte_size(None), 2);
        assert_eq!(schema_byte_size(Some(&json!({"a": 1.0}))), 7);
    }

    #[test]
    fn preview_media_are_pngs() {
        let m = preview_media();
        for k in [PREVIEW_SIGNATURE_KEY, PREVIEW_PHOTO_KEY] {
            let media = &m[k];
            assert_eq!(media.content_type, "image/png");
            assert!(media.buffer.starts_with(&[0x89, b'P', b'N', b'G']));
        }
    }
}
