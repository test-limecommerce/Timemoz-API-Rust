//! Penulis PDF Rust murni yang meniru `pdfkit` 0.17 untuk generator submission form.
//!
//! Versi diperluas dari `mz-billing::pdf` (metrik AFM + putus baris bersama: `mz-pdfkit`;
//! penulisnya sengaja belum disatukan karena keluarannya berbeda, mis. format komponen warna
//! dan kompresi aliran — menyatukannya mengubah byte PDF salah satu sisi). Tambahan terhadap
//! versi billing: empat font Helvetica (regular/bold/oblique/bold-oblique, metrik oblique =
//! tegak), `lineGap`, opsi `height` + `ellipsis` LineWrapper, `heightOfString`, halaman
//! ber-buffer (`switchToPage`), margin atas/bawah per halaman (pemenggalan halaman otomatis
//! saat teks melewati `page.maxY()`), banyak gambar (PNG → Flate + SMask alfa, JPEG →
//! DCTDecode apa adanya) yang ditanam sekali, garis putus-putus, `fillAndStroke`, tautan.
//!
//! Beda dari pdfkit: karakter di luar WinAnsi ditulis `?`; PNG di-dekode lalu ditulis ulang
//! (bukan IDAT mentah + predictor); orientasi EXIF JPEG diabaikan.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;

use mz_pdfkit::{afm, breaks};

pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    /// `#rrggbb` / `#rgb` (pdfkit: `parseInt(hex)/255`). Selain hex → hitam.
    pub fn hex(s: &str) -> Self {
        let h = s.trim_start_matches('#');
        let expanded: String = if h.len() == 3 {
            h.chars().flat_map(|c| [c, c]).collect()
        } else {
            h.to_owned()
        };
        let c = |i: usize| {
            expanded
                .get(i..i + 2)
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .map_or(0.0, |v| f64::from(v) / 255.0)
        };
        Self(c(0), c(2), c(4))
    }

    /// pdfkit menulis komponen warna apa adanya (`String(number)`), tanpa pembulatan.
    fn ops(self) -> String {
        use mz_core::jsfmt::num_string;
        format!(
            "{} {} {}",
            num_string(self.0),
            num_string(self.1),
            num_string(self.2)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Font {
    fn base_name(self) -> &'static str {
        match self {
            Self::Regular => "Helvetica",
            Self::Bold => "Helvetica-Bold",
            Self::Italic => "Helvetica-Oblique",
            Self::BoldItalic => "Helvetica-BoldOblique",
        }
    }

    fn is_bold(self) -> bool {
        matches!(self, Self::Bold | Self::BoldItalic)
    }

    fn widths(self) -> &'static [u16; 256] {
        if self.is_bold() {
            &afm::W_BOLD
        } else {
            &afm::W_REGULAR
        }
    }

    fn kerns(self) -> &'static [(u16, i16)] {
        if self.is_bold() {
            afm::KERN_BOLD
        } else {
            afm::KERN_REGULAR
        }
    }

    fn kern(self, a: u8, b: u8) -> i32 {
        let key = (u16::from(a) << 8) | u16::from(b);
        let ks = self.kerns();
        ks.binary_search_by_key(&key, |(k, _)| *k)
            .ok()
            .and_then(|i| ks.get(i))
            .map_or(0, |(_, v)| i32::from(*v))
    }

    const ALL: [Font; 4] = [Self::Regular, Self::Bold, Self::Italic, Self::BoldItalic];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Right,
    Center,
}

/// Opsi `doc.text(...)` yang dipakai generator.
#[derive(Debug, Clone, Copy)]
pub struct TextOpts {
    pub font: Font,
    pub size: f64,
    pub color: Rgb,
    pub width: f64,
    pub align: Align,
    pub line_gap: f64,
    /// `height` LineWrapper (baris berlebih dibuang).
    pub height: Option<f64>,
    /// `ellipsis: true` (`…` di baris terakhir yang muat).
    pub ellipsis: bool,
}

/// Bilangan PDF pdfkit: dibulatkan 6 desimal, tanpa nol buntut.
pub fn num(v: f64) -> String {
    let r = round6(v);
    if r == 0.0 {
        "0".to_owned()
    } else {
        format!("{r}")
    }
}

/// `PDFNumber`: `Math.round(n * 1e6) / 1e6`.
pub fn round6(v: f64) -> f64 {
    if !v.is_finite() {
        return v;
    }
    (v * 1e6).round() / 1e6
}

/// `PDFNumber` LineWrapper pdfkit 0.17: float32 terbesar yang ≤ `n` (bukan pembulatan
/// 6 desimal — itu `PDFObject.number`, lihat [`num`]).
pub fn pdf_number(n: f64) -> f64 {
    let r = n as f32;
    if f64::from(r) <= n || r.is_nan() {
        return f64::from(r);
    }
    let bits = r.to_bits();
    let adjusted = if n <= 0.0 {
        f32::from_bits(bits + 1)
    } else {
        f32::from_bits(bits - 1)
    };
    f64::from(adjusted)
}

/// Kode WinAnsi satu karakter (`None` = tak ada; pemanggil memakai `?`).
fn win_ansi(c: char) -> Option<u8> {
    let u = c as u32;
    Some(match u {
        0x20..=0x7E | 0xA0..=0xFF => u as u8,
        0x20AC => 0x80,
        0x201A => 0x82,
        0x0192 => 0x83,
        0x201E => 0x84,
        0x2026 => 0x85,
        0x2020 => 0x86,
        0x2021 => 0x87,
        0x02C6 => 0x88,
        0x2030 => 0x89,
        0x0160 => 0x8A,
        0x2039 => 0x8B,
        0x0152 => 0x8C,
        0x017D => 0x8E,
        0x2018 => 0x91,
        0x2019 => 0x92,
        0x201C => 0x93,
        0x201D => 0x94,
        0x2022 => 0x95,
        0x2013 => 0x96,
        0x2014 => 0x97,
        0x02DC => 0x98,
        0x2122 => 0x99,
        0x0161 => 0x9A,
        0x203A => 0x9B,
        0x0153 => 0x9C,
        0x017E => 0x9E,
        0x0178 => 0x9F,
        0x0A => 0x0A,
        _ => return None,
    })
}

/// Kebalikan [`win_ansi`] (untuk ekstraksi teks di uji).
pub fn from_win_ansi(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}',
        0x85 => '\u{2026}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x99 => '\u{2122}',
        other => char::from(other),
    }
}

fn codes(s: &str) -> Vec<u8> {
    s.chars().map(|c| win_ansi(c).unwrap_or(b'?')).collect()
}

/// `font.widthOfString(s, size)` (lebar glyph + kerning).
pub fn text_width(s: &str, font: Font, size: f64) -> f64 {
    let cs = codes(s);
    let w = font.widths();
    let mut total = 0.0;
    for (i, c) in cs.iter().enumerate() {
        total += f64::from(w.get(usize::from(*c)).copied().unwrap_or(0));
        if let Some(n) = cs.get(i + 1) {
            total += f64::from(font.kern(*c, *n));
        }
    }
    // pdfkit: `width * (size / 1000)` — urutan operasi memengaruhi pembulatan.
    total * (size / 1000.0)
}

/// `currentLineHeight(true)`: Helvetica(-Oblique) 1,156 em, Helvetica-Bold(-Oblique) 1,19 em.
pub fn line_height(font: Font, size: f64) -> f64 {
    if font.is_bold() {
        size * 1.19
    } else {
        size * 1.156
    }
}

struct Annot {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    url: String,
}

struct Page {
    content: String,
    annots: Vec<Annot>,
    images: Vec<usize>,
    fonts: Vec<Font>,
    margin_top: f64,
    margin_bottom: f64,
}

impl Page {
    fn new(margin_top: f64, margin_bottom: f64) -> Self {
        Self {
            content: format!("1 0 0 -1 0 {} cm\n", num(PAGE_H)),
            annots: Vec::new(),
            images: Vec::new(),
            fonts: Vec::new(),
            margin_top,
            margin_bottom,
        }
    }
}

enum ImageData {
    /// Flate (RGB atau Gray) + SMask opsional.
    Flate {
        color_space: &'static str,
        data: Vec<u8>,
        alpha: Option<Vec<u8>>,
    },
    /// JPEG apa adanya.
    Dct {
        color_space: &'static str,
        bits: u8,
        data: Vec<u8>,
    },
}

struct EmbeddedImage {
    width: u32,
    height: u32,
    data: ImageData,
}

/// Gambar yang sudah dibuka (`doc.openImage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedImage {
    pub id: usize,
    pub width: u32,
    pub height: u32,
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Header JPEG seperti `pdfkit` `JPEG` (SOI + penanda SOF pertama).
fn parse_jpeg(bytes: &[u8]) -> Option<EmbeddedImage> {
    const SOF: [u16; 15] = [
        0xffc0, 0xffc1, 0xffc2, 0xffc3, 0xffc5, 0xffc6, 0xffc7, 0xffc8, 0xffc9, 0xffca, 0xffcb,
        0xffcc, 0xffcd, 0xffce, 0xffcf,
    ];
    let u16_at = |p: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*bytes.get(p)?, *bytes.get(p + 1)?]))
    };
    if u16_at(0)? != 0xffd8 {
        return None;
    }
    let mut pos = 2;
    let mut marker;
    loop {
        marker = u16_at(pos)?;
        pos += 2;
        if SOF.contains(&marker) {
            break;
        }
        pos += usize::from(u16_at(pos)?);
    }
    pos += 2;
    let bits = *bytes.get(pos)?;
    let height = u16_at(pos + 1)?;
    let width = u16_at(pos + 3)?;
    let channels = *bytes.get(pos + 5)?;
    let color_space = match channels {
        1 => "DeviceGray",
        3 => "DeviceRGB",
        4 => "DeviceCMYK",
        _ => return None,
    };
    Some(EmbeddedImage {
        width: u32::from(width),
        height: u32::from(height),
        data: ImageData::Dct {
            color_space,
            bits,
            data: bytes.to_vec(),
        },
    })
}

fn parse_png(bytes: &[u8]) -> Option<EmbeddedImage> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let color = img.color();
    let (width, height) = (img.width(), img.height());
    let gray = !color.has_color();
    let has_alpha = color.has_alpha();
    let (raw, color_space): (Vec<u8>, &'static str) = if gray {
        (img.to_luma8().into_raw(), "DeviceGray")
    } else {
        (img.to_rgb8().into_raw(), "DeviceRGB")
    };
    let alpha = if has_alpha {
        let a: Vec<u8> = img.to_rgba8().pixels().map(|p| p.0[3]).collect();
        Some(deflate(&a)?)
    } else {
        None
    };
    Some(EmbeddedImage {
        width,
        height,
        data: ImageData::Flate {
            color_space,
            data: deflate(&raw)?,
            alpha,
        },
    })
}

/// Dokumen PDF A4 ber-buffer.
pub struct Doc {
    pages: Vec<Page>,
    current: usize,
    /// `doc.options.margins` (dipakai halaman baru).
    margin_top: f64,
    margin_bottom: f64,
    images: Vec<EmbeddedImage>,
    info: Vec<(&'static str, String)>,
    /// Urutan pemakaian pertama font (pdfkit memberi id `F1`, `F2`, … berdasarkan itu).
    font_order: Vec<Font>,
}

impl Doc {
    pub fn new(margin_top: f64, margin_bottom: f64) -> Self {
        Self {
            pages: vec![Page::new(margin_top, margin_bottom)],
            current: 0,
            margin_top,
            margin_bottom,
            images: Vec::new(),
            info: Vec::new(),
            // pdfkit memasang Helvetica saat dokumen dibuat (`F1`).
            font_order: vec![Font::Regular],
        }
    }

    /// `info: { Title, Author, Creator }` (+ Producer).
    pub fn set_info(&mut self, key: &'static str, value: String) {
        self.info.push((key, value));
    }

    /// Mutasi `doc.options.margins` + `doc.page.margins` (halaman aktif).
    pub fn set_margins(&mut self, top: f64, bottom: f64) {
        self.margin_top = top;
        self.margin_bottom = bottom;
        self.set_page_margins(top, bottom);
    }

    /// `doc.page.margins.top/bottom = ...` (halaman aktif saja).
    pub fn set_page_margins(&mut self, top: f64, bottom: f64) {
        if let Some(p) = self.pages.get_mut(self.current) {
            p.margin_top = top;
            p.margin_bottom = bottom;
        }
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// `doc.addPage()`.
    pub fn add_page(&mut self) {
        self.pages
            .push(Page::new(self.margin_top, self.margin_bottom));
        self.current = self.pages.len() - 1;
    }

    /// `doc.switchToPage(i)`.
    pub fn switch_to_page(&mut self, i: usize) {
        if i < self.pages.len() {
            self.current = i;
        }
    }

    fn page(&self) -> Option<&Page> {
        self.pages.get(self.current)
    }

    fn page_max_y(&self) -> f64 {
        PAGE_H - self.page().map_or(0.0, |p| p.margin_bottom)
    }

    fn page_top(&self) -> f64 {
        self.page().map_or(0.0, |p| p.margin_top)
    }

    fn put(&mut self, s: &str) {
        if let Some(p) = self.pages.get_mut(self.current) {
            p.content.push_str(s);
        }
    }

    pub fn fill_color(&mut self, c: Rgb) {
        let s = format!("/DeviceRGB cs\n{} scn\n", c.ops());
        self.put(&s);
    }

    pub fn stroke_color(&mut self, c: Rgb) {
        let s = format!("/DeviceRGB CS\n{} SCN\n", c.ops());
        self.put(&s);
    }

    pub fn line_width(&mut self, w: f64) {
        let s = format!("{} w\n", num(w));
        self.put(&s);
    }

    pub fn save(&mut self) {
        self.put("q\n");
    }

    pub fn restore(&mut self) {
        self.put("Q\n");
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        let s = format!("{} {} m\n", num(x), num(y));
        self.put(&s);
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        let s = format!("{} {} l\n", num(x), num(y));
        self.put(&s);
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let s = format!("{} {} {} {} re\n", num(x), num(y), num(w), num(h));
        self.put(&s);
    }

    /// `stroke(color?)`.
    pub fn stroke(&mut self) {
        self.put("S\n");
    }

    /// `fill(color)`.
    pub fn fill(&mut self, c: Rgb) {
        self.fill_color(c);
        self.put("f\n");
    }

    /// `fillAndStroke(fill, stroke)`.
    pub fn fill_and_stroke(&mut self, fill: Rgb, stroke: Rgb) {
        self.fill_color(fill);
        self.stroke_color(stroke);
        self.put("B\n");
    }

    /// `dash(length, { space })`.
    pub fn dash(&mut self, length: f64, space: f64) {
        let s = format!("[{} {}] 0 d\n", num(length), num(space));
        self.put(&s);
    }

    /// `undash()`.
    pub fn undash(&mut self) {
        self.put("[] 0 d\n");
    }

    /// Jalur `roundedRect(x, y, w, h, r)` (kappa pdfkit).
    pub fn rounded_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64) {
        let kappa = 4.0 * ((2.0_f64.sqrt() - 1.0) / 3.0);
        let r = r.min(0.5 * w).min(0.5 * h);
        let c = r * (1.0 - kappa);
        let mut s = String::new();
        let _ = writeln!(s, "{} {} m", num(x + r), num(y));
        let _ = writeln!(s, "{} {} l", num(x + w - r), num(y));
        let _ = writeln!(
            s,
            "{} {} {} {} {} {} c",
            num(x + w - c),
            num(y),
            num(x + w),
            num(y + c),
            num(x + w),
            num(y + r)
        );
        let _ = writeln!(s, "{} {} l", num(x + w), num(y + h - r));
        let _ = writeln!(
            s,
            "{} {} {} {} {} {} c",
            num(x + w),
            num(y + h - c),
            num(x + w - c),
            num(y + h),
            num(x + w - r),
            num(y + h)
        );
        let _ = writeln!(s, "{} {} l", num(x + r), num(y + h));
        let _ = writeln!(
            s,
            "{} {} {} {} {} {} c",
            num(x + c),
            num(y + h),
            num(x),
            num(y + h - c),
            num(x),
            num(y + h - r)
        );
        let _ = writeln!(s, "{} {} l", num(x), num(y + r));
        let _ = writeln!(
            s,
            "{} {} {} {} {} {} c",
            num(x),
            num(y + c),
            num(x + c),
            num(y),
            num(x + r),
            num(y)
        );
        s.push_str("h\n");
        self.put(&s);
    }

    /// `doc.link(x, y, w, h, url)`.
    pub fn link(&mut self, x: f64, y: f64, w: f64, h: f64, url: &str) {
        if let Some(p) = self.pages.get_mut(self.current) {
            p.annots.push(Annot {
                x,
                y,
                w,
                h,
                url: url.to_owned(),
            });
        }
    }

    /// `doc.openImage(buffer)` — PNG/JPEG dikenali dari byte awal (bukan content type).
    pub fn open_image(&mut self, bytes: &[u8]) -> Option<OpenedImage> {
        let img = if bytes.starts_with(&[0xff, 0xd8]) {
            parse_jpeg(bytes)?
        } else if bytes.first() == Some(&0x89) && bytes.get(1..4) == Some(b"PNG".as_slice()) {
            parse_png(bytes)?
        } else {
            return None;
        };
        if img.width == 0 || img.height == 0 {
            return None;
        }
        let opened = OpenedImage {
            id: self.images.len(),
            width: img.width,
            height: img.height,
        };
        self.images.push(img);
        Some(opened)
    }

    /// `doc.image(opened, x, y, { width, height })` (orientasi 1).
    pub fn image(&mut self, img: OpenedImage, x: f64, y: f64, w: f64, h: f64) {
        let s = format!(
            "q\n{} 0 0 {} {} {} cm\n/I{} Do\nQ\n",
            num(w),
            num(-h),
            num(x),
            num(y + h),
            img.id + 1
        );
        self.put(&s);
        if let Some(p) = self.pages.get_mut(self.current)
            && !p.images.contains(&img.id)
        {
            p.images.push(img.id);
        }
    }

    /// `heightOfString(text, { width, lineGap })`.
    pub fn height_of_string(
        &mut self,
        text: &str,
        font: Font,
        size: f64,
        width: f64,
        line_gap: f64,
    ) -> f64 {
        // `doc.font(name)` mendaftarkan id font walau hanya untuk mengukur.
        self.font_id(font);
        let opts = TextOpts {
            font,
            size,
            color: Rgb(0.0, 0.0, 0.0),
            width,
            align: Align::Left,
            line_gap,
            height: Some(f64::INFINITY),
            ellipsis: false,
        };
        let mut e = Engine::new(self, &opts, 0.0, 0.0, false);
        e.wrap(self, text, &opts);
        e.y
    }

    /// `fillColor(color).text(text, x, y, opts)`. Mengembalikan `doc.y` sesudahnya.
    pub fn text(&mut self, text: &str, x: f64, y: f64, opts: &TextOpts) -> f64 {
        self.fill_color(opts.color);
        let mut e = Engine::new(self, opts, x, y, true);
        e.wrap(self, text, opts);
        e.y
    }

    /// Id font (`F<n>`, urutan pemakaian pertama).
    fn font_id(&mut self, font: Font) -> usize {
        if let Some(i) = self.font_order.iter().position(|f| *f == font) {
            return i + 1;
        }
        self.font_order.push(font);
        self.font_order.len()
    }

    fn mark_font(&mut self, font: Font) {
        if let Some(p) = self.pages.get_mut(self.current)
            && !p.fonts.contains(&font)
        {
            p.fonts.push(font);
        }
    }

    /// `_fragment`: satu baris terbungkus.
    #[allow(clippy::too_many_arguments)]
    fn fragment(
        &mut self,
        text: &str,
        mut x: f64,
        y: f64,
        o: &TextOpts,
        line_width: f64,
        text_width_opt: f64,
    ) {
        let text = text.replace('\n', "");
        if text.is_empty() {
            return;
        }
        match o.align {
            Align::Right => {
                x += line_width - text_width(text.trim_end(), o.font, o.size);
            }
            Align::Center => {
                x += line_width / 2.0 - text_width_opt / 2.0;
            }
            Align::Left => {}
        }
        let dy = 718.0 / 1000.0 * o.size;
        let cs = codes(&text);
        let mut tj = String::from("[<");
        for (i, c) in cs.iter().enumerate() {
            let _ = write!(tj, "{c:02x}");
            if let Some(n) = cs.get(i + 1) {
                let k = o.font.kern(*c, *n);
                if k != 0 {
                    let _ = write!(tj, "> {} <", -k);
                }
            }
        }
        tj.push_str("> 0] TJ");
        self.mark_font(o.font);
        let font_id = self.font_id(o.font);
        let s = format!(
            "q\n1 0 0 -1 0 {} cm\nBT\n1 0 0 1 {} {} Tm\n/F{font_id} {} Tf\n{}\nET\nQ\n",
            num(PAGE_H),
            num(x),
            num(PAGE_H - y - dy),
            num(o.size),
            tj
        );
        self.put(&s);
    }

    /// Rakit berkas PDF. `compress` = content stream Flate (bawaan pdfkit).
    pub fn finish(self, compress: bool) -> Vec<u8> {
        let Self {
            pages,
            images,
            info,
            font_order,
            ..
        } = self;
        let count = pages.len();
        // 1 katalog, 2 pages, 3..6 font, 7 info, lalu gambar (+smask), lalu halaman.
        let mut next_id = 8;
        let mut image_ids: Vec<(usize, Option<usize>)> = Vec::new();
        for img in &images {
            let id = next_id;
            next_id += 1;
            let smask = match &img.data {
                ImageData::Flate { alpha: Some(_), .. } => {
                    let m = next_id;
                    next_id += 1;
                    Some(m)
                }
                _ => None,
            };
            image_ids.push((id, smask));
        }
        struct PageIds {
            page: usize,
            content: usize,
            annots: Vec<usize>,
        }
        let mut ids: Vec<PageIds> = Vec::new();
        for p in &pages {
            let page = next_id;
            let content = next_id + 1;
            next_id += 2;
            let annots = p
                .annots
                .iter()
                .map(|_| {
                    let id = next_id;
                    next_id += 1;
                    id
                })
                .collect();
            ids.push(PageIds {
                page,
                content,
                annots,
            });
        }
        let total = next_id - 1;
        let mut objs: Vec<Vec<u8>> = vec![Vec::new(); total];
        let mut set = |id: usize, body: Vec<u8>| {
            if let Some(slot) = objs.get_mut(id - 1) {
                *slot = body;
            }
        };
        set(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        let kids: Vec<String> = ids.iter().map(|i| format!("{} 0 R", i.page)).collect();
        set(
            2,
            format!(
                "<< /Type /Pages /Kids [{}] /Count {count} >>",
                kids.join(" ")
            )
            .into_bytes(),
        );
        for (i, f) in Font::ALL.iter().enumerate() {
            set(
                3 + i,
                format!(
                    "<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>",
                    f.base_name()
                )
                .into_bytes(),
            );
        }
        let mut info_body = String::from("<< /Producer (PDFKit)");
        for (k, v) in &info {
            let _ = write!(info_body, " /{k} {}", pdf_string(v));
        }
        info_body.push_str(" >>");
        set(7, info_body.into_bytes());
        for (img, (iid, smask)) in images.iter().zip(&image_ids) {
            match &img.data {
                ImageData::Flate {
                    color_space,
                    data,
                    alpha,
                } => {
                    let smask_ref = smask
                        .map(|m| format!(" /SMask {m} 0 R"))
                        .unwrap_or_default();
                    set(
                        *iid,
                        stream(
                            &format!(
                                "/Type /XObject /Subtype /Image /BitsPerComponent 8 /Width {} /Height {} /Filter /FlateDecode /ColorSpace /{color_space}{smask_ref}",
                                img.width, img.height
                            ),
                            data,
                        ),
                    );
                    if let (Some(m), Some(a)) = (smask, alpha) {
                        set(
                            *m,
                            stream(
                                &format!(
                                    "/Type /XObject /Subtype /Image /BitsPerComponent 8 /Width {} /Height {} /Filter /FlateDecode /ColorSpace /DeviceGray /Decode [0 1]",
                                    img.width, img.height
                                ),
                                a,
                            ),
                        );
                    }
                }
                ImageData::Dct {
                    color_space,
                    bits,
                    data,
                } => {
                    let decode = if *color_space == "DeviceCMYK" {
                        " /Decode [1 0 1 0 1 0 1 0]"
                    } else {
                        ""
                    };
                    set(
                        *iid,
                        stream(
                            &format!(
                                "/Type /XObject /Subtype /Image /BitsPerComponent {bits} /Width {} /Height {} /ColorSpace /{color_space} /Filter /DCTDecode{decode}",
                                img.width, img.height
                            ),
                            data,
                        ),
                    );
                }
            }
        }
        for (p, pid) in pages.iter().zip(&ids) {
            let mut res = String::from("/ProcSet [/PDF /Text /ImageB /ImageC /ImageI]");
            if !p.fonts.is_empty() {
                res.push_str(" /Font <<");
                for f in &p.fonts {
                    let idx = Font::ALL.iter().position(|x| x == f).unwrap_or(0);
                    let fid = font_order.iter().position(|x| x == f).map_or(0, |i| i + 1);
                    let _ = write!(res, " /F{fid} {} 0 R", 3 + idx);
                }
                res.push_str(" >>");
            }
            if !p.images.is_empty() {
                res.push_str(" /XObject <<");
                for i in &p.images {
                    if let Some((iid, _)) = image_ids.get(*i) {
                        let _ = write!(res, " /I{} {iid} 0 R", i + 1);
                    }
                }
                res.push_str(" >>");
            }
            let annots = if pid.annots.is_empty() {
                String::new()
            } else {
                format!(
                    " /Annots [{}]",
                    pid.annots
                        .iter()
                        .map(|a| format!("{a} 0 R"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            set(
                pid.page,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << {res} >> /Contents {} 0 R{annots} >>",
                    num(PAGE_W),
                    num(PAGE_H),
                    pid.content
                )
                .into_bytes(),
            );
            let body = if compress {
                deflate(p.content.as_bytes()).map_or_else(
                    || stream("", p.content.as_bytes()),
                    |z| stream("/Filter /FlateDecode", &z),
                )
            } else {
                stream("", p.content.as_bytes())
            };
            set(pid.content, body);
            for (a, aid) in p.annots.iter().zip(&pid.annots) {
                let (x1, x2) = (a.x, a.x + a.w);
                let (y1, y2) = (PAGE_H - a.y - a.h, PAGE_H - a.y);
                set(
                    *aid,
                    format!(
                        "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] /A << /S /URI /URI {} >> >>",
                        num(x1),
                        num(y1),
                        num(x2),
                        num(y2),
                        pdf_string(&a.url)
                    )
                    .into_bytes(),
                );
            }
        }
        let mut out: Vec<u8> = b"%PDF-1.3\n%\xff\xff\xff\xff\n".to_vec();
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info 7 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// Isi content stream tiap halaman (tanpa kompresi) — untuk uji.
    pub fn page_contents(&self) -> Vec<&str> {
        self.pages.iter().map(|p| p.content.as_str()).collect()
    }

    pub fn image_count(&self) -> usize {
        self.images.len()
    }
}

fn stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let sep = if dict_extra.is_empty() { "" } else { " " };
    let mut body = format!("<< {dict_extra}{sep}/Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// String PDF seperti pdfkit: ASCII → literal `(...)`, selain itu UTF-16BE ber-BOM `<FEFF...>`.
fn pdf_string(s: &str) -> String {
    if s.chars().all(|c| (c as u32) < 0x80) {
        let mut out = String::from("(");
        for c in s.chars() {
            match c {
                '(' | ')' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                _ => out.push(c),
            }
        }
        out.push(')');
        out
    } else {
        let mut out = String::from("<FEFF");
        for u in s.encode_utf16() {
            let _ = write!(out, "{u:04X}");
        }
        out.push('>');
        out
    }
}

/// Teks per baris (`TJ`) dari satu content stream tak terkompresi, beserta posisi `Tm`
/// (ruang PDF). Untuk uji & perbandingan dengan pdfkit.
pub fn extract_fragments(content: &str) -> Vec<(f64, f64, String)> {
    let mut out = Vec::new();
    let mut pos = (0.0, 0.0);
    for line in content.lines() {
        if let Some(rest) = line.strip_suffix(" Tm") {
            let parts: Vec<f64> = rest.split(' ').filter_map(|p| p.parse().ok()).collect();
            if let (Some(x), Some(y)) = (parts.get(4), parts.get(5)) {
                pos = (*x, *y);
            }
        } else if line.ends_with(" TJ") {
            let mut text = String::new();
            let mut in_hex = false;
            let mut hex = String::new();
            for c in line.chars() {
                match c {
                    '<' => in_hex = true,
                    '>' => {
                        in_hex = false;
                        for i in (0..hex.len()).step_by(2) {
                            if let Some(b) = hex
                                .get(i..i + 2)
                                .and_then(|h| u8::from_str_radix(h, 16).ok())
                            {
                                text.push(from_win_ansi(b));
                            }
                        }
                        hex.clear();
                    }
                    c if in_hex => hex.push(c),
                    _ => {}
                }
            }
            out.push((pos.0, pos.1, text));
        }
    }
    out
}

// ───────────────────────── LineWrapper pdfkit ─────────────────────────

struct Engine {
    draw: bool,
    x: f64,
    /// `doc.y`.
    y: f64,
    line_width: f64,
    space_left: f64,
    max_y: f64,
    has_height: bool,
    start_x: f64,
    buffer: String,
    text_width: f64,
    wc: u32,
}

impl Engine {
    fn new(doc: &Doc, o: &TextOpts, x: f64, y: f64, draw: bool) -> Self {
        // pdfkit: `(width * horizontalScaling / 100 - columnGap * 0) / 1` — perkalian/pembagian
        // 100 bisa menggeser bit terakhir, jadi ditiru apa adanya.
        #[allow(clippy::identity_op)]
        let width = (o.width.max(0.0) * 100.0 / 100.0 - 18.0 * 0.0) / 1.0;
        let max_y = match o.height {
            Some(h) => pdf_number(y + h),
            None => pdf_number(doc.page_max_y()),
        };
        Self {
            draw,
            x,
            y,
            line_width: width,
            space_left: width,
            max_y,
            has_height: o.height.is_some(),
            start_x: x,
            buffer: String::new(),
            text_width: 0.0,
            wc: 0,
        }
    }

    fn word_width(o: &TextOpts, w: &str) -> f64 {
        pdf_number(text_width(w, o.font, o.size))
    }

    /// `nextSection` — dengan `height` berhenti; tanpa itu `continueOnNewPage`.
    fn next_section(&mut self, doc: &mut Doc, o: &TextOpts) -> bool {
        if self.has_height {
            return false;
        }
        doc.add_page();
        self.y = doc.page_top();
        self.max_y = pdf_number(doc.page_max_y());
        self.x = self.start_x;
        if self.draw {
            doc.fill_color(o.color);
        }
        true
    }

    /// `_line`: gambar baris lalu `y += currentLineHeight(true) + lineGap`.
    fn emit_line(&mut self, doc: &mut Doc, o: &TextOpts) {
        if self.draw {
            let buffer = self.buffer.clone();
            doc.fragment(&buffer, self.x, self.y, o, self.line_width, self.text_width);
        }
        self.y += line_height(o.font, o.size) + o.line_gap;
    }

    fn process(
        &mut self,
        doc: &mut Doc,
        o: &TextOpts,
        word: &str,
        w: f64,
        required: bool,
        last: Option<bool>,
    ) -> bool {
        if last.is_none_or(|r| r) {
            self.space_left = self.line_width;
        }
        let can_fit = w <= self.space_left;
        if can_fit {
            self.buffer.push_str(word);
            self.text_width += w;
            self.wc += 1;
        }
        if !(required || !can_fit) {
            self.space_left -= w;
            return true;
        }
        let lh = line_height(o.font, o.size);
        if self.has_height && o.ellipsis && pdf_number(self.y + lh * 2.0) > self.max_y {
            const ELLIPSIS: &str = "\u{2026}";
            let mut buffer = self.buffer.trim_end_matches(char::is_whitespace).to_owned();
            let mut tw = Self::word_width(o, &format!("{buffer}{ELLIPSIS}"));
            while !buffer.is_empty() && tw > self.line_width {
                buffer.pop();
                buffer = buffer.trim_end_matches(char::is_whitespace).to_owned();
                tw = Self::word_width(o, &format!("{buffer}{ELLIPSIS}"));
            }
            if tw <= self.line_width {
                buffer.push_str(ELLIPSIS);
            }
            self.text_width = Self::word_width(o, &buffer);
            self.buffer = buffer;
        }
        if required && w > self.space_left {
            self.emit_line(doc, o);
            self.buffer = word.to_owned();
            self.text_width = w;
            self.wc = 1;
        }
        self.emit_line(doc, o);
        if pdf_number(self.y + lh) > self.max_y && !self.next_section(doc, o) {
            self.wc = 0;
            self.buffer.clear();
            return false;
        }
        if required {
            self.space_left = self.line_width;
            self.buffer.clear();
            self.text_width = 0.0;
            self.wc = 0;
        } else {
            self.space_left = self.line_width - w;
            self.buffer = word.to_owned();
            self.text_width = w;
            self.wc = 1;
        }
        true
    }

    fn wrap(&mut self, doc: &mut Doc, text: &str, o: &TextOpts) {
        let lh = line_height(o.font, o.size);
        if self.y > self.max_y || self.y + lh > self.max_y {
            let _ = self.next_section(doc, o);
        }
        let chars: Vec<char> = text.chars().collect();
        let bks = breaks::breaks(&chars);
        let mut prev_pos = 0usize;
        let mut last: Option<bool> = None;
        let mut widths: HashMap<String, f64> = HashMap::new();
        for (pos, req) in bks {
            let word: String = chars
                .get(prev_pos..pos)
                .unwrap_or_default()
                .iter()
                .collect();
            prev_pos = pos;
            let mut w = *widths
                .entry(word.clone())
                .or_insert_with(|| Self::word_width(o, &word));
            let mut cont;
            if w > self.line_width {
                let mut lbk = last;
                let mut rest: Vec<char> = word.chars().collect();
                cont = true;
                while !rest.is_empty() {
                    let len = rest.len();
                    let mut l;
                    let mut might_grow = false;
                    if w > self.space_left {
                        l = (self.space_left / (w / len as f64)).ceil().max(0.0) as usize;
                        l = l.min(len);
                        w = Self::word_width(o, &slice(&rest, l));
                        might_grow = w <= self.space_left && l < len;
                    } else {
                        l = len;
                    }
                    let mut must_shrink = w > self.space_left && l > 0;
                    while must_shrink || might_grow {
                        if must_shrink {
                            l -= 1;
                            w = Self::word_width(o, &slice(&rest, l));
                            must_shrink = w > self.space_left && l > 0;
                        } else {
                            l += 1;
                            w = Self::word_width(o, &slice(&rest, l));
                            must_shrink = w > self.space_left && l > 0;
                            might_grow = w <= self.space_left && l < len;
                        }
                    }
                    // pdfkit memaksa satu karakter tanpa menghitung ulang `w`.
                    if l == 0 && self.space_left == self.line_width {
                        l = 1;
                    }
                    let fbk_required = req || l < len;
                    let piece = slice(&rest, l);
                    cont = self.process(doc, o, &piece, w, fbk_required, lbk);
                    lbk = Some(false);
                    rest = rest.get(l..).unwrap_or_default().to_vec();
                    w = Self::word_width(o, &rest.iter().collect::<String>());
                    if !cont {
                        break;
                    }
                }
            } else {
                cont = self.process(doc, o, &word, w, req, last);
            }
            if !cont {
                break;
            }
            last = Some(req);
        }
        if self.wc > 0 {
            self.emit_line(doc, o);
        }
    }
}

fn slice(chars: &[char], l: usize) -> String {
    chars.get(..l).unwrap_or_default().iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(width: f64) -> TextOpts {
        TextOpts {
            font: Font::Regular,
            size: 10.0,
            color: Rgb(0.0, 0.0, 0.0),
            width,
            align: Align::Left,
            line_gap: 0.0,
            height: None,
            ellipsis: false,
        }
    }

    #[test]
    fn widths_and_heights() {
        assert!((text_width("Hello", Font::Regular, 10.0) - 22.78).abs() < 1e-9);
        assert!((text_width("Hello", Font::Italic, 10.0) - 22.78).abs() < 1e-9);
        assert!((text_width("Hello", Font::BoldItalic, 10.0) - 24.45).abs() < 1e-9);
        let mut d = Doc::new(50.0, 50.0);
        let h1 = d.height_of_string("aaaa bbbb cccc", Font::Regular, 10.0, 1000.0, 1.5);
        assert!((h1 - 13.06).abs() < 1e-9, "{h1}");
        let h3 = d.height_of_string("aaaa bbbb cccc", Font::Regular, 10.0, 40.0, 0.0);
        assert!((h3 - 3.0 * 11.56).abs() < 1e-9, "{h3}");
        assert_eq!(d.page_contents()[0].matches("TJ").count(), 0);
    }

    #[test]
    fn ellipsis_caps_lines() {
        let mut d = Doc::new(50.0, 50.0);
        let mut o = opts(100.0);
        o.height = Some(25.0);
        o.ellipsis = true;
        d.text(&"word ".repeat(60), 60.0, 100.0, &o);
        let frags = extract_fragments(d.page_contents()[0]);
        assert_eq!(frags.len(), 2, "{frags:?}");
        assert!(frags[1].2.ends_with('\u{2026}'), "{frags:?}");
    }

    #[test]
    fn auto_page_break_past_bottom() {
        let mut d = Doc::new(50.0, 50.0);
        d.text("late", 50.0, 790.0, &opts(100.0));
        assert_eq!(d.page_count(), 2);
    }

    #[test]
    fn images_and_strings() {
        let png = {
            let mut buf = Vec::new();
            let img = image::RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 128]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        let mut d = Doc::new(50.0, 50.0);
        let o = d.open_image(&png).unwrap();
        assert_eq!((o.width, o.height), (2, 3));
        assert!(d.open_image(b"bukan gambar").is_none());
        // JPEG minimal: SOI + SOF0 (8 bit, 4x5, 3 kanal).
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x05, 0x00, 0x04, 0x03,
        ];
        let j = d.open_image(&jpeg).unwrap();
        assert_eq!((j.width, j.height), (4, 5));
        d.image(o, 10.0, 10.0, 20.0, 30.0);
        d.image(o, 10.0, 10.0, 20.0, 30.0);
        let pdf = String::from_utf8_lossy(&d.finish(true)).into_owned();
        // Gambar RGBA = XObject + SMask; JPEG = 1 XObject DCT.
        assert_eq!(pdf.matches("/Subtype /Image").count(), 3);
        assert!(pdf.contains("/DCTDecode"));
        assert_eq!(pdf_string("a(b)"), "(a\\(b\\))");
        assert_eq!(pdf_string("a—b"), "<FEFF006120140062>");
    }
}
