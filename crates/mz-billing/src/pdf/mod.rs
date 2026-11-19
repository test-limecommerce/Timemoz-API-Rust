//! Penulis PDF Rust murni yang meniru perilaku `pdfkit` 0.17 untuk satu pemakaian: invoice.
//!
//! Koordinat, urutan operator, dan pembungkusan teks mengikuti pdfkit supaya tata letak
//! sama dengan `invoices/invoice-pdf.generator.ts`:
//! - ruang kerja dibalik (`1 0 0 -1 0 841.89 cm`), `y` = ATAS baris teks, satuan point;
//! - font standar Helvetica / Helvetica-Bold, WinAnsi, lebar glyph + KERNING dari AFM
//!   (`mz_pdfkit::afm`, dihasilkan dari data pdfkit), tinggi baris 1,156 em, garis dasar = atas + 0,718 em;
//! - `LineWrapper` pdfkit (kata bersama spasi buntutnya, pecah kata per huruf bila lebih lebar
//!   dari baris, opsi `height` memotong baris berlebih, halaman baru otomatis bila baris
//!   melewati margin bawah);
//! - rata kanan memakai lebar baris tanpa spasi buntut, rata tengah memakai jumlah lebar kata;
//! - rounded rect (kappa pdfkit), rotasi `rotate(angle, {origin})`, anotasi tautan + garis bawah,
//!   gambar PNG (RGB + SMask alfa, Flate).
//!
//! DUPLIKAT (diperluas) dari `mz-timesheets::pdf` — sengaja dibiarkan (dicek 2026-11-18): versi
//! timesheets tanpa kerning/AFM penuh dan pembungkusan berbeda; menyatukan mengubah byte PDF. AFM +
//! putus baris kini bersama dengan `mz-visit-forms` lewat `mz-pdfkit`. Beda dari pdfkit:
//! aliran konten tidak dikompres (mudah diperiksa di uji), tanpa `/Info` tanggal, dan karakter
//! di luar WinAnsi ditulis `?` (pdfkit menulis kode mentah dengan lebar nol).

use mz_pdfkit::{afm, breaks};

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;

pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;
/// `margin: 50` (kiri/kanan/atas; bawah bisa diubah sementara, lihat [`Doc::set_margin_bottom`]).
pub const MARGIN: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    /// `#rrggbb` (pdfkit: `parseInt(hex)/255`).
    pub fn hex(s: &str) -> Self {
        let h = s.trim_start_matches('#');
        let c = |i: usize| {
            h.get(i..i + 2)
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .map_or(0.0, |v| f64::from(v) / 255.0)
        };
        Self(c(0), c(2), c(4))
    }

    fn ops(self) -> String {
        format!("{} {} {}", num(self.0), num(self.1), num(self.2))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    fn id(self) -> &'static str {
        match self {
            Self::Regular => "F1",
            Self::Bold => "F2",
        }
    }

    fn widths(self) -> &'static [u16; 256] {
        match self {
            Self::Regular => &afm::W_REGULAR,
            Self::Bold => &afm::W_BOLD,
        }
    }

    fn kerns(self) -> &'static [(u16, i16)] {
        match self {
            Self::Regular => afm::KERN_REGULAR,
            Self::Bold => afm::KERN_BOLD,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Right,
    Center,
}

/// Opsi `doc.text(...)` yang dipakai invoice.
#[derive(Debug, Clone, Default)]
pub struct TextOpts {
    /// `width` (bawaan: `page.width - x - margin kanan`).
    pub width: Option<f64>,
    /// `height`: baris yang tak muat dibuang (tanpa ellipsis).
    pub height: Option<f64>,
    pub align: Align,
    /// `link` + `underline`.
    pub link: Option<String>,
    pub underline: bool,
    /// Teks lanjutan (`continued: true` pada panggilan sebelumnya): geser baris pertama.
    pub indent_first: f64,
}

impl TextOpts {
    pub fn width(w: f64) -> Self {
        Self {
            width: Some(w),
            ..Self::default()
        }
    }

    pub fn aligned(w: f64, align: Align) -> Self {
        Self {
            width: Some(w),
            align,
            ..Self::default()
        }
    }
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
fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
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
        // `\n` tetap ada di string untuk pembungkusan; tidak ada glyph (lebar 0).
        0x0A => 0x0A,
        _ => return None,
    })
}

fn codes(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| win_ansi(c).unwrap_or(b'?'))
        .collect::<Vec<_>>()
}

/// `font.widthOfString(s, size)` (lebar glyph + kerning antar-glyph dalam string itu).
pub fn text_width(s: &str, font: Font, size: f64) -> f64 {
    width_of_codes(&codes(s), font, size)
}

fn width_of_codes(cs: &[u8], font: Font, size: f64) -> f64 {
    let w = font.widths();
    let mut total = 0.0;
    for (i, c) in cs.iter().enumerate() {
        total += f64::from(w.get(usize::from(*c)).copied().unwrap_or(0));
        if let Some(n) = cs.get(i + 1) {
            total += f64::from(font.kern(*c, *n));
        }
    }
    total * size / 1000.0
}

/// Tinggi baris dengan gap (`currentLineHeight(true)`): (ascender 718 + descender 207 + lineGap) / 1000 em,
/// lineGap = tinggi FontBBox - (718 + 207): Helvetica 231 (1,156 em), Helvetica-Bold 265 (1,19 em).
pub fn line_height(font: Font, size: f64) -> f64 {
    match font {
        Font::Regular => size * 1.156,
        Font::Bold => size * 1.19,
    }
}

/// Tinggi baris tanpa gap (`currentLineHeight()`): (718 + 207) / 1000 em.
fn line_height_nogap(size: f64) -> f64 {
    size * 0.925
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
    image: bool,
}

impl Page {
    fn new() -> Self {
        Self {
            content: format!("1 0 0 -1 0 {} cm\n", num(PAGE_H)),
            annots: Vec::new(),
            image: false,
        }
    }
}

/// Gambar PNG yang sudah disiapkan: RGB + alfa, masing-masing Flate.
#[derive(Clone)]
pub struct Image {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    alpha: Vec<u8>,
}

impl Image {
    /// Dekode PNG (RGBA8 → RGB + SMask). `None` bila gagal.
    pub fn from_png(bytes: &[u8]) -> Option<Self> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        let mut alpha = Vec::with_capacity((width * height) as usize);
        for px in rgba.pixels() {
            let [r, g, b, a] = px.0;
            rgb.extend_from_slice(&[r, g, b]);
            alpha.push(a);
        }
        Some(Self {
            width,
            height,
            rgb: deflate(&rgb)?,
            alpha: deflate(&alpha)?,
        })
    }
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Dokumen PDF (A4, margin 50).
pub struct Doc {
    pages: Vec<Page>,
    margin_bottom: f64,
    image: Option<Image>,
    /// `options.textWidth` baris terakhir yang digambar (untuk teks `continued`).
    last_text_width: f64,
}

impl Default for Doc {
    fn default() -> Self {
        Self::new()
    }
}

impl Doc {
    pub fn new() -> Self {
        Self {
            pages: vec![Page::new()],
            margin_bottom: MARGIN,
            image: None,
            last_text_width: 0.0,
        }
    }

    /// `doc.page.margins.bottom = v` (footer invoice menolkan margin bawah sementara).
    pub fn set_margin_bottom(&mut self, v: f64) -> f64 {
        std::mem::replace(&mut self.margin_bottom, v)
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// `doc.addPage()`.
    pub fn add_page(&mut self) {
        self.pages.push(Page::new());
    }

    fn put(&mut self, s: &str) {
        if let Some(p) = self.pages.last_mut() {
            p.content.push_str(s);
        }
    }

    fn fill_color(&mut self, c: Rgb) {
        let s = format!("/DeviceRGB cs\n{} scn\n", c.ops());
        self.put(&s);
    }

    fn stroke_color(&mut self, c: Rgb) {
        let s = format!("/DeviceRGB CS\n{} SCN\n", c.ops());
        self.put(&s);
    }

    pub fn save(&mut self) {
        self.put("q\n");
    }

    pub fn restore(&mut self) {
        self.put("Q\n");
    }

    /// `doc.rotate(angle, { origin: [ox, oy] })` (ruang pdfkit, y ke bawah).
    pub fn rotate(&mut self, angle_deg: f64, ox: f64, oy: f64) {
        let rad = angle_deg * std::f64::consts::PI / 180.0;
        let (cos, sin) = (rad.cos(), rad.sin());
        let x1 = ox * cos - oy * sin;
        let y1 = ox * sin + oy * cos;
        let s = format!(
            "{} {} {} {} {} {} cm\n",
            num(cos),
            num(sin),
            num(-sin),
            num(cos),
            num(ox - x1),
            num(oy - y1)
        );
        self.put(&s);
    }

    /// `rect(x, y, w, h).fill(color)`.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgb) {
        let s = format!("{} {} {} {} re\n", num(x), num(y), num(w), num(h));
        self.put(&s);
        self.fill_color(c);
        self.put("f\n");
    }

    fn round_rect_path(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64) {
        let kappa = 4.0 * ((2.0_f64.sqrt() - 1.0) / 3.0);
        let r = r.min(0.5 * w).min(0.5 * h);
        let c = r * (1.0 - kappa);
        let s = format!(
            "{} {} m\n{} {} l\n{} {} {} {} {} {} c\n{} {} l\n{} {} {} {} {} {} c\n{} {} l\n{} {} {} {} {} {} c\n{} {} l\n{} {} {} {} {} {} c\nh\n",
            num(x + r),
            num(y),
            num(x + w - r),
            num(y),
            num(x + w - c),
            num(y),
            num(x + w),
            num(y + c),
            num(x + w),
            num(y + r),
            num(x + w),
            num(y + h - r),
            num(x + w),
            num(y + h - c),
            num(x + w - c),
            num(y + h),
            num(x + w - r),
            num(y + h),
            num(x + r),
            num(y + h),
            num(x + c),
            num(y + h),
            num(x),
            num(y + h - c),
            num(x),
            num(y + h - r),
            num(x),
            num(y + r),
            num(x),
            num(y + c),
            num(x + c),
            num(y),
            num(x + r),
            num(y),
        );
        self.put(&s);
    }

    /// `roundedRect(x, y, w, h, r).fill(color)`.
    pub fn fill_round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, c: Rgb) {
        self.round_rect_path(x, y, w, h, r);
        self.fill_color(c);
        self.put("f\n");
    }

    /// `roundedRect(...).lineWidth(lw).strokeColor(color).stroke()`.
    #[allow(clippy::too_many_arguments)]
    pub fn stroke_round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, lw: f64, c: Rgb) {
        self.round_rect_path(x, y, w, h, r);
        let s = format!("{} w\n", num(lw));
        self.put(&s);
        self.stroke_color(c);
        self.put("S\n");
    }

    /// `strokeColor(c).lineWidth(lw).moveTo(x1, y1).lineTo(x2, y2).stroke()`.
    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c: Rgb, lw: f64) {
        self.stroke_color(c);
        let s = format!(
            "{} w\n{} {} m\n{} {} l\nS\n",
            num(lw),
            num(x1),
            num(y1),
            num(x2),
            num(y2)
        );
        self.put(&s);
    }

    /// `doc.image(buf, x, y, { width })`; mengembalikan tinggi gambar.
    pub fn image(&mut self, img: Image, x: f64, y: f64, width: f64) -> f64 {
        let h = f64::from(img.height) * width / f64::from(img.width);
        let s = format!(
            "q\n{} 0 0 {} {} {} cm\n/I1 Do\nQ\n",
            num(width),
            num(-h),
            num(x),
            num(y + h)
        );
        self.put(&s);
        if let Some(p) = self.pages.last_mut() {
            p.image = true;
        }
        self.image = Some(img);
        h
    }

    /// `doc.text(s, x, y, opts)` dengan font/ukuran/warna isi saat ini. Mengembalikan lebar
    /// baris terakhir (`options.textWidth`, jumlah lebar kata) — dipakai sebagai `indent_first`
    /// teks lanjutan (`continued: true`).
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        s: &str,
        x: f64,
        y: f64,
        font: Font,
        size: f64,
        color: Rgb,
        opts: &TextOpts,
    ) -> f64 {
        self.last_text_width = 0.0;
        Engine::run(self, s, x, y, font, size, color, opts);
        self.last_text_width
    }

    /// Satu baris teks tanpa opsi (`doc.text(s, x, y)`).
    pub fn text_at(&mut self, s: &str, x: f64, y: f64, font: Font, size: f64, color: Rgb) {
        self.text(s, x, y, font, size, color, &TextOpts::default());
    }

    fn next_page_for_text(&mut self) {
        self.add_page();
    }

    /// `_fragment`: satu baris terbungkus, sudah pada koordinat akhir.
    #[allow(clippy::too_many_arguments)]
    fn fragment(
        &mut self,
        text: &str,
        mut x: f64,
        y: f64,
        font: Font,
        size: f64,
        color: Rgb,
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
                x += line_width - text_width(text.trim_end(), font, size);
            }
            Align::Center => {
                x += line_width / 2.0 - text_width_opt / 2.0;
            }
            Align::Left => {}
        }
        let dy = 718.0 / 1000.0 * size;
        let rendered = text_width_opt;
        if let Some(url) = &o.link {
            let annot = Annot {
                x,
                y,
                w: rendered,
                h: line_height_nogap(size),
                url: url.clone(),
            };
            if let Some(p) = self.pages.last_mut() {
                p.annots.push(annot);
            }
        }
        if o.underline {
            self.save();
            self.stroke_color(color);
            let lw = if size < 10.0 {
                0.5
            } else {
                (size / 10.0).floor()
            };
            let s = format!("{} w\n", num(lw));
            self.put(&s);
            let line_y = y + line_height_nogap(size) - lw;
            let s = format!(
                "{} {} m\n{} {} l\nS\n",
                num(x),
                num(line_y),
                num(x + rendered),
                num(line_y)
            );
            self.put(&s);
            self.restore();
        }
        let cs = codes(&text);
        let mut tj = String::from("[<");
        for (i, c) in cs.iter().enumerate() {
            let _ = write!(tj, "{c:02x}");
            if let Some(n) = cs.get(i + 1) {
                let k = font.kern(*c, *n);
                if k != 0 {
                    let _ = write!(tj, "> {} <", -k);
                }
            }
        }
        tj.push_str("> 0] TJ");
        self.save();
        let s = format!(
            "1 0 0 -1 0 {} cm\nBT\n1 0 0 1 {} {} Tm\n/{} {} Tf\n{}\nET\n",
            num(PAGE_H),
            num(x),
            num(PAGE_H - y - dy),
            font.id(),
            num(size),
            tj
        );
        self.put(&s);
        self.restore();
    }

    /// Rakit berkas PDF.
    pub fn finish(self) -> Vec<u8> {
        let Self { pages, image, .. } = self;
        let count = pages.len();
        let has_image = image.is_some() && pages.iter().any(|p| p.image);
        // Nomor objek: 1 katalog, 2 pages, 3 Helvetica, 4 Helvetica-Bold, 5 info,
        // [6 gambar, 7 smask], lalu per halaman: page, content, annots...
        let mut next_id = 6;
        let image_ids = if has_image {
            let ids = (next_id, next_id + 1);
            next_id += 2;
            Some(ids)
        } else {
            None
        };
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
        set(
            3,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        );
        set(4, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec());
        set(
            5,
            b"<< /Producer (mudah-rs mz-billing) /Creator (mudah-rs mz-billing) >>".to_vec(),
        );
        if let (Some((iid, mid)), Some(img)) = (image_ids, image.as_ref()) {
            let mut body = format!(
                "<< /Type /XObject /Subtype /Image /BitsPerComponent 8 /Width {} /Height {} /Filter /FlateDecode /ColorSpace /DeviceRGB /SMask {mid} 0 R /Length {} >>\nstream\n",
                img.width,
                img.height,
                img.rgb.len()
            )
            .into_bytes();
            body.extend_from_slice(&img.rgb);
            body.extend_from_slice(b"\nendstream");
            set(iid, body);
            let mut body = format!(
                "<< /Type /XObject /Subtype /Image /BitsPerComponent 8 /Width {} /Height {} /Filter /FlateDecode /ColorSpace /DeviceGray /Length {} >>\nstream\n",
                img.width,
                img.height,
                img.alpha.len()
            )
            .into_bytes();
            body.extend_from_slice(&img.alpha);
            body.extend_from_slice(b"\nendstream");
            set(mid, body);
        }
        for (p, pid) in pages.iter().zip(&ids) {
            let mut res = String::from(
                "/ProcSet [/PDF /Text /ImageB /ImageC /ImageI] /Font << /F1 3 0 R /F2 4 0 R >>",
            );
            if let (true, Some((iid, _))) = (p.image, image_ids) {
                let _ = write!(res, " /XObject << /I1 {iid} 0 R >>");
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
            set(
                pid.content,
                format!(
                    "<< /Length {} >>\nstream\n{}endstream",
                    p.content.len(),
                    p.content
                )
                .into_bytes(),
            );
            for (a, aid) in p.annots.iter().zip(&pid.annots) {
                // Koordinat anotasi di ruang PDF (y ke atas).
                let (x1, x2) = (a.x, a.x + a.w);
                let (y1, y2) = (PAGE_H - a.y - a.h, PAGE_H - a.y);
                set(
                    *aid,
                    format!(
                        "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] /F 4 /A << /Type /Action /S /URI /URI ({}) >> >>",
                        num(x1),
                        num(y1),
                        num(x2),
                        num(y2),
                        escape_literal(&a.url)
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
                "trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}

/// String literal PDF `(...)` (ASCII; selain itu di-escape oktal).
fn escape_literal(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => {
                let b = win_ansi(c).unwrap_or(b'?');
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out
}

/// Logo Timemoz (PNG) yang dibundel; dekode sekali (`loadInvoiceLogo`). `None` → header jatuh
/// ke teks "Timemoz".
pub fn logo() -> Option<Image> {
    static LOGO: &[u8] = include_bytes!("../../assets/timemoz-logo.png");
    static IMG: OnceLock<Option<Image>> = OnceLock::new();
    IMG.get_or_init(|| Image::from_png(LOGO)).clone()
}

// ───────────────────────── pembungkus teks (LineWrapper pdfkit) ─────────────────────────

struct Engine {
    font: Font,
    size: f64,
    color: Rgb,
    // Keadaan LineWrapper
    x: f64,
    y: f64,
    start_x: f64,
    line_width: f64,
    space_left: f64,
    max_y: f64,
    has_height: bool,
    continued_x: f64,
    indent_active: f64,
    // Buffer baris
    buffer: String,
    text_width: f64,
    wc: u32,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    fn run(
        doc: &mut Doc,
        text: &str,
        x: f64,
        y: f64,
        font: Font,
        size: f64,
        color: Rgb,
        o: &TextOpts,
    ) {
        let width = o.width.unwrap_or(PAGE_W - x - MARGIN).max(0.0);
        let max_y = match o.height {
            Some(h) => round6(y + h),
            None => round6(PAGE_H - doc.margin_bottom),
        };
        let mut e = Self {
            font,
            size,
            color,
            x,
            y,
            start_x: x,
            line_width: width,
            space_left: width,
            max_y,
            has_height: o.height.is_some(),
            continued_x: o.indent_first,
            indent_active: 0.0,
            buffer: String::new(),
            text_width: 0.0,
            wc: 0,
        };
        e.wrap(doc, text, o);
    }

    fn word_width(&self, w: &str) -> f64 {
        round6(text_width(w, self.font, self.size))
    }

    fn next_section(&mut self, doc: &mut Doc) -> bool {
        if self.has_height {
            return false;
        }
        doc.next_page_for_text();
        self.y = MARGIN;
        self.x = self.start_x;
        self.max_y = round6(PAGE_H - doc.margin_bottom);
        true
    }

    fn emit_line(&mut self, doc: &mut Doc, o: &TextOpts) {
        let (buffer, tw, lw) = (self.buffer.clone(), self.text_width, self.line_width);
        doc.last_text_width = tw;
        doc.fragment(
            &buffer, self.x, self.y, self.font, self.size, self.color, o, lw, tw,
        );
        self.y += line_height(self.font, self.size);
        // `once('line')` dari firstLine: kembalikan indentasi.
        if self.indent_active != 0.0 {
            self.x -= self.indent_active;
            self.line_width += self.indent_active;
            self.indent_active = 0.0;
            self.continued_x = 0.0;
        }
    }

    fn first_line(&mut self) {
        let indent = self.continued_x;
        self.x += indent;
        self.line_width -= indent;
        self.indent_active = indent;
    }

    /// Isi callback `eachWord` di `LineWrapper.wrap`. `last`: `None` = belum ada kata
    /// sebelumnya, `Some(required)`. Mengembalikan `false` untuk berhenti.
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
            self.first_line();
            self.space_left = self.line_width;
        }
        let can_fit = w <= self.space_left;
        if can_fit {
            self.buffer.push_str(word);
            self.text_width += w;
            self.wc += 1;
        }
        if required || !can_fit {
            let lh = line_height(self.font, self.size);
            if required && w > self.space_left {
                self.emit_line(doc, o);
                self.buffer = word.to_owned();
                self.text_width = w;
                self.wc = 1;
            }
            self.emit_line(doc, o);
            if round6(self.y + lh) > self.max_y && !self.next_section(doc) {
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
        } else {
            self.space_left -= w;
            true
        }
    }

    fn wrap(&mut self, doc: &mut Doc, text: &str, o: &TextOpts) {
        let lh = line_height(self.font, self.size);
        if self.y > self.max_y || self.y + lh > self.max_y {
            let _ = self.next_section(doc);
        }
        doc.fill_color(self.color);
        let chars: Vec<char> = text.chars().collect();
        let bks = breaks::breaks(&chars);
        let mut prev_pos = 0usize;
        let mut last: Option<bool> = None;
        for (pos, req) in bks {
            let word: String = chars
                .get(prev_pos..pos)
                .unwrap_or_default()
                .iter()
                .collect();
            prev_pos = pos;
            let mut w = self.word_width(&word);
            let mut cont = true;
            if w > self.line_width + self.continued_x {
                let mut lbk = last;
                let mut rest: Vec<char> = word.chars().collect();
                while !rest.is_empty() {
                    let len = rest.len();
                    let mut l;
                    let mut might_grow = false;
                    if w > self.space_left {
                        l = (self.space_left / (w / len as f64)).ceil().max(0.0) as usize;
                        l = l.min(len);
                        w = self.word_width(&slice(&rest, l));
                        might_grow = w <= self.space_left && l < len;
                    } else {
                        l = len;
                    }
                    let mut must_shrink = w > self.space_left && l > 0;
                    while must_shrink || might_grow {
                        if must_shrink {
                            l -= 1;
                            w = self.word_width(&slice(&rest, l));
                            must_shrink = w > self.space_left && l > 0;
                        } else {
                            l += 1;
                            w = self.word_width(&slice(&rest, l));
                            must_shrink = w > self.space_left && l > 0;
                            might_grow = w <= self.space_left && l < len;
                        }
                    }
                    if l == 0 && self.space_left == self.line_width {
                        l = 1;
                    }
                    let fbk_required = req || l < len;
                    let piece = slice(&rest, l);
                    // `w` sudah lebar potongan pertama; bila `l` dipaksa 1, hitung ulang.
                    let wpiece = self.word_width(&piece);
                    cont = self.process(doc, o, &piece, wpiece, fbk_required, lbk);
                    lbk = Some(false);
                    rest = rest.get(l..).unwrap_or_default().to_vec();
                    w = self.word_width(&rest.iter().collect::<String>());
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

    #[test]
    fn widths_match_pdfkit() {
        // Diukur dengan pdfkit 0.17.2: "www.timemoz.com  |  support@timemoz.com" 8.5pt.
        assert!((text_width("Hello", Font::Regular, 10.0) - 22.78).abs() < 1e-9);
        assert!((text_width("Hello", Font::Bold, 10.0) - 24.45).abs() < 1e-9);
        // Kerning: "TO" bold (T,O = -?) lebih sempit dari jumlah lebar glyph.
        let plain = f64::from(afm::W_BOLD[usize::from(b'T')] + afm::W_BOLD[usize::from(b'O')])
            * 9.0
            / 1000.0;
        assert!(text_width("TO", Font::Bold, 9.0) < plain);
    }

    #[test]
    fn num_format() {
        assert_eq!(num(0.0), "0");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(495.28), "495.28");
        assert_eq!(num(1.0 / 3.0), "0.333333");
        assert_eq!(num(5.0), "5");
    }

    #[test]
    fn structure_and_tj() {
        let mut d = Doc::new();
        d.fill_rect(10.0, 10.0, 50.0, 20.0, Rgb::hex("#1a56db"));
        d.text_at(
            "Hello (x)",
            50.0,
            50.0,
            Font::Regular,
            10.0,
            Rgb::hex("#000000"),
        );
        let bytes = d.finish();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("%PDF-1.3"));
        assert!(s.contains("/Count 1"));
        // hex: "Hello (x)"
        assert!(s.contains("<48656c6c6f20287829>"));
        assert!(s.trim_end().ends_with("%%EOF"));
        let xref = s.rfind("startxref").unwrap_or(0);
        let pos: usize = s
            .get(xref + 10..)
            .and_then(|t| t.lines().next())
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        assert!(bytes.get(pos..).is_some_and(|t| t.starts_with(b"xref")));
    }

    #[test]
    fn right_align_and_wrap() {
        let mut d = Doc::new();
        // Lebar 40pt: "aaaa bbbb cccc" harus pecah jadi beberapa baris.
        d.text(
            "aaaa bbbb cccc",
            50.0,
            50.0,
            Font::Regular,
            10.0,
            Rgb::hex("#000000"),
            &TextOpts::width(40.0),
        );
        let s = String::from_utf8_lossy(&d.finish()).into_owned();
        assert!(s.matches("TJ").count() >= 3, "{s}");
    }

    #[test]
    fn page_break_when_past_margin() {
        let mut d = Doc::new();
        d.text_at(
            "late",
            50.0,
            800.0,
            Font::Regular,
            10.0,
            Rgb::hex("#000000"),
        );
        assert_eq!(d.page_count(), 2);
    }

    #[test]
    fn height_clips_lines() {
        let mut d = Doc::new();
        let long = "word ".repeat(60);
        d.text(
            &long,
            60.0,
            100.0,
            Font::Regular,
            10.0,
            Rgb::hex("#000000"),
            &TextOpts {
                width: Some(100.0),
                height: Some(30.0),
                ..TextOpts::default()
            },
        );
        let s = String::from_utf8_lossy(&d.finish()).into_owned();
        assert_eq!(s.matches("TJ").count(), 2, "{s}");
    }

    #[test]
    fn logo_decodes() {
        assert!(logo().is_some());
    }
}
