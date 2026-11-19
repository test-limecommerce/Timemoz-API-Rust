//! Kesempatan putus baris (UAX #14 yang disederhanakan) untuk teks invoice / form.
//!
//! pdfkit memakai pustaka `linebreak` (UAX #14 penuh). Teks invoice hanya ASCII/Latin-1
//! (deskripsi item, nama, alamat, URL pembayaran), jadi yang ditiru: putus setelah spasi,
//! setelah tanda hubung (bukan sebelum angka), setelah `/` (bukan sebelum angka), sekitar
//! em-dash, dan putus wajib setelah `\n`. Tidak ada putus di dalam kata lain (mis. `user(s)`,
//! `1.5`, `a@b.c`). Kelas CJK/Thai dsb. tidak ditiru.

/// Posisi putus: `(indeks karakter sesudah putus, wajib?)`. Posisi terakhir = panjang teks
/// (tidak wajib), seperti `LineBreaker.nextBreak` di akhir string. Teks kosong → tidak ada.
pub fn breaks(chars: &[char]) -> Vec<(usize, bool)> {
    let n = chars.len();
    let mut out = Vec::new();
    for p in 1..n {
        let prev = chars.get(p - 1).copied().unwrap_or(' ');
        let next = chars.get(p).copied().unwrap_or(' ');
        if prev == '\n' {
            out.push((p, true));
            continue;
        }
        if allowed(chars, p, prev, next) {
            out.push((p, false));
        }
    }
    if n > 0 {
        if chars.last() == Some(&'\n') {
            // Putus wajib tepat di akhir: sudah dicatat bila n > 1; untuk n == 1 ("\n") juga.
            if out.last().is_none_or(|(pos, _)| *pos != n) {
                out.push((n, true));
            }
        } else {
            out.push((n, false));
        }
    }
    out
}

/// LB13: tidak ada putus sebelum penutup / tanda baca.
fn no_break_before(c: char) -> bool {
    matches!(
        c,
        ')' | ']' | '}' | '!' | '?' | ',' | '.' | ':' | ';' | '/' | '\u{2026}'
    )
}

fn is_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn allowed(chars: &[char], p: usize, prev: char, next: char) -> bool {
    // LB7: tidak ada putus sebelum spasi.
    if is_space(next) {
        return false;
    }
    if is_space(prev) {
        // LB13 menang atas LB18 (putus setelah spasi).
        if no_break_before(next) {
            return false;
        }
        // LB14: tidak ada putus setelah "(" + spasi.
        let mut q = p;
        while q > 0 && chars.get(q - 1).is_some_and(|c| is_space(*c)) {
            q -= 1;
        }
        let before = q.checked_sub(1).and_then(|i| chars.get(i)).copied();
        return !matches!(before, Some('(' | '[' | '{'));
    }
    if no_break_before(next) && next != '/' {
        return false;
    }
    match prev {
        '-' => {
            // LB21 (× HY) + LB25 (HY × NU) + LB20a (tanda hubung di awal kata).
            let before = p.checked_sub(2).and_then(|i| chars.get(i)).copied();
            let word_start = before.is_none_or(|c| is_space(c) || c == '\n');
            !word_start && !next.is_ascii_digit() && next != '-' && !no_break_before(next)
        }
        '/' => !next.is_ascii_digit() && !no_break_before(next) && next != '/',
        // EX (`?`, `!`): putus sesudahnya diizinkan (LB13 hanya melarang sebelumnya); BA (`|`, en dash).
        '?' | '!' | '|' | '\u{2013}' => !no_break_before(next),
        '\u{2014}' => true,
        _ => next == '\u{2014}',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Vec<(usize, bool)> {
        breaks(&s.chars().collect::<Vec<_>>())
    }

    #[test]
    fn spaces_and_end() {
        assert_eq!(b("ab cd"), vec![(3, false), (5, false)]);
        assert_eq!(b(""), Vec::<(usize, bool)>::new());
        assert_eq!(b("a"), vec![(1, false)]);
    }

    #[test]
    fn hyphen_and_slash() {
        // "well-known": putus setelah "-"; "$1.5/user": setelah "/" sebelum huruf.
        assert_eq!(b("well-known"), vec![(5, false), (10, false)]);
        assert_eq!(b("$1.5/user"), vec![(5, false), (9, false)]);
        // Tidak putus sebelum angka.
        assert_eq!(b("A-5"), vec![(3, false)]);
        assert_eq!(b("a/5"), vec![(3, false)]);
        // " - " berdiri sendiri: hanya putus setelah spasi.
        assert_eq!(b("a - b"), vec![(2, false), (4, false), (5, false)]);
        // URL: putus sesudah "?" dan sesudah "-" yang diikuti huruf.
        assert_eq!(
            b("x?token=a-1&b=9-abc"),
            vec![(2, false), (16, false), (19, false)]
        );
    }

    #[test]
    fn newline_required() {
        assert_eq!(b("a\nb"), vec![(2, true), (3, false)]);
        assert_eq!(b("a\n"), vec![(2, true)]);
    }

    #[test]
    fn no_break_in_parens_and_decimals() {
        assert_eq!(b("user(s) @ x"), vec![(8, false), (10, false), (11, false)]);
    }
}
