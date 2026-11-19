//! Penulis PDF minimal (Helvetica / Helvetica-Bold standar, WinAnsi) — pengganti pdfkit.
//! Koordinat berasal dari kiri-atas (seperti pdfkit); konversi ke sistem PDF dilakukan di sini.

use std::fmt::Write as _;
use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::afm::{HELVETICA, HELVETICA_BOLD};

/// Kertas A4 portrait, point.
pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    fn resource(self) -> &'static str {
        match self {
            Self::Regular => "F1",
            Self::Bold => "F2",
        }
    }
}

/// Unicode -> byte WinAnsi (cp1252) untuk karakter yang lolos `cleanText`; lainnya `?`.
pub fn to_winansi(c: char) -> u8 {
    match c {
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => u8::try_from(u32::from(c)).unwrap_or(b'?'),
        '–' => 0x96,
        '—' => 0x97,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '…' => 0x85,
        _ => b'?',
    }
}

/// Lebar teks dalam point.
pub fn text_width(text: &str, font: Font, size: f64) -> f64 {
    let table = match font {
        Font::Regular => &HELVETICA,
        Font::Bold => &HELVETICA_BOLD,
    };
    let units: u32 = text
        .chars()
        .map(|c| {
            let b = to_winansi(c);
            table
                .get(usize::from(b).saturating_sub(32))
                .copied()
                .map_or(278, u32::from)
        })
        .sum();
    f64::from(units) / 1000.0 * size
}

/// Tinggi baris pdfkit untuk Helvetica: (ascender - descender + lineGap) = 1.156 em.
pub fn line_height(size: f64) -> f64 {
    size * 1.156
}

/// Ascender Helvetica (baseline = y atas + ascender).
const ASCENDER: f64 = 0.718;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

fn rgb(hex: &str) -> (f64, f64, f64) {
    let h = hex.trim_start_matches('#');
    let c = |i: usize| {
        h.get(i..i + 2)
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .map_or(0.0, |v| f64::from(v) / 255.0)
    };
    (c(0), c(2), c(4))
}

fn num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

struct Page {
    content: Vec<u8>,
}

pub struct Doc {
    pages: Vec<Page>,
    pub title: String,
    pub author: String,
    pub creator: String,
}

impl Doc {
    pub fn new(title: &str, author: &str, creator: &str) -> Self {
        Self {
            pages: vec![Page {
                content: Vec::new(),
            }],
            title: title.to_owned(),
            author: author.to_owned(),
            creator: creator.to_owned(),
        }
    }

    pub fn add_page(&mut self) {
        self.pages.push(Page {
            content: Vec::new(),
        });
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Pilih halaman aktif (untuk footer); gambar berikutnya masuk ke halaman itu.
    fn emit(&mut self, page: usize, s: &str) {
        if let Some(p) = self.pages.get_mut(page) {
            p.content.extend_from_slice(s.as_bytes());
        }
    }

    fn last(&self) -> usize {
        self.pages.len().saturating_sub(1)
    }

    fn fill_color(hex: &str) -> String {
        let (r, g, b) = rgb(hex);
        format!("{} {} {} rg\n", num(r), num(g), num(b))
    }

    fn stroke_color(hex: &str) -> String {
        let (r, g, b) = rgb(hex);
        format!("{} {} {} RG\n", num(r), num(g), num(b))
    }

    /// Path persegi membulat (koordinat atas-kiri) dalam sistem PDF.
    fn rrect_path(x: f64, y: f64, w: f64, h: f64, r: f64) -> String {
        let r = r.max(0.0).min(w / 2.0).min(h / 2.0);
        let (x0, x1) = (x, x + w);
        let (y0, y1) = (PAGE_H - (y + h), PAGE_H - y); // bawah, atas (PDF)
        if r <= 0.0 {
            return format!("{} {} {} {} re\n", num(x0), num(y0), num(w), num(h));
        }
        let k = 0.552_284_75 * r;
        let mut p = String::new();
        let _ = writeln!(p, "{} {} m", num(x0 + r), num(y0));
        let _ = writeln!(p, "{} {} l", num(x1 - r), num(y0));
        let _ = writeln!(
            p,
            "{} {} {} {} {} {} c",
            num(x1 - r + k),
            num(y0),
            num(x1),
            num(y0 + r - k),
            num(x1),
            num(y0 + r)
        );
        let _ = writeln!(p, "{} {} l", num(x1), num(y1 - r));
        let _ = writeln!(
            p,
            "{} {} {} {} {} {} c",
            num(x1),
            num(y1 - r + k),
            num(x1 - r + k),
            num(y1),
            num(x1 - r),
            num(y1)
        );
        let _ = writeln!(p, "{} {} l", num(x0 + r), num(y1));
        let _ = writeln!(
            p,
            "{} {} {} {} {} {} c",
            num(x0 + r - k),
            num(y1),
            num(x0),
            num(y1 - r + k),
            num(x0),
            num(y1 - r)
        );
        let _ = writeln!(p, "{} {} l", num(x0), num(y0 + r));
        let _ = writeln!(
            p,
            "{} {} {} {} {} {} c",
            num(x0),
            num(y0 + r - k),
            num(x0 + r - k),
            num(y0),
            num(x0 + r),
            num(y0)
        );
        p.push_str("h\n");
        p
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, fill: &str) {
        self.rrect(x, y, w, h, 0.0, fill);
    }

    pub fn rrect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, fill: &str) {
        let s = format!(
            "q\n{}{}f\nQ\n",
            Self::fill_color(fill),
            Self::rrect_path(x, y, w, h, r)
        );
        self.emit(self.last(), &s);
    }

    pub fn rrect_stroke(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        r: f64,
        fill: &str,
        stroke: &str,
    ) {
        let s = format!(
            "q\n{}{}0.8 w\n{}B\nQ\n",
            Self::fill_color(fill),
            Self::stroke_color(stroke),
            Self::rrect_path(x, y, w, h, r)
        );
        self.emit(self.last(), &s);
    }

    pub fn circle(&mut self, cx: f64, cy: f64, r: f64, fill: &str) {
        self.rrect(cx - r, cy - r, r * 2.0, r * 2.0, r, fill);
    }

    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, weight: f64, color: &str) {
        let page = self.last();
        self.line_on(page, x1, y1, x2, y2, weight, color);
    }

    /// Garis di halaman tertentu.
    pub fn line_on(
        &mut self,
        page: usize,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        weight: f64,
        color: &str,
    ) {
        let s = format!(
            "q\n{}{} w\n{} {} m\n{} {} l\nS\nQ\n",
            Self::stroke_color(color),
            num(weight),
            num(x1),
            num(PAGE_H - y1),
            num(x2),
            num(PAGE_H - y2)
        );
        self.emit(page, &s);
    }

    /// Satu baris teks; `y` = sisi atas kotak teks (seperti pdfkit).
    pub fn text_at(
        &mut self,
        page: Option<usize>,
        text: &str,
        x: f64,
        y: f64,
        font: Font,
        size: f64,
        color: &str,
    ) {
        let baseline = PAGE_H - (y + ASCENDER * size);
        let mut bytes: Vec<u8> = Vec::new();
        for c in text.chars() {
            let b = to_winansi(c);
            match b {
                b'\\' | b'(' | b')' => {
                    bytes.push(b'\\');
                    bytes.push(b);
                }
                0x20..=0x7e => bytes.push(b),
                _ => bytes.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            }
        }
        let mut out = format!(
            "BT\n{}/{} {} Tf\n{} {} Td\n(",
            Self::fill_color(color),
            font.resource(),
            num(size),
            num(x),
            num(baseline)
        )
        .into_bytes();
        out.extend_from_slice(&bytes);
        out.extend_from_slice(b") Tj\nET\n");
        let idx = page.unwrap_or_else(|| self.last());
        if let Some(p) = self.pages.get_mut(idx) {
            p.content.extend_from_slice(&out);
        }
    }

    /// Selesaikan dokumen menjadi bytes PDF.
    pub fn finish(self) -> Vec<u8> {
        let mut out: Vec<u8> = b"%PDF-1.3\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets: Vec<usize> = Vec::new();
        let n_pages = self.pages.len();
        // Objek: 1 Catalog, 2 Pages, 3 F1, 4 F2, 5 Info, lalu (page, content) berpasangan.
        let page_obj = |i: usize| 6 + i * 2;
        let content_obj = |i: usize| 7 + i * 2;

        let mut objs: Vec<Vec<u8>> = Vec::new();
        objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        let kids: String = (0..n_pages)
            .map(|i| format!("{} 0 R", page_obj(i)))
            .collect::<Vec<_>>()
            .join(" ");
        objs.push(format!("<< /Type /Pages /Count {n_pages} /Kids [{kids}] >>").into_bytes());
        objs.push(
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        );
        objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec());
        objs.push(
            format!(
                "<< /Title ({}) /Author ({}) /Creator ({}) /Producer (Timemoz) >>",
                pdf_text(&self.title),
                pdf_text(&self.author),
                pdf_text(&self.creator)
            )
            .into_bytes(),
        );
        for (i, page) in self.pages.iter().enumerate() {
            objs.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                    num(PAGE_W),
                    num(PAGE_H),
                    content_obj(i)
                )
                .into_bytes(),
            );
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            let _ = enc.write_all(&page.content);
            let compressed = enc.finish().unwrap_or_default();
            let mut stream = format!(
                "<< /Length {} /Filter /FlateDecode >>\nstream\n",
                compressed.len()
            )
            .into_bytes();
            stream.extend_from_slice(&compressed);
            stream.extend_from_slice(b"\nendstream");
            objs.push(stream);
        }

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
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
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

/// Escape string literal PDF (ASCII/latin-1 saja; sisanya `?`).
fn pdf_text(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            '\u{20}'..='\u{7e}' => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Pecah teks per baris dengan lebar maksimum `width` (greedy per kata; kata terlalu panjang dipotong).
pub fn wrap(text: &str, width: f64, font: Font, size: f64) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let candidate = if cur.is_empty() {
            word.to_owned()
        } else {
            format!("{cur} {word}")
        };
        if text_width(&candidate, font, size) <= width || cur.is_empty() && word.is_empty() {
            cur = candidate;
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        // kata sendiri lebih lebar dari baris: potong per karakter
        let mut piece = String::new();
        for c in word.chars() {
            let mut next = piece.clone();
            next.push(c);
            if text_width(&next, font, size) > width && !piece.is_empty() {
                lines.push(std::mem::take(&mut piece));
                piece.push(c);
            } else {
                piece = next;
            }
        }
        cur = piece;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_match_afm() {
        // Helvetica: "Hello" = H722 e556 l222 l222 o556 = 2278 -> 22.78pt @10
        assert!((text_width("Hello", Font::Regular, 10.0) - 22.78).abs() < 1e-9);
        // Bold: H722 e556 l278 l278 o611 = 2445
        assert!((text_width("Hello", Font::Bold, 10.0) - 24.45).abs() < 1e-9);
        assert_eq!(text_width("", Font::Regular, 9.0), 0.0);
    }

    #[test]
    fn winansi_mapping() {
        assert_eq!(to_winansi('A'), b'A');
        assert_eq!(to_winansi('é'), 0xE9);
        assert_eq!(to_winansi('–'), 0x96);
        assert_eq!(to_winansi('日'), b'?');
    }

    #[test]
    fn wrap_breaks_words_and_long_tokens() {
        let lines = wrap(
            "aaa bbb ccc",
            text_width("aaa bbb", Font::Regular, 10.0),
            Font::Regular,
            10.0,
        );
        assert_eq!(lines, vec!["aaa bbb", "ccc"]);
        let long = wrap(&"x".repeat(100), 50.0, Font::Regular, 10.0);
        assert!(
            long.len() > 1
                && long
                    .iter()
                    .all(|l| text_width(l, Font::Regular, 10.0) <= 50.0)
        );
        assert_eq!(wrap("", 10.0, Font::Regular, 10.0), vec![String::new()]);
    }

    #[test]
    fn produces_valid_pdf_skeleton() {
        let mut d = Doc::new("T", "A", "C");
        d.rect(10.0, 10.0, 50.0, 20.0, "#0f2a5f");
        d.text_at(
            None,
            "Hi (there) é",
            10.0,
            40.0,
            Font::Bold,
            12.0,
            "#ffffff",
        );
        d.add_page();
        d.line(0.0, 0.0, 100.0, 0.0, 0.5, "#e2e8f0");
        d.circle(50.0, 50.0, 5.0, "#1d4ed8");
        d.rrect_stroke(5.0, 5.0, 40.0, 40.0, 6.0, "#ffffff", "#e2e8f0");
        let bytes = d.finish();
        assert!(bytes.starts_with(b"%PDF-1.3"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Count 2"));
        assert!(text.contains("startxref"));
    }
}
