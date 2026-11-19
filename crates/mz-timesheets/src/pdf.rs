//! Penulis PDF minimal (Rust murni, tanpa dependensi): halaman A4, font bawaan
//! Helvetica / Helvetica-Bold (WinAnsi), persegi, persegi bulat, garis, dan teks
//! dengan pembungkusan kata. Koordinat meniru pdfkit: titik (0,0) di KIRI ATAS,
//! satuan point, `y` teks = bagian atas baris.

use std::fmt::Write as _;

pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    /// `#rrggbb`.
    pub fn hex(s: &str) -> Self {
        let h = s.trim_start_matches('#');
        let c = |i: usize| {
            h.get(i..i + 2)
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .map_or(0.0, |v| f64::from(v) / 255.0)
        };
        Self(c(0), c(2), c(4))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    fn res(self) -> &'static str {
        match self {
            Self::Regular => "F1",
            Self::Bold => "F2",
        }
    }
}

// Lebar glyph AFM Helvetica untuk ASCII 32..=126 (per 1000 em).
const W_REGULAR: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // 32-47
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0-9
    278, 278, 584, 584, 584, 556, 1015, // : ; < = > ? @
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A-Z
    278, 278, 278, 469, 556, 333, // [ \ ] ^ _ `
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, // a-z
    334, 260, 334, 584, // { | } ~
];

// Lebar glyph AFM Helvetica-Bold.
const W_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, //
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, //
    333, 333, 584, 584, 584, 611, 975, //
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, //
    333, 278, 333, 584, 556, 333, //
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556,
    333, 611, 556, 778, 556, 556, 500, //
    389, 280, 389, 584,
];

/// Byte WinAnsi untuk satu karakter (`?` bila tidak ada).
fn win_ansi(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{00A0}'..='\u{00FF}' => c as u32 as u8,
        _ => b'?',
    }
}

fn glyph_width(c: char, font: Font) -> f64 {
    let table = match font {
        Font::Regular => &W_REGULAR,
        Font::Bold => &W_BOLD,
    };
    let w = match c {
        ' '..='~' => table
            .get((c as usize).wrapping_sub(32))
            .copied()
            .unwrap_or(556),
        '\u{2013}' => 556,
        '\u{2014}' => 1000,
        '\u{00D7}' => 584,
        _ => 556,
    };
    f64::from(w)
}

/// Lebar teks dalam point.
pub fn text_width(s: &str, font: Font, size: f64, char_spacing: f64) -> f64 {
    let n = s.chars().count() as f64;
    s.chars().map(|c| glyph_width(c, font)).sum::<f64>() * size / 1000.0 + char_spacing * n
}

/// Tinggi baris pdfkit untuk Helvetica (ascender + descender + lineGap ≈ 1.156 em).
pub fn line_height(size: f64) -> f64 {
    size * 1.156
}

/// Pembungkusan kata seperti `doc.text(..., {width})`.
pub fn wrap(s: &str, width: f64, font: Font, size: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.split('\n') {
        let mut cur = String::new();
        for word in para.split(' ') {
            let cand = if cur.is_empty() {
                word.to_owned()
            } else {
                format!("{cur} {word}")
            };
            if !cur.is_empty() && text_width(&cand, font, size, 0.0) > width {
                lines.push(std::mem::take(&mut cur));
                cur = word.to_owned();
            } else {
                cur = cand;
            }
        }
        lines.push(cur);
    }
    lines
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let b = win_ansi(c);
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Debug, Default)]
pub struct Pdf {
    pages: Vec<String>,
    cur: String,
    started: bool,
}

fn n(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

impl Pdf {
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            cur: String::new(),
            started: true,
        }
    }

    /// `doc.addPage()`.
    pub fn add_page(&mut self) {
        self.pages.push(std::mem::take(&mut self.cur));
    }

    pub fn page_count(&self) -> usize {
        self.pages.len() + usize::from(self.started)
    }

    fn color(&mut self, c: Rgb, op: &str) {
        let _ = writeln!(self.cur, "{} {} {} {op}", n(c.0), n(c.1), n(c.2));
    }

    /// `rect(x, y, w, h).fill(color)`.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgb) {
        self.color(c, "rg");
        let _ = writeln!(
            self.cur,
            "{} {} {} {} re f",
            n(x),
            n(PAGE_H - y - h),
            n(w),
            n(h)
        );
    }

    /// `roundedRect(x, y, w, h, r).fill(color)`.
    pub fn fill_round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, c: Rgb) {
        self.color(c, "rg");
        let k = 0.5523 * r;
        let (x0, y0) = (x, PAGE_H - y - h);
        let (x1, y1) = (x + w, PAGE_H - y);
        let _ = writeln!(
            self.cur,
            "{} {} m {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c h f",
            n(x0 + r),
            n(y0),
            n(x1 - r),
            n(y0),
            n(x1 - r + k),
            n(y0),
            n(x1),
            n(y0 + r - k),
            n(x1),
            n(y0 + r),
            n(x1),
            n(y1 - r),
            n(x1),
            n(y1 - r + k),
            n(x1 - r + k),
            n(y1),
            n(x1 - r),
            n(y1),
            n(x0 + r),
            n(y1),
            n(x0 + r - k),
            n(y1),
            n(x0),
            n(y1 - r + k),
            n(x0),
            n(y1 - r),
            n(x0),
            n(y0 + r),
            n(x0),
            n(y0 + r - k),
            n(x0 + r - k),
            n(y0),
            n(x0 + r),
            n(y0),
        );
    }

    /// `moveTo().lineTo().strokeColor().lineWidth().stroke()`.
    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c: Rgb, width: f64) {
        self.color(c, "RG");
        let _ = writeln!(
            self.cur,
            "{} w {} {} m {} {} l S",
            n(width),
            n(x1),
            n(PAGE_H - y1),
            n(x2),
            n(PAGE_H - y2)
        );
    }

    /// Satu baris teks; `y` = atas baris, `x` = tepi kiri.
    pub fn text(&mut self, x: f64, y: f64, s: &str, font: Font, size: f64, c: Rgb) {
        self.text_spaced(x, y, s, font, size, c, 0.0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_spaced(
        &mut self,
        x: f64,
        y: f64,
        s: &str,
        font: Font,
        size: f64,
        c: Rgb,
        spacing: f64,
    ) {
        // Garis dasar = atas baris + ascender Helvetica (718/1000).
        let baseline = PAGE_H - (y + size * 0.718);
        self.color(c, "rg");
        let _ = writeln!(
            self.cur,
            "BT /{} {} Tf {} Tc {} {} Td ({}) Tj ET",
            font.res(),
            n(size),
            n(spacing),
            n(x),
            n(baseline),
            escape(s)
        );
    }

    /// Teks dalam kotak lebar `width` (pembungkusan + perataan); mengembalikan tinggi terpakai.
    #[allow(clippy::too_many_arguments)]
    pub fn text_box(
        &mut self,
        x: f64,
        y: f64,
        width: f64,
        s: &str,
        font: Font,
        size: f64,
        c: Rgb,
        align: Align,
    ) -> f64 {
        let lh = line_height(size);
        let mut yy = y;
        for line in wrap(s, width, font, size) {
            let w = text_width(&line, font, size, 0.0);
            let lx = match align {
                Align::Left => x,
                Align::Right => x + width - w,
                Align::Center => x + (width - w) / 2.0,
            };
            self.text(lx, yy, &line, font, size, c);
            yy += lh;
        }
        yy - y
    }

    /// Rakit berkas PDF.
    pub fn finish(mut self) -> Vec<u8> {
        self.pages.push(std::mem::take(&mut self.cur));
        let pages = self.pages;
        let count = pages.len();
        let mut objs: Vec<String> = Vec::new();
        objs.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
        let kids: Vec<String> = (0..count).map(|i| format!("{} 0 R", 5 + i * 2)).collect();
        objs.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {count} >>",
            kids.join(" ")
        ));
        objs.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        );
        objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_owned());
        for (i, content) in pages.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                n(PAGE_W),
                n(PAGE_H),
                6 + i * 2
            ));
            objs.push(format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            ));
        }
        let mut out: Vec<u8> = b"%PDF-1.3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
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
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_match_afm() {
        assert!((text_width("Hello", Font::Regular, 10.0, 0.0) - 22.78).abs() < 1e-9);
        assert!((text_width("Hello", Font::Bold, 10.0, 0.0) - 24.45).abs() < 1e-9);
        assert!((text_width("ab", Font::Regular, 10.0, 0.5) - 12.12).abs() < 1e-9);
    }

    #[test]
    fn wraps_words() {
        let lines = wrap(
            "aaa bbb ccc",
            text_width("aaa bbb", Font::Regular, 10.0, 0.0),
            Font::Regular,
            10.0,
        );
        assert_eq!(lines, vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("a\nb", 100.0, Font::Regular, 10.0), vec!["a", "b"]);
    }

    #[test]
    fn produces_valid_structure() {
        let mut p = Pdf::new();
        p.fill_rect(10.0, 10.0, 50.0, 20.0, Rgb::hex("#1a56db"));
        p.fill_round_rect(10.0, 40.0, 50.0, 20.0, 4.0, Rgb::hex("#f3f4f6"));
        p.line(0.0, 80.0, 100.0, 80.0, Rgb::hex("#e5e7eb"), 1.0);
        p.text(
            10.0,
            100.0,
            "Timesheet (Report) \u{2013} x",
            Font::Bold,
            12.0,
            Rgb::hex("#000000"),
        );
        p.add_page();
        p.text(
            10.0,
            10.0,
            "Page 2",
            Font::Regular,
            9.0,
            Rgb::hex("#000000"),
        );
        assert_eq!(p.page_count(), 2);
        let bytes = p.finish();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("%PDF-1.3"));
        assert!(s.contains("/Count 2"));
        assert!(s.contains("Timesheet \\(Report\\) \\226 x"));
        assert!(s.trim_end().ends_with("%%EOF"));
        let xref = s.rfind("startxref").unwrap_or(0);
        let pos: usize = s[xref + 10..]
            .lines()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        assert!(s[pos..].starts_with("xref"));
    }
}
