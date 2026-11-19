//! Penulis PDF minimal Rust murni untuk laporan absensi A4 LANDSCAPE.
//!
//! DUPLIKAT (diperluas: ukuran halaman bebas, pembungkusan per huruf untuk kata panjang) dari
//! `mz-timesheets::pdf` — sengaja dibiarkan (dicek 2026-11-18: lebar glyph `–`/`×`/`…`, pembungkusan
//! kata panjang, dan format angka berbeda; menyatukan mengubah byte PDF salah satu). Koordinat ala pdfkit: (0,0) di kiri atas, `y` teks =
//! atas baris, font standar Helvetica/Helvetica-Bold (WinAnsi), lebar glyph AFM tanpa kerning.

use std::fmt::Write as _;

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
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

// Lebar glyph AFM Helvetica-Bold.
const W_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

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
        '\u{2026}' => 0x85,
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
        '\u{2014}' | '\u{2026}' => 1000,
        _ => 556,
    };
    f64::from(w)
}

/// Lebar teks dalam point.
pub fn text_width(s: &str, font: Font, size: f64) -> f64 {
    s.chars().map(|c| glyph_width(c, font)).sum::<f64>() * size / 1000.0
}

/// Tinggi baris pdfkit untuk Helvetica (≈ 1.156 em).
pub fn line_height(size: f64) -> f64 {
    size * 1.156
}

/// Pembungkusan `doc.text(..., { width })`: per kata, kata yang lebih lebar dari baris dipecah
/// per huruf (seperti `LineWrapper` pdfkit).
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
            if text_width(&cand, font, size) <= width {
                cur = cand;
                continue;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            // Kata sendirian masih terlalu lebar → pecah per huruf.
            for ch in word.chars() {
                let mut next = cur.clone();
                next.push(ch);
                if !cur.is_empty() && text_width(&next, font, size) > width {
                    lines.push(std::mem::take(&mut cur));
                    cur.push(ch);
                } else {
                    cur = next;
                }
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

fn n(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Dokumen dengan halaman berukuran `w` × `h` point (semua halaman sama).
#[derive(Debug)]
pub struct Pdf {
    w: f64,
    h: f64,
    pages: Vec<String>,
    /// Indeks halaman yang sedang ditulis (`switchToPage`).
    cur: usize,
}

impl Pdf {
    pub fn new(w: f64, h: f64) -> Self {
        Self {
            w,
            h,
            pages: vec![String::new()],
            cur: 0,
        }
    }

    /// `doc.addPage()`.
    pub fn add_page(&mut self) {
        self.pages.push(String::new());
        self.cur = self.pages.len() - 1;
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// `doc.switchToPage(i)` (`bufferPages: true`).
    pub fn switch_to_page(&mut self, i: usize) {
        if i < self.pages.len() {
            self.cur = i;
        }
    }

    fn emit(&mut self, s: &str) {
        if let Some(p) = self.pages.get_mut(self.cur) {
            p.push_str(s);
        }
    }

    fn color(&mut self, c: Rgb, op: &str) {
        let line = format!("{} {} {} {op}\n", n(c.0), n(c.1), n(c.2));
        self.emit(&line);
    }

    /// `rect(x, y, w, h).fill(color)`.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgb) {
        self.color(c, "rg");
        let line = format!("{} {} {} {} re f\n", n(x), n(self.h - y - h), n(w), n(h));
        self.emit(&line);
    }

    /// `roundedRect(x, y, w, h, r).fill(color)`.
    pub fn fill_round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, c: Rgb) {
        self.color(c, "rg");
        let k = 0.5523 * r;
        let (x0, y0) = (x, self.h - y - h);
        let (x1, y1) = (x + w, self.h - y);
        let pts = [
            (x0 + r, y0),
            (x1 - r, y0),
            (x1 - r + k, y0),
            (x1, y0 + r - k),
            (x1, y0 + r),
            (x1, y1 - r),
            (x1, y1 - r + k),
            (x1 - r + k, y1),
            (x1 - r, y1),
            (x0 + r, y1),
            (x0 + r - k, y1),
            (x0, y1 - r + k),
            (x0, y1 - r),
            (x0, y0 + r),
            (x0, y0 + r - k),
            (x0 + r - k, y0),
            (x0 + r, y0),
        ];
        let p = |i: usize| {
            pts.get(i)
                .map_or_else(|| "0 0".to_owned(), |(a, b)| format!("{} {}", n(*a), n(*b)))
        };
        let line = format!(
            "{} m {} l {} {} {} c {} l {} {} {} c {} l {} {} {} c {} l {} {} {} c h f\n",
            p(0),
            p(1),
            p(2),
            p(3),
            p(4),
            p(5),
            p(6),
            p(7),
            p(8),
            p(9),
            p(10),
            p(11),
            p(12),
            p(13),
            p(14),
            p(15),
            p(16),
        );
        self.emit(&line);
    }

    /// `moveTo().lineTo().strokeColor().lineWidth().stroke()`.
    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c: Rgb, width: f64) {
        self.color(c, "RG");
        let line = format!(
            "{} w {} {} m {} {} l S\n",
            n(width),
            n(x1),
            n(self.h - y1),
            n(x2),
            n(self.h - y2)
        );
        self.emit(&line);
    }

    /// Satu baris teks; `y` = atas baris.
    pub fn text(&mut self, x: f64, y: f64, s: &str, font: Font, size: f64, c: Rgb) {
        let baseline = self.h - (y + size * 0.718);
        self.color(c, "rg");
        let line = format!(
            "BT /{} {} Tf {} {} Td ({}) Tj ET\n",
            font.res(),
            n(size),
            n(x),
            n(baseline),
            escape(s)
        );
        self.emit(&line);
    }

    /// `doc.text(s, x, y, { width, align })`: dibungkus, baris berikut di bawahnya.
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
    ) {
        let lh = line_height(size);
        for (i, line) in wrap(s, width, font, size).iter().enumerate() {
            let lx = match align {
                Align::Left => x,
                Align::Right => x + width - text_width(line.trim_end(), font, size),
            };
            #[allow(clippy::cast_precision_loss)]
            self.text(lx, y + lh * i as f64, line, font, size, c);
        }
    }

    /// Rakit berkas PDF.
    pub fn finish(self) -> Vec<u8> {
        let count = self.pages.len();
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
        objs.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        );
        for (i, content) in self.pages.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                n(self.w),
                n(self.h),
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
    fn wrap_splits_long_words() {
        let w = text_width("aaaa", Font::Regular, 10.0);
        assert_eq!(
            wrap("aaaaaaaaa b", w, Font::Regular, 10.0),
            vec!["aaaa", "aaaa", "a b"]
        );
        assert_eq!(wrap("a\nb", 100.0, Font::Regular, 10.0), vec!["a", "b"]);
    }

    #[test]
    fn landscape_structure() {
        let mut p = Pdf::new(841.89, 595.28);
        p.fill_round_rect(10.0, 10.0, 50.0, 20.0, 5.0, Rgb::hex("#f3f4f6"));
        p.text(10.0, 40.0, "Hi (x)", Font::Bold, 12.0, Rgb::hex("#000000"));
        p.add_page();
        p.switch_to_page(0);
        p.text(
            10.0,
            60.0,
            "Page 1 of 2",
            Font::Regular,
            8.0,
            Rgb::hex("#6b7280"),
        );
        let s = String::from_utf8_lossy(&p.finish()).into_owned();
        assert!(s.starts_with("%PDF-1.3"));
        assert!(s.contains("/Count 2"));
        assert!(s.contains("/MediaBox [0 0 841.89 595.28]"));
        assert!(s.contains("Hi \\(x\\)"));
    }
}
