//! Port `form-submission-pdf.generator.ts`: render satu submission menjadi PDF A4 portrait.
//!
//! Murni: tidak menyentuh database/bucket. Seluruh masukan (termasuk byte gambar) datang
//! lewat [`SubmissionPdfData`]. Penulis PDF ala pdfkit ada di [`writer`] (versi diperluas
//! dari `mz-billing::pdf`; metrik AFM & putus baris bersama di `mz-pdfkit`). Tata letak, koordinat, gaya, dan urutan gambar mengikuti Node
//! baris demi baris; lihat `plan/deviations/mz-visit-forms.md` untuk selisihnya.

mod format;
pub mod writer;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::consts::{
    FormNodeType, GRID_COLUMNS, LAYOUT_VARIABLE_PATTERN, MAX_LAYOUT_FOOTER_HEIGHT,
    MAX_LAYOUT_HEADER_HEIGHT,
};
use crate::types::{FormDocument, FormLayoutBlock, FormNode, PdfMedia};
use crate::value_util::is_empty_value;
use crate::visibility::is_node_visible;

pub use format::{format_instant, intl_number, js_string};
use writer::{Align, Doc, Font, OpenedImage, Rgb, TextOpts};

/// `status` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfStatus {
    Draft,
    Submitted,
}

/// `SubmissionPdfData` (`form-submission-pdf.generator.ts:46`).
#[derive(Debug, Clone)]
pub struct SubmissionPdfData {
    pub company_name: String,
    pub template_name: String,
    pub template_version: Option<i64>,
    pub status: PdfStatus,
    pub submitted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,

    pub sales_name: String,
    /// Snapshot saat kunjungan.
    pub store_name: String,
    pub store_code: Option<String>,
    pub visit_number: String,
    pub check_in_at: DateTime<Utc>,
    pub check_out_at: Option<DateTime<Utc>>,

    pub revision_count: i64,
    pub last_edited_at: Option<DateTime<Utc>>,
    pub last_edited_by_name: Option<String>,

    pub schema: FormDocument,
    pub answers: Map<String, Value>,
    /// fileUuid → byte gambar siap tanam.
    pub media: HashMap<String, PdfMedia>,
    /// assetUuid (huruf kecil) → gambar kop/kaki. `None` = blok `image` dilewati.
    pub layout_media: Option<HashMap<String, PdfMedia>>,

    /// Timezone company.
    pub time_zone: String,
    pub generated_at: DateTime<Utc>,
    pub generated_by: String,
}

// ── Ukuran halaman ───────────────────────────────────────────────────────────

const PAGE_HEIGHT: f64 = writer::PAGE_H;
const MARGIN: f64 = 40.0;
const HEADER_HEIGHT: f64 = 58.0;
const FOOTER_HEIGHT: f64 = 30.0;
const CONTENT_WIDTH: f64 = writer::PAGE_W - MARGIN * 2.0;

const LAYOUT_HEADER_GAP: f64 = 10.0;
const LAYOUT_FOOTER_GAP: f64 = 12.0;
const LAYOUT_ROW_GAP: f64 = 4.0;
const PAGE_LABEL_HEIGHT: f64 = 9.0;
const PAGE_LABEL_GAP: f64 = 4.0;

const DEFAULT_LAYOUT_TEXT_SIZE: f64 = 10.0;
const DEFAULT_LAYOUT_IMAGE_HEIGHT: f64 = 40.0;
const DEFAULT_LAYOUT_SPACER_HEIGHT: f64 = 8.0;
const DEFAULT_LAYOUT_DIVIDER_THICKNESS: f64 = 0.75;
const LAYOUT_DIVIDER_PADDING: f64 = 3.0;
const MEASURE_PAGE_PLACEHOLDER: &str = "99";

const GUTTER: f64 = 8.0;
const ROW_GAP: f64 = 10.0;
const CARD_PADDING: f64 = 12.0;
const CELL_PADDING: f64 = 5.0;
const NUMBER_COLUMN_WIDTH: f64 = 22.0;

const MAX_IMAGE_HEIGHT: f64 = 120.0;
const TABLE_IMAGE_HEIGHT: f64 = 56.0;
const PLACEHOLDER_HEIGHT: f64 = 44.0;
const TITLE_LINE_HEIGHT: f64 = 20.0;
const SECTION_ORPHAN_GUARD: f64 = 64.0;
const DEFAULT_RATING_MAX: f64 = 5.0;
const EMPTY_MARK: &str = "\u{2014}";

fn layout_text_size(s: &str) -> Option<f64> {
    Some(match s {
        "xs" => 7.0,
        "sm" => 8.5,
        "md" => 10.0,
        "lg" => 13.0,
        "xl" => 16.0,
        _ => return None,
    })
}

mod colors {
    pub const INK: &str = "#0F172A";
    pub const BODY: &str = "#1E293B";
    pub const MUTED: &str = "#64748B";
    pub const FAINT: &str = "#94A3B8";
    pub const BORDER: &str = "#E2E8F0";
    pub const HAIRLINE: &str = "#EEF2F7";
    pub const SURFACE: &str = "#F8FAFC";
    pub const PRIMARY: &str = "#2563EB";
    pub const PRIMARY_SOFT: &str = "#EFF4FE";
    pub const PRIMARY_BORDER: &str = "#DCE7FD";
    pub const AMBER: &str = "#B45309";
    pub const AMBER_SOFT: &str = "#FFFBEB";
    pub const AMBER_BORDER: &str = "#F59E0B";
}

#[derive(Debug, Clone, Copy)]
struct TextStyle {
    font: Font,
    size: f64,
    color: Rgb,
    align: Align,
    line_gap: f64,
}

const fn style(font: Font, size: f64, line_gap: f64) -> TextStyle {
    TextStyle {
        font,
        size,
        color: Rgb(0.0, 0.0, 0.0),
        align: Align::Left,
        line_gap,
    }
}

impl TextStyle {
    fn colored(mut self, hex: &str) -> Self {
        self.color = Rgb::hex(hex);
        self
    }
    fn aligned(mut self, align: Align) -> Self {
        self.align = align;
        self
    }
}

/// `STYLE` (`form-submission-pdf.generator.ts:185`).
struct Styles {
    title: TextStyle,
    company: TextStyle,
    header_meta: TextStyle,
    footer: TextStyle,
    card_label: TextStyle,
    card_value: TextStyle,
    banner_amber: TextStyle,
    banner_blue: TextStyle,
    section_label: TextStyle,
    section_note: TextStyle,
    group_label: TextStyle,
    heading: TextStyle,
    paragraph: TextStyle,
    field_label: TextStyle,
    field_note: TextStyle,
    value: TextStyle,
    value_empty: TextStyle,
    value_link: TextStyle,
    value_note: TextStyle,
    table_head: TextStyle,
    table_cell: TextStyle,
    table_cell_empty: TextStyle,
    table_index: TextStyle,
}

impl Styles {
    fn new() -> Self {
        use Font::{Bold, Italic, Regular};
        Self {
            title: style(Bold, 16.0, 0.0).colored(colors::INK),
            company: style(Regular, 8.0, 0.0).colored(colors::MUTED),
            header_meta: style(Regular, 8.0, 0.0).colored(colors::FAINT),
            footer: style(Regular, 7.5, 0.0).colored(colors::MUTED),
            card_label: style(Regular, 7.0, 0.0).colored(colors::FAINT),
            card_value: style(Regular, 9.0, 0.0).colored(colors::INK),
            banner_amber: style(Bold, 8.5, 1.0).colored(colors::AMBER),
            banner_blue: style(Bold, 8.5, 1.0).colored(colors::PRIMARY),
            section_label: style(Bold, 11.0, 0.0).colored(colors::INK),
            section_note: style(Regular, 7.5, 1.0).colored(colors::MUTED),
            group_label: style(Bold, 9.5, 0.0).colored(colors::INK),
            heading: style(Bold, 12.0, 0.0).colored(colors::INK),
            paragraph: style(Regular, 9.0, 1.5).colored(colors::MUTED),
            field_label: style(Bold, 8.0, 0.0).colored(colors::MUTED),
            field_note: style(Regular, 7.5, 1.0).colored(colors::FAINT),
            value: style(Regular, 9.5, 1.5).colored(colors::BODY),
            value_empty: style(Regular, 9.5, 0.0).colored(colors::FAINT),
            value_link: style(Regular, 9.5, 1.5).colored(colors::PRIMARY),
            value_note: style(Italic, 8.5, 0.0).colored(colors::FAINT),
            table_head: style(Bold, 7.0, 0.0).colored(colors::MUTED),
            table_cell: style(Regular, 8.5, 1.0).colored(colors::BODY),
            table_cell_empty: style(Regular, 8.5, 0.0).colored(colors::FAINT),
            table_index: style(Regular, 8.5, 0.0)
                .colored(colors::FAINT)
                .aligned(Align::Center),
        }
    }
}

// ── Pembantu murni ───────────────────────────────────────────────────────────

fn config_string<'a>(block: &'a FormLayoutBlock, key: &str) -> Option<&'a str> {
    block.config_value(key).and_then(Value::as_str)
}

fn config_number(block: &FormLayoutBlock, key: &str) -> Option<f64> {
    block
        .config_value(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
}

fn align_of(block: &FormLayoutBlock) -> Align {
    match config_string(block, "align") {
        Some("center") => Align::Center,
        Some("right") => Align::Right,
        _ => Align::Left,
    }
}

fn layout_text_style(block: &FormLayoutBlock) -> TextStyle {
    let bold = block.config_value("bold") == Some(&Value::Bool(true));
    let italic = block.config_value("italic") == Some(&Value::Bool(true));
    let font = match (bold, italic) {
        (true, true) => Font::BoldItalic,
        (true, false) => Font::Bold,
        (false, true) => Font::Italic,
        (false, false) => Font::Regular,
    };
    TextStyle {
        font,
        size: config_string(block, "size")
            .and_then(layout_text_size)
            .unwrap_or(DEFAULT_LAYOUT_TEXT_SIZE),
        color: Rgb::hex(config_string(block, "color").unwrap_or(colors::INK)),
        align: align_of(block),
        line_gap: 1.0,
    }
}

/// `Math.round` JS.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `clampSpan`.
fn clamp_span(span: Option<f64>) -> f64 {
    let Some(span) = span.filter(|s| s.is_finite()) else {
        return GRID_COLUMNS as f64;
    };
    js_round(span).clamp(1.0, GRID_COLUMNS as f64)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    x: f64,
    width: f64,
}

/// `placeSlots` — baris flex (group `direction: row` & blok kop `row`).
fn place_slots(spans: &[f64], x: f64, width: f64, justify: &str) -> Vec<Slot> {
    let count = spans.len();
    let alone = count == 1;
    let inner = |raw: f64| if alone { raw } else { (raw - GUTTER).max(1.0) };
    let grid = GRID_COLUMNS as f64;

    if justify == "stretch" {
        let raw = width / (count.max(1) as f64);
        return (0..count)
            .map(|i| Slot {
                x: x + raw * i as f64,
                width: inner(raw),
            })
            .collect();
    }

    let total_span: f64 = spans.iter().sum();
    let scale = if total_span > grid {
        grid / total_span
    } else {
        1.0
    };
    let raws: Vec<f64> = spans.iter().map(|s| (width * s * scale) / grid).collect();
    let free = (width - raws.iter().sum::<f64>()).max(0.0);

    let (mut offset, mut between) = (0.0, 0.0);
    match justify {
        "end" => offset = free,
        "center" => offset = free / 2.0,
        "between" => {
            between = if count > 1 {
                free / (count - 1) as f64
            } else {
                0.0
            }
        }
        "around" => {
            between = free / count as f64;
            offset = between / 2.0;
        }
        _ => {}
    }

    let mut cursor = x + offset;
    raws.iter()
        .map(|raw| {
            let cell_x = cursor + if alone { 0.0 } else { GUTTER / 2.0 };
            cursor += raw + between;
            Slot {
                x: cell_x,
                width: inner(*raw),
            }
        })
        .collect()
}

struct MediaReference {
    file_uuid: String,
    content_type: Option<String>,
    size_bytes: Option<f64>,
}

fn read_media_reference(value: Option<&Value>) -> Option<MediaReference> {
    let o = value?.as_object()?;
    Some(MediaReference {
        file_uuid: o.get("fileUuid")?.as_str()?.to_owned(),
        content_type: o
            .get("contentType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        size_bytes: o.get("sizeBytes").and_then(Value::as_f64),
    })
}

fn read_location(value: Option<&Value>) -> Option<(f64, f64, Option<f64>)> {
    let o = value?.as_object()?;
    let lat = o.get("lat")?;
    let lng = o.get("lng")?;
    if !lat.is_number() || !lng.is_number() {
        return None;
    }
    Some((
        lat.as_f64()?,
        lng.as_f64()?,
        o.get("accuracy").and_then(Value::as_f64),
    ))
}

/// `optionLabel`.
fn option_label(node: &FormNode, value: Option<&Value>) -> String {
    let Some(Value::String(s)) = value else {
        return match value {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        };
    };
    node.options
        .as_ref()
        .and_then(|o| o.items.as_ref())
        .and_then(|items| items.iter().find(|i| &i.value == s))
        .map_or_else(|| s.clone(), |i| i.label.clone())
}

/// `formatBytes`.
fn format_bytes(bytes: f64) -> String {
    use mz_core::jsfmt::{num_string, to_fixed};
    if bytes < 1024.0 {
        format!("{} B", num_string(bytes))
    } else if bytes < 1024.0 * 1024.0 {
        format!("{} KB", to_fixed(bytes / 1024.0, 1))
    } else {
        format!("{} MB", to_fixed(bytes / (1024.0 * 1024.0), 1))
    }
}

/// `Number(x)` JS untuk nilai JSON.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => mz_core::jsfmt::number(s),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Null => 0.0,
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [one] => js_number(&Value::String(js_string(one))),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

struct PlacedBlock<'a> {
    block: &'a FormLayoutBlock,
    x: f64,
    width: f64,
}

struct Pages {
    page: String,
    page_count: String,
}

#[derive(Clone)]
struct LayoutRegion {
    blocks: Vec<FormLayoutBlock>,
    height: f64,
}

struct RepeaterColumn<'a> {
    node: &'a FormNode,
    group_key: Option<&'a str>,
}

/// Penggambar sekali pakai (`SubmissionPdfRenderer`).
struct Renderer<'a> {
    data: &'a SubmissionPdfData,
    doc: Doc,
    st: Styles,
    image_cache: HashMap<String, Option<OpenedImage>>,
    header: Option<LayoutRegion>,
    footer: Option<LayoutRegion>,
    content_top: f64,
    content_bottom: f64,
    usable_height: f64,
    y: f64,
}

impl<'a> Renderer<'a> {
    fn new(data: &'a SubmissionPdfData) -> Self {
        let mut doc = Doc::new(MARGIN + HEADER_HEIGHT, MARGIN + FOOTER_HEIGHT);
        doc.set_info(
            "Title",
            format!("{} \u{2014} {}", data.template_name, data.visit_number),
        );
        doc.set_info("Author", data.company_name.clone());
        doc.set_info("Creator", "Timemoz".to_owned());
        let mut r = Self {
            data,
            doc,
            st: Styles::new(),
            image_cache: HashMap::new(),
            header: None,
            footer: None,
            content_top: 0.0,
            content_bottom: 0.0,
            usable_height: 0.0,
            y: 0.0,
        };
        r.header = r.measure_region(
            data.schema.headers.clone().unwrap_or_default(),
            MAX_LAYOUT_HEADER_HEIGHT,
        );
        r.footer = r.measure_region(
            data.schema.footers.clone().unwrap_or_default(),
            MAX_LAYOUT_FOOTER_HEIGHT,
        );
        r.content_top = match &r.header {
            Some(h) => MARGIN + h.height + LAYOUT_HEADER_GAP,
            None => MARGIN + HEADER_HEIGHT,
        };
        r.content_bottom = if r.footer.is_some() {
            r.footer_top() - LAYOUT_FOOTER_GAP
        } else {
            PAGE_HEIGHT - MARGIN - FOOTER_HEIGHT
        };
        r.usable_height = r.content_bottom - r.content_top;
        r.y = r.content_top;
        let bottom_margin = PAGE_HEIGHT - r.content_bottom;
        r.doc.set_margins(r.content_top, bottom_margin);
        r
    }

    fn footer_top(&self) -> f64 {
        let height = self.footer.as_ref().map_or(0.0, |f| f.height);
        PAGE_HEIGHT - MARGIN - PAGE_LABEL_HEIGHT - PAGE_LABEL_GAP - height
    }

    fn render(mut self) -> Doc {
        self.draw_context_card();
        self.draw_banners();
        let fields = &self.data.schema.fields;
        let answers = &self.data.answers;
        self.draw_scope(fields, answers, MARGIN, CONTENT_WIDTH);
        self.draw_headers_and_footers();
        self.doc
    }

    // ── Primitif ───────────────────────────────────────────────────────────

    fn height_of(&mut self, text: &str, width: f64, s: &TextStyle) -> f64 {
        if text.is_empty() || width <= 0.0 {
            return 0.0;
        }
        self.doc
            .height_of_string(text, s.font, s.size, width, s.line_gap)
    }

    /// `line(...)` — gambar (atau ukur) satu blok teks; tidak menyentuh `self.y`.
    #[allow(clippy::too_many_arguments)]
    fn line(
        &mut self,
        text: &str,
        x: f64,
        y: f64,
        width: f64,
        s: &TextStyle,
        draw: bool,
        max_height: Option<f64>,
    ) -> f64 {
        if text.is_empty() || width <= 0.0 {
            return 0.0;
        }
        let natural = self.height_of(text, width, s);
        let capped = match max_height {
            Some(m) if natural > m => m,
            _ => natural,
        };
        if draw {
            let opts = TextOpts {
                font: s.font,
                size: s.size,
                color: s.color,
                width,
                align: s.align,
                line_gap: s.line_gap,
                height: (capped < natural).then_some(capped),
                ellipsis: capped < natural,
            };
            self.doc.text(text, x, y, &opts);
        }
        capped
    }

    fn rule(&mut self, x: f64, y: f64, width: f64, color: &str) {
        self.doc.stroke_color(Rgb::hex(color));
        self.doc.line_width(0.5);
        self.doc.move_to(x, y);
        self.doc.line_to(x + width, y);
        self.doc.stroke();
    }

    fn add_page(&mut self) {
        self.doc.add_page();
        self.y = self.content_top;
    }

    fn ensure_space(&mut self, needed: f64) {
        if needed >= self.usable_height {
            return;
        }
        if self.y + needed > self.content_bottom {
            self.add_page();
        }
    }

    // ── Header, footer, kartu konteks ──────────────────────────────────────

    fn status_label(&self) -> &'static str {
        match self.data.status {
            PdfStatus::Draft => "Draft",
            PdfStatus::Submitted => "Submitted",
        }
    }

    fn draw_headers_and_footers(&mut self) {
        let count = self.doc.page_count();
        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(v) = self.data.template_version {
            meta_parts.push(format!("v{v}"));
        }
        meta_parts.push(self.status_label().to_owned());
        let meta = meta_parts.join("  \u{b7}  ");

        let footer_y = PAGE_HEIGHT - MARGIN - 18.0;
        let generated = format!(
            "Generated {} by {}",
            self.format_instant(self.data.generated_at),
            self.data.generated_by
        );
        let footer_right = self.st.footer.aligned(Align::Right);
        let footer_style = self.st.footer;

        for index in 0..count {
            self.doc.switch_to_page(index);
            self.doc.set_page_margins(0.0, 0.0);
            let pages = Pages {
                page: (index + 1).to_string(),
                page_count: count.to_string(),
            };
            let page_label = format!("Page {} of {}", index + 1, count);

            if let Some(header) = self.header.clone() {
                self.draw_region(&header, MARGIN, &pages);
            } else {
                self.draw_default_header(&meta);
            }

            if let Some(footer) = self.footer.clone() {
                let top = self.footer_top();
                self.draw_region(&footer, top, &pages);
                self.line(
                    &page_label,
                    MARGIN,
                    PAGE_HEIGHT - MARGIN - PAGE_LABEL_HEIGHT,
                    CONTENT_WIDTH,
                    &footer_right,
                    true,
                    None,
                );
            } else {
                self.rule(MARGIN, footer_y, CONTENT_WIDTH, colors::BORDER);
                self.line(
                    &generated,
                    MARGIN,
                    footer_y + 7.0,
                    CONTENT_WIDTH * 0.7,
                    &footer_style,
                    true,
                    None,
                );
                self.line(
                    &page_label,
                    MARGIN + CONTENT_WIDTH * 0.7,
                    footer_y + 7.0,
                    CONTENT_WIDTH * 0.3,
                    &footer_right,
                    true,
                    None,
                );
            }
        }
    }

    fn draw_default_header(&mut self, meta: &str) {
        let (company, title, header_meta) = (self.st.company, self.st.title, self.st.header_meta);
        let company_name = self.data.company_name.clone();
        let template_name = self.data.template_name.clone();
        self.line(
            &company_name,
            MARGIN,
            MARGIN,
            CONTENT_WIDTH,
            &company,
            true,
            None,
        );
        self.line(
            &template_name,
            MARGIN,
            MARGIN + 11.0,
            CONTENT_WIDTH,
            &title,
            true,
            Some(TITLE_LINE_HEIGHT),
        );
        self.line(
            meta,
            MARGIN,
            MARGIN + 34.0,
            CONTENT_WIDTH,
            &header_meta,
            true,
            None,
        );
        self.rule(MARGIN, MARGIN + 48.0, CONTENT_WIDTH, colors::BORDER);
    }

    // ── Kop & kaki custom ──────────────────────────────────────────────────

    fn measure_region(
        &mut self,
        blocks: Vec<FormLayoutBlock>,
        max_height: f64,
    ) -> Option<LayoutRegion> {
        if blocks.is_empty() {
            return None;
        }
        let pages = Pages {
            page: MEASURE_PAGE_PLACEHOLDER.to_owned(),
            page_count: MEASURE_PAGE_PLACEHOLDER.to_owned(),
        };
        let mut height = 0.0;
        for row in layout_rows(&blocks) {
            let row_height = self.layout_row_height(&row, &pages);
            let next = if height == 0.0 {
                row_height
            } else {
                height + LAYOUT_ROW_GAP + row_height
            };
            if next > max_height {
                break;
            }
            height = next;
        }
        Some(LayoutRegion { blocks, height })
    }

    fn layout_row_height(&mut self, row: &[PlacedBlock<'_>], pages: &Pages) -> f64 {
        row.iter().fold(0.0, |tallest: f64, cell| {
            tallest.max(self.render_block(cell.block, cell.x, 0.0, cell.width, pages, false))
        })
    }

    fn draw_region(&mut self, region: &LayoutRegion, top: f64, pages: &Pages) {
        let mut y = top;
        let bottom = top + region.height;
        for row in layout_rows(&region.blocks) {
            let heights: Vec<f64> = row
                .iter()
                .map(|cell| self.render_block(cell.block, cell.x, 0.0, cell.width, pages, false))
                .collect();
            let row_height = heights.iter().fold(0.0_f64, |a, b| a.max(*b));
            if y + row_height > bottom + 0.5 {
                break;
            }
            for (position, cell) in row.iter().enumerate() {
                let offset = (row_height - heights.get(position).copied().unwrap_or(0.0)) / 2.0;
                self.render_block(cell.block, cell.x, y + offset, cell.width, pages, true);
            }
            y += row_height + LAYOUT_ROW_GAP;
        }
    }

    fn render_block(
        &mut self,
        block: &FormLayoutBlock,
        x: f64,
        y: f64,
        width: f64,
        pages: &Pages,
        draw: bool,
    ) -> f64 {
        match block.ty.as_str() {
            "text" => {
                let text = self.resolve_layout_text(block.text.as_deref().unwrap_or(""), pages);
                self.line(&text, x, y, width, &layout_text_style(block), draw, None)
            }
            "image" => self.render_layout_image(block, x, y, width, draw),
            "divider" => {
                let thickness =
                    config_number(block, "thickness").unwrap_or(DEFAULT_LAYOUT_DIVIDER_THICKNESS);
                if draw {
                    let line_y = y + LAYOUT_DIVIDER_PADDING + thickness / 2.0;
                    self.doc.stroke_color(Rgb::hex(
                        config_string(block, "color").unwrap_or(colors::BORDER),
                    ));
                    self.doc.line_width(thickness);
                    if config_string(block, "style") == Some("dashed") {
                        self.doc.dash(4.0, 3.0);
                    }
                    self.doc.move_to(x, line_y);
                    self.doc.line_to(x + width, line_y);
                    self.doc.stroke();
                    self.doc.undash();
                }
                thickness + LAYOUT_DIVIDER_PADDING * 2.0
            }
            "spacer" => config_number(block, "height").unwrap_or(DEFAULT_LAYOUT_SPACER_HEIGHT),
            "row" => {
                let children = block.blocks.as_deref().unwrap_or(&[]);
                if children.is_empty() {
                    return 0.0;
                }
                let spans: Vec<f64> = children.iter().map(|c| clamp_span(c.col_span)).collect();
                let slots = place_slots(
                    &spans,
                    x,
                    width,
                    config_string(block, "justify").unwrap_or("stretch"),
                );
                let fallback = Slot { x, width };
                let measured: Vec<f64> = children
                    .iter()
                    .enumerate()
                    .map(|(i, child)| {
                        let slot = slots.get(i).copied().unwrap_or(fallback);
                        self.render_block(child, slot.x, 0.0, slot.width, pages, false)
                    })
                    .collect();
                let row_height = measured.iter().fold(0.0_f64, |a, b| a.max(*b));
                if draw {
                    for (i, child) in children.iter().enumerate() {
                        let slot = slots.get(i).copied().unwrap_or(fallback);
                        let offset = (row_height - measured.get(i).copied().unwrap_or(0.0)) / 2.0;
                        self.render_block(child, slot.x, y + offset, slot.width, pages, true);
                    }
                }
                row_height
            }
            _ => 0.0,
        }
    }

    fn render_layout_image(
        &mut self,
        block: &FormLayoutBlock,
        x: f64,
        y: f64,
        width: f64,
        draw: bool,
    ) -> f64 {
        let media_key = block.asset_uuid.as_deref().unwrap_or("").to_lowercase();
        let Some(entry) = self
            .data
            .layout_media
            .as_ref()
            .and_then(|m| m.get(&media_key))
        else {
            return 0.0;
        };
        let Some(opened) = self.open_image(&format!("layout:{media_key}"), &entry.buffer) else {
            return 0.0;
        };
        let max_height = config_number(block, "height").unwrap_or(DEFAULT_LAYOUT_IMAGE_HEIGHT);
        let (iw, ih) = (f64::from(opened.width), f64::from(opened.height));
        let scale = (width / iw).min(max_height / ih);
        let draw_width = iw * scale;
        let draw_height = ih * scale;
        if draw {
            let left = match align_of(block) {
                Align::Center => x + (width - draw_width) / 2.0,
                Align::Right => x + width - draw_width,
                Align::Left => x,
            };
            self.doc.image(opened, left, y, draw_width, draw_height);
        }
        draw_height
    }

    /// `resolveLayoutText` — nama tak dikenal dibiarkan apa adanya.
    fn resolve_layout_text(&self, text: &str, pages: &Pages) -> String {
        LAYOUT_VARIABLE_PATTERN
            .replace_all(text, |caps: &regex::Captures<'_>| {
                let whole = caps.get(0).map_or("", |m| m.as_str());
                let name = caps.get(1).map_or("", |m| m.as_str());
                self.layout_variable(name, pages)
                    .unwrap_or_else(|| whole.to_owned())
            })
            .into_owned()
    }

    fn layout_variable(&self, name: &str, pages: &Pages) -> Option<String> {
        let d = self.data;
        let or_mark = |s: &str| {
            if s.is_empty() {
                EMPTY_MARK.to_owned()
            } else {
                s.to_owned()
            }
        };
        Some(match name {
            "companyName" => d.company_name.clone(),
            "templateName" => d.template_name.clone(),
            "templateVersion" => d
                .template_version
                .map_or_else(|| EMPTY_MARK.to_owned(), |v| format!("v{v}")),
            "status" => self.status_label().to_owned(),
            "storeName" => or_mark(&d.store_name),
            "storeCode" => or_mark(d.store_code.as_deref().unwrap_or("")),
            "salesName" => or_mark(&d.sales_name),
            "visitNumber" => or_mark(&d.visit_number),
            "checkInAt" => self.format_instant(d.check_in_at),
            "checkOutAt" => self.format_optional_instant(d.check_out_at),
            "submittedAt" => self.format_optional_instant(d.submitted_at),
            "generatedAt" => self.format_instant(d.generated_at),
            "generatedBy" => or_mark(&d.generated_by),
            "page" => pages.page.clone(),
            "pageCount" => pages.page_count.clone(),
            _ => return None,
        })
    }

    fn draw_context_card(&mut self) {
        let d = self.data;
        let store = match d.store_code.as_deref() {
            Some(code) if !code.is_empty() => format!("{} ({code})", d.store_name),
            _ => d.store_name.clone(),
        };
        let visit = if d.visit_number.is_empty() {
            EMPTY_MARK.to_owned()
        } else {
            d.visit_number.clone()
        };
        let entries: Vec<(&str, String)> = vec![
            ("Store", store),
            ("Sales", d.sales_name.clone()),
            ("Visit ID", visit),
            ("Check-in", self.format_instant(d.check_in_at)),
            ("Check-out", self.format_optional_instant(d.check_out_at)),
            ("Submitted at", self.format_optional_instant(d.submitted_at)),
        ];
        let (label_style, value_style) = (self.st.card_label, self.st.card_value);

        let column_width = (CONTENT_WIDTH - CARD_PADDING * 2.0 - GUTTER) / 2.0;
        let row_count = entries.len().div_ceil(2);
        let mut row_heights = vec![0.0_f64; row_count];
        for (index, (label, value)) in entries.iter().enumerate() {
            let height = self.height_of(label, column_width, &label_style)
                + 2.0
                + self.height_of(value, column_width, &value_style);
            if let Some(slot) = row_heights.get_mut(index / 2) {
                *slot = slot.max(height);
            }
        }
        let inner_height: f64 = row_heights.iter().sum::<f64>() + (row_count as f64 - 1.0) * 8.0;
        let card_height = inner_height + CARD_PADDING * 2.0;

        self.ensure_space(card_height + ROW_GAP);
        self.doc
            .rounded_rect(MARGIN, self.y, CONTENT_WIDTH, card_height, 8.0);
        self.doc
            .fill_and_stroke(Rgb::hex(colors::SURFACE), Rgb::hex(colors::BORDER));

        for (index, (label, value)) in entries.iter().enumerate() {
            let row = index / 2;
            let column = (index % 2) as f64;
            let offset: f64 = row_heights.iter().take(row).map(|h| h + 8.0).sum();
            let x = MARGIN + CARD_PADDING + column * (column_width + GUTTER);
            let top = self.y + CARD_PADDING + offset;
            let label_height = self.line(label, x, top, column_width, &label_style, true, None);
            self.line(
                value,
                x,
                top + label_height + 2.0,
                column_width,
                &value_style,
                true,
                None,
            );
        }
        self.y += card_height + ROW_GAP;
    }

    fn draw_banners(&mut self) {
        if self.data.status == PdfStatus::Draft {
            let s = self.st.banner_amber;
            self.draw_banner(
                "DRAFT \u{2014} this form was never submitted; the answers below may be incomplete.",
                colors::AMBER_SOFT,
                colors::AMBER_BORDER,
                &s,
            );
        }
        if self.data.revision_count > 0 {
            let when = self.format_optional_instant(self.data.last_edited_at);
            let who = self
                .data
                .last_edited_by_name
                .clone()
                .unwrap_or_else(|| "an unknown user".to_owned());
            let s = self.st.banner_blue;
            let text = format!(
                "Corrected {}\u{d7} \u{2014} last edited {when} by {who}.",
                self.data.revision_count
            );
            self.draw_banner(&text, colors::PRIMARY_SOFT, colors::PRIMARY_BORDER, &s);
        }
    }

    fn draw_banner(&mut self, text: &str, background: &str, border: &str, s: &TextStyle) {
        let inner_width = CONTENT_WIDTH - CARD_PADDING * 2.0;
        let height = self.height_of(text, inner_width, s) + 16.0;
        self.ensure_space(height + ROW_GAP);
        self.doc
            .rounded_rect(MARGIN, self.y, CONTENT_WIDTH, height, 6.0);
        self.doc
            .fill_and_stroke(Rgb::hex(background), Rgb::hex(border));
        let y = self.y + 8.0;
        self.line(text, MARGIN + CARD_PADDING, y, inner_width, s, true, None);
        self.y += height + ROW_GAP;
    }

    // ── Grid ───────────────────────────────────────────────────────────────

    fn draw_scope(&mut self, fields: &[FormNode], values: &Map<String, Value>, x: f64, width: f64) {
        let mut row: Vec<&FormNode> = Vec::new();
        let mut span = 0.0;
        let grid = GRID_COLUMNS as f64;
        for node in fields {
            if !is_node_visible(node, values) {
                continue;
            }
            let node_span = clamp_span(node.col_span);
            if node.ty.is_container() {
                if !row.is_empty() {
                    self.draw_row(&row, values, x, width, None);
                    row.clear();
                    span = 0.0;
                }
                let w = column_width(width, node_span, true);
                self.draw_container(node, values, x, w);
                continue;
            }
            if span + node_span > grid && !row.is_empty() {
                self.draw_row(&row, values, x, width, None);
                row.clear();
                span = 0.0;
            }
            row.push(node);
            span += node_span;
        }
        if !row.is_empty() {
            self.draw_row(&row, values, x, width, None);
        }
    }

    fn draw_row(
        &mut self,
        nodes: &[&FormNode],
        values: &Map<String, Value>,
        x: f64,
        width: f64,
        justify: Option<&str>,
    ) {
        let placed = place_row(nodes, x, width, justify);
        let y = self.y;
        let heights: Vec<f64> = placed
            .iter()
            .map(|(node, slot)| self.render_leafish(node, values, slot.x, y, slot.width, false))
            .collect();
        let row_height = heights.iter().fold(0.0_f64, |a, b| a.max(*b));
        self.ensure_space(row_height);
        let top = self.y;
        for (node, slot) in &placed {
            self.render_leafish(node, values, slot.x, top, slot.width, true);
        }
        self.y = top + row_height + ROW_GAP;
    }

    // ── Node ───────────────────────────────────────────────────────────────

    fn render_leafish(
        &mut self,
        node: &FormNode,
        values: &Map<String, Value>,
        x: f64,
        y: f64,
        width: f64,
        draw: bool,
    ) -> f64 {
        match node.ty {
            FormNodeType::Heading => {
                let s = self.st.heading;
                self.line(
                    node.label.as_deref().unwrap_or(""),
                    x,
                    y,
                    width,
                    &s,
                    draw,
                    None,
                )
            }
            FormNodeType::Paragraph => {
                let s = self.st.paragraph;
                self.line(
                    node.label.as_deref().unwrap_or(""),
                    x,
                    y,
                    width,
                    &s,
                    draw,
                    None,
                )
            }
            FormNodeType::Divider => {
                if draw {
                    self.rule(x, y + 8.0, width, colors::BORDER);
                }
                17.0
            }
            FormNodeType::Section | FormNodeType::Group | FormNodeType::Repeater => 0.0,
            _ => self.render_field(node, values, x, y, width, draw),
        }
    }

    fn render_field(
        &mut self,
        node: &FormNode,
        values: &Map<String, Value>,
        x: f64,
        y: f64,
        width: f64,
        draw: bool,
    ) -> f64 {
        let mut height = 0.0;
        if let Some(label) = node.label.as_deref().filter(|l| !l.is_empty()) {
            let text = if node.is_required() {
                format!("{label} *")
            } else {
                label.to_owned()
            };
            let s = self.st.field_label;
            height += self.line(&text, x, y + height, width, &s, draw, None) + 3.0;
        }
        if let Some(desc) = node.description.as_deref().filter(|d| !d.is_empty()) {
            let s = self.st.field_note;
            height += self.line(desc, x, y + height, width, &s, draw, None) + 3.0;
        }
        height += self.render_value(
            node,
            values.get(&node.key),
            x,
            y + height,
            width,
            draw,
            MAX_IMAGE_HEIGHT,
            false,
        );
        height
    }

    /// `renderValue` — nilai satu daun tanpa label (field biasa & sel tabel repeater).
    #[allow(clippy::too_many_arguments)]
    fn render_value(
        &mut self,
        node: &FormNode,
        value: Option<&Value>,
        x: f64,
        y: f64,
        width: f64,
        draw: bool,
        image_height: f64,
        compact: bool,
    ) -> f64 {
        let text_style = if compact {
            self.st.table_cell
        } else {
            self.st.value
        };
        let empty_style = if compact {
            self.st.table_cell_empty
        } else {
            self.st.value_empty
        };
        let cap = Some(self.usable_height - 40.0);
        let empty = |r: &mut Self| r.line(EMPTY_MARK, x, y, width, &empty_style, draw, None);
        let is_empty = is_empty_value(value);
        let text =
            |r: &mut Self, content: &str| r.line(content, x, y, width, &text_style, draw, cap);

        match node.ty {
            FormNodeType::Signature | FormNodeType::Photo => {
                self.render_image(value, x, y, width, draw, image_height, &empty_style)
            }
            FormNodeType::File => {
                let Some(reference) = read_media_reference(value) else {
                    return empty(self);
                };
                let name = reference
                    .content_type
                    .clone()
                    .or_else(|| {
                        self.data
                            .media
                            .get(&reference.file_uuid)
                            .map(|m| m.content_type.clone())
                    })
                    .unwrap_or_else(|| "Attachment".to_owned());
                let size = reference
                    .size_bytes
                    .map(|b| format!("  \u{b7}  {}", format_bytes(b)))
                    .unwrap_or_default();
                let s = if compact {
                    self.st.table_cell
                } else {
                    self.st.value
                };
                self.line(&format!("{name}{size}"), x, y, width, &s, draw, None)
            }
            FormNodeType::Location => {
                let Some((lat, lng, accuracy)) = read_location(value) else {
                    return empty(self);
                };
                let acc = accuracy
                    .map(|a| format!(" (\u{b1}{} m)", mz_core::jsfmt::num_string(js_round(a))))
                    .unwrap_or_default();
                text(
                    self,
                    &format!(
                        "{}, {}{acc}",
                        mz_core::jsfmt::to_fixed(lat, 6),
                        mz_core::jsfmt::to_fixed(lng, 6)
                    ),
                )
            }
            FormNodeType::Rating => {
                if is_empty {
                    return empty(self);
                }
                let max = node
                    .config_value("max")
                    .filter(|v| !v.is_null())
                    .map_or(DEFAULT_RATING_MAX, js_number);
                let shown = value.map(js_string).unwrap_or_default();
                text(
                    self,
                    &format!("{shown} / {}", mz_core::jsfmt::num_string(max)),
                )
            }
            FormNodeType::Boolean => {
                if is_empty {
                    return empty(self);
                }
                let yes = value == Some(&Value::Bool(true));
                text(self, if yes { "Yes" } else { "No" })
            }
            FormNodeType::Number | FormNodeType::Percent => {
                if is_empty {
                    return empty(self);
                }
                let formatted = number_or_string(value);
                if node.ty == FormNodeType::Percent {
                    text(self, &format!("{formatted}%"))
                } else {
                    text(self, &formatted)
                }
            }
            FormNodeType::Currency => {
                if is_empty {
                    return empty(self);
                }
                let code = match node.config_value("currencyCode") {
                    None | Some(Value::Null) => "IDR".to_owned(),
                    Some(v) => js_string(v),
                };
                let formatted = number_or_string(value);
                text(self, &format!("{code} {formatted}"))
            }
            FormNodeType::Date => {
                if is_empty {
                    return empty(self);
                }
                let raw = value.map(js_string).unwrap_or_default();
                let shown = format::format_date(&raw).unwrap_or(raw);
                text(self, &shown)
            }
            FormNodeType::Time => {
                if is_empty {
                    return empty(self);
                }
                let raw = value.map(js_string).unwrap_or_default();
                text(self, &raw)
            }
            FormNodeType::Datetime => {
                if is_empty {
                    return empty(self);
                }
                let shown = match value {
                    Some(Value::Number(n)) => n
                        .as_f64()
                        .map(|ms| format::format_epoch_ms(ms, &self.data.time_zone))
                        .unwrap_or_default(),
                    Some(v) => js_string(v),
                    None => String::new(),
                };
                text(self, &shown)
            }
            FormNodeType::Select | FormNodeType::Radio => {
                if is_empty {
                    return empty(self);
                }
                let label = option_label(node, value);
                text(self, &label)
            }
            FormNodeType::Multiselect | FormNodeType::Checkbox => {
                if is_empty {
                    return empty(self);
                }
                let joined = match value {
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|i| option_label(node, Some(i)))
                        .collect::<Vec<_>>()
                        .join("  \u{b7}  "),
                    other => option_label(node, other),
                };
                text(self, &joined)
            }
            FormNodeType::Url => {
                if is_empty {
                    return empty(self);
                }
                let url = value.map(js_string).unwrap_or_default();
                let s = self.st.value_link;
                let height = self.line(&url, x, y, width, &s, draw, cap);
                if draw {
                    self.doc.link(x, y, width, height, &url);
                }
                height
            }
            FormNodeType::Textarea
            | FormNodeType::Text
            | FormNodeType::Email
            | FormNodeType::Phone
            | FormNodeType::Barcode => {
                if is_empty {
                    return empty(self);
                }
                let raw = value.map(js_string).unwrap_or_default();
                text(self, &raw)
            }
            FormNodeType::Heading
            | FormNodeType::Paragraph
            | FormNodeType::Divider
            | FormNodeType::Section
            | FormNodeType::Group
            | FormNodeType::Repeater => 0.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_image(
        &mut self,
        value: Option<&Value>,
        x: f64,
        y: f64,
        width: f64,
        draw: bool,
        max_height: f64,
        empty_style: &TextStyle,
    ) -> f64 {
        let Some(reference) = read_media_reference(value) else {
            return self.line(EMPTY_MARK, x, y, width, empty_style, draw, None);
        };
        let opened = match self.data.media.get(&reference.file_uuid) {
            Some(entry) => self.open_image(&reference.file_uuid, &entry.buffer),
            None => None,
        };
        let Some(opened) = opened else {
            return self.render_missing_image(x, y, width, draw);
        };
        let (iw, ih) = (f64::from(opened.width), f64::from(opened.height));
        let scale = (width / iw).min(max_height / ih).min(1.0);
        let draw_width = iw * scale;
        let draw_height = ih * scale;
        if draw {
            self.doc.image(opened, x, y, draw_width, draw_height);
            self.doc.stroke_color(Rgb::hex(colors::BORDER));
            self.doc.line_width(0.5);
            self.doc.rect(x, y, draw_width, draw_height);
            self.doc.stroke();
        }
        draw_height
    }

    fn render_missing_image(&mut self, x: f64, y: f64, width: f64, draw: bool) -> f64 {
        if draw {
            self.doc.dash(3.0, 3.0);
            self.doc.stroke_color(Rgb::hex(colors::BORDER));
            self.doc.line_width(0.5);
            self.doc.rect(x, y, width, PLACEHOLDER_HEIGHT);
            self.doc.stroke();
            self.doc.undash();
            let s = self.st.value_note.aligned(Align::Center);
            self.line(
                "Image unavailable",
                x,
                y + PLACEHOLDER_HEIGHT / 2.0 - 5.0,
                width,
                &s,
                true,
                None,
            );
        }
        PLACEHOLDER_HEIGHT
    }

    fn open_image(&mut self, key: &str, buffer: &[u8]) -> Option<OpenedImage> {
        if let Some(cached) = self.image_cache.get(key) {
            return *cached;
        }
        let opened = self.doc.open_image(buffer);
        self.image_cache.insert(key.to_owned(), opened);
        opened
    }

    // ── Container ──────────────────────────────────────────────────────────

    fn draw_container(&mut self, node: &FormNode, values: &Map<String, Value>, x: f64, width: f64) {
        match node.ty {
            FormNodeType::Section => self.draw_section(node, values, x, width),
            FormNodeType::Group => self.draw_group(node, values, x, width),
            _ => self.draw_repeater(node, values, x, width),
        }
    }

    fn draw_section(&mut self, node: &FormNode, values: &Map<String, Value>, x: f64, width: f64) {
        let bar_height = 22.0;
        self.ensure_space(bar_height + SECTION_ORPHAN_GUARD);
        self.doc.rect(x, self.y, width, bar_height);
        self.doc.fill(Rgb::hex(colors::SURFACE));
        let s = self.st.section_label;
        let y = self.y + 6.0;
        self.line(
            node.label.as_deref().unwrap_or(""),
            x + 8.0,
            y,
            width - 16.0,
            &s,
            true,
            None,
        );
        self.y += bar_height + 8.0;
        if let Some(desc) = node.description.as_deref().filter(|d| !d.is_empty()) {
            let s = self.st.section_note;
            let y = self.y;
            let height = self.line(desc, x, y, width, &s, true, None);
            self.y += height + 6.0;
        }
        self.draw_scope(node.children(), values, x, width);
    }

    fn draw_group(&mut self, node: &FormNode, values: &Map<String, Value>, x: f64, width: f64) {
        self.ensure_space(28.0);
        if let Some(label) = node.label.as_deref().filter(|l| !l.is_empty()) {
            let s = self.st.group_label;
            let y = self.y;
            let height = self.line(label, x, y, width, &s, true, None);
            self.y += height + 3.0;
        }
        let y = self.y;
        self.rule(x, y, width, colors::HAIRLINE);
        self.y += 8.0;
        if let Some(desc) = node.description.as_deref().filter(|d| !d.is_empty()) {
            let s = self.st.field_note;
            let y = self.y;
            let height = self.line(desc, x, y, width, &s, true, None);
            self.y += height + 6.0;
        }
        let empty = Map::new();
        let scope = match values.get(&node.key) {
            Some(Value::Object(o)) => o,
            _ => &empty,
        };
        let indent = 12.0;
        if node.config_value("direction").and_then(Value::as_str) == Some("row") {
            let visible: Vec<&FormNode> = node
                .children()
                .iter()
                .filter(|f| is_node_visible(f, scope))
                .collect();
            let justify = node
                .config_value("justify")
                .and_then(Value::as_str)
                .unwrap_or("stretch");
            if !visible.is_empty() {
                self.draw_row(&visible, scope, x + indent, width - indent, Some(justify));
            }
            return;
        }
        self.draw_scope(node.children(), scope, x + indent, width - indent);
    }

    fn draw_repeater(&mut self, node: &FormNode, values: &Map<String, Value>, x: f64, width: f64) {
        self.ensure_space(46.0);
        if let Some(label) = node.label.as_deref().filter(|l| !l.is_empty()) {
            let text = if node.is_required() {
                format!("{label} *")
            } else {
                label.to_owned()
            };
            let s = self.st.group_label;
            let y = self.y;
            let height = self.line(&text, x, y, width, &s, true, None);
            self.y += height + 3.0;
        }
        if let Some(desc) = node.description.as_deref().filter(|d| !d.is_empty()) {
            let s = self.st.field_note;
            let y = self.y;
            let height = self.line(desc, x, y, width, &s, true, None);
            self.y += height + 4.0;
        }

        let rows: &[Value] = match values.get(&node.key) {
            Some(Value::Array(a)) => a,
            _ => &[],
        };
        let columns = repeater_columns(node);
        if rows.is_empty() || columns.is_empty() {
            let s = self.st.value_note;
            let y = self.y;
            let height = self.line("No rows.", x, y, width, &s, true, None);
            self.y += height + ROW_GAP;
            return;
        }

        let total_span: f64 = columns.iter().map(|c| clamp_span(c.node.col_span)).sum();
        let available = width - NUMBER_COLUMN_WIDTH;
        let widths: Vec<f64> = columns
            .iter()
            .map(|c| (available * clamp_span(c.node.col_span)) / total_span)
            .collect();

        self.draw_repeater_header(&columns, &widths, x, width);

        let empty = Map::new();
        let (cell_empty, index_style) = (self.st.table_cell_empty, self.st.table_index);
        for (index, row) in rows.iter().enumerate() {
            let scope = row.as_object().unwrap_or(&empty);
            struct Cell<'s> {
                scope: &'s Map<String, Value>,
                visible: bool,
                width: f64,
                height: f64,
            }
            let mut cells: Vec<Cell<'_>> = Vec::with_capacity(columns.len());
            for (position, column) in columns.iter().enumerate() {
                let cell_width = widths.get(position).copied().unwrap_or(0.0) - CELL_PADDING * 2.0;
                let cell_scope = repeater_cell_scope(column, scope, &empty);
                let visible = is_node_visible(column.node, cell_scope);
                let y = self.y;
                let height = if visible {
                    self.render_value(
                        column.node,
                        cell_scope.get(&column.node.key),
                        x,
                        y,
                        cell_width,
                        false,
                        TABLE_IMAGE_HEIGHT,
                        true,
                    )
                } else {
                    0.0
                };
                cells.push(Cell {
                    scope: cell_scope,
                    visible,
                    width: cell_width,
                    height,
                });
            }
            let row_height =
                cells.iter().fold(10.0_f64, |a, c| a.max(c.height)) + CELL_PADDING * 2.0;
            if self.y + row_height > self.content_bottom {
                self.add_page();
                self.draw_repeater_header(&columns, &widths, x, width);
            }
            let top = self.y;
            self.line(
                &(index + 1).to_string(),
                x,
                top + CELL_PADDING,
                NUMBER_COLUMN_WIDTH,
                &index_style,
                true,
                None,
            );
            let mut cursor = x + NUMBER_COLUMN_WIDTH;
            for (position, (cell, column)) in cells.iter().zip(&columns).enumerate() {
                if cell.visible {
                    self.render_value(
                        column.node,
                        cell.scope.get(&column.node.key),
                        cursor + CELL_PADDING,
                        top + CELL_PADDING,
                        cell.width,
                        true,
                        TABLE_IMAGE_HEIGHT,
                        true,
                    );
                } else {
                    self.line(
                        EMPTY_MARK,
                        cursor + CELL_PADDING,
                        top + CELL_PADDING,
                        cell.width,
                        &cell_empty,
                        true,
                        None,
                    );
                }
                cursor += widths.get(position).copied().unwrap_or(0.0);
            }
            self.y = top + row_height;
            let y = self.y;
            self.rule(x, y, width, colors::HAIRLINE);
        }
        self.y += ROW_GAP;
    }

    fn draw_repeater_header(
        &mut self,
        columns: &[RepeaterColumn<'_>],
        widths: &[f64],
        x: f64,
        width: f64,
    ) {
        let labels: Vec<String> = columns
            .iter()
            .map(|c| {
                c.node
                    .label
                    .as_deref()
                    .unwrap_or(&c.node.key)
                    .to_uppercase()
            })
            .collect();
        let head = self.st.table_head;
        let mut tallest: f64 = 8.0;
        for (position, label) in labels.iter().enumerate() {
            let cell_width = widths.get(position).copied().unwrap_or(0.0) - CELL_PADDING * 2.0;
            tallest = tallest.max(self.height_of(label, cell_width, &head));
        }
        let header_height = tallest + CELL_PADDING * 2.0;
        self.ensure_space(header_height + 20.0);
        self.doc.rect(x, self.y, width, header_height);
        self.doc.fill(Rgb::hex(colors::SURFACE));
        let y = self.y;
        self.line(
            "#",
            x,
            y + CELL_PADDING,
            NUMBER_COLUMN_WIDTH,
            &head.aligned(Align::Center),
            true,
            None,
        );
        let mut cursor = x + NUMBER_COLUMN_WIDTH;
        for (position, label) in labels.iter().enumerate() {
            let w = widths.get(position).copied().unwrap_or(0.0);
            self.line(
                label,
                cursor + CELL_PADDING,
                y + CELL_PADDING,
                w - CELL_PADDING * 2.0,
                &head,
                true,
                None,
            );
            cursor += w;
        }
        self.y += header_height;
        let y = self.y;
        self.rule(x, y, width, colors::BORDER);
    }

    // ── Format waktu ───────────────────────────────────────────────────────

    fn format_instant(&self, date: DateTime<Utc>) -> String {
        format_instant(date, &self.data.time_zone)
    }

    fn format_optional_instant(&self, date: Option<DateTime<Utc>>) -> String {
        date.map_or_else(|| EMPTY_MARK.to_owned(), |d| self.format_instant(d))
    }
}

fn column_width(width: f64, span: f64, alone: bool) -> f64 {
    let raw = (width * span) / GRID_COLUMNS as f64;
    if alone { raw } else { (raw - GUTTER).max(1.0) }
}

/// `typeof value === 'number' ? numberFormatter.format(value) : String(value)`.
fn number_or_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(n)) => n.as_f64().map(intl_number).unwrap_or_default(),
        Some(v) => js_string(v),
        None => "undefined".to_owned(),
    }
}

/// `layoutRows` — grid 12 kolom kop/kaki.
fn layout_rows(blocks: &[FormLayoutBlock]) -> Vec<Vec<PlacedBlock<'_>>> {
    let grid = GRID_COLUMNS as f64;
    let mut rows: Vec<Vec<&FormLayoutBlock>> = Vec::new();
    let mut current: Vec<&FormLayoutBlock> = Vec::new();
    let mut span = 0.0;
    for block in blocks {
        let block_span = clamp_span(block.col_span);
        if span + block_span > grid && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            span = 0.0;
        }
        current.push(block);
        span += block_span;
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows.into_iter()
        .map(|row| {
            let alone = row.len() == 1;
            let mut cursor = 0.0;
            row.into_iter()
                .map(|block| {
                    let block_span = clamp_span(block.col_span);
                    let x = MARGIN + (CONTENT_WIDTH * cursor) / grid;
                    cursor += block_span;
                    let raw = (CONTENT_WIDTH * block_span) / grid;
                    PlacedBlock {
                        block,
                        x,
                        width: if alone { raw } else { (raw - GUTTER).max(1.0) },
                    }
                })
                .collect()
        })
        .collect()
}

/// `placeRow` — posisi & lebar sel satu baris.
fn place_row<'n>(
    nodes: &[&'n FormNode],
    x: f64,
    width: f64,
    justify: Option<&str>,
) -> Vec<(&'n FormNode, Slot)> {
    let alone = nodes.len() == 1;
    let grid = GRID_COLUMNS as f64;
    let Some(justify) = justify else {
        let mut cursor = 0.0;
        return nodes
            .iter()
            .map(|node| {
                let span = clamp_span(node.col_span);
                let cell_x = x + (width * cursor) / grid;
                cursor += span;
                let raw = (width * span) / grid;
                (
                    *node,
                    Slot {
                        x: cell_x,
                        width: if alone { raw } else { (raw - GUTTER).max(1.0) },
                    },
                )
            })
            .collect();
    };
    let spans: Vec<f64> = nodes.iter().map(|n| clamp_span(n.col_span)).collect();
    let slots = place_slots(&spans, x, width, justify);
    nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (*node, slots.get(i).copied().unwrap_or(Slot { x, width })))
        .collect()
}

fn repeater_columns(node: &FormNode) -> Vec<RepeaterColumn<'_>> {
    let mut columns = Vec::new();
    for child in node.children() {
        if child.ty.is_display() {
            continue;
        }
        if child.ty.is_container() {
            for grand in child.children() {
                if grand.ty.is_display() || grand.ty.is_container() {
                    continue;
                }
                columns.push(RepeaterColumn {
                    node: grand,
                    group_key: Some(&child.key),
                });
            }
            continue;
        }
        columns.push(RepeaterColumn {
            node: child,
            group_key: None,
        });
    }
    columns
}

fn repeater_cell_scope<'s>(
    column: &RepeaterColumn<'_>,
    row: &'s Map<String, Value>,
    empty: &'s Map<String, Value>,
) -> &'s Map<String, Value> {
    match column.group_key {
        None => row,
        Some(k) => match row.get(k) {
            Some(Value::Object(o)) => o,
            _ => empty,
        },
    }
}

fn render_doc(data: &SubmissionPdfData) -> Doc {
    Renderer::new(data).render()
}

/// `FormSubmissionPdfGenerator.generate` (`form-submission-pdf.generator.ts:1783`) — byte PDF.
pub fn generate_submission_pdf(data: &SubmissionPdfData) -> Vec<u8> {
    render_doc(data).finish(true)
}

#[cfg(test)]
mod tests;
