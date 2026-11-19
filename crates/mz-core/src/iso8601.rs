//! `isISO8601` validator.js 13.15 (non-strict), dipakai `@IsISO8601()` dan `@IsDateString()`
//! class-validator 0.14. Port setia regex `iso8601` (`validator/lib/isISO8601.js:11`):
//!
//! ```text
//! ^([\+-]?\d{4}(?!\d{2}\b))((-?)((0[1-9]|1[0-2])(\3([12]\d|0[1-9]|3[01]))?|W([0-4]\d|5[0-3])(-?[1-7])?
//! |(00[1-9]|0[1-9]\d|[12]\d{2}|3([0-5]\d|6[1-6])))([T\s]((([01]\d|2[0-3])((:?)[0-5]\d)?|24:?00)
//! ([\.,]\d+(?!:))?)?(\17[0-5]\d([\.,]\d+)?)?([zZ]|([\+-])([01]\d|2[0-3]):?([0-5]\d)?)?)?)?$
//! ```
//!
//! Karena `RegExp.test` hanya menanyakan ADA-TIDAKNYA kecocokan, urutan backtracking tidak
//! penting: semua cabang dijelajahi sebagai himpunan state `(posisi, isi grup backref)`.
//! Semantik JS yang ditiru: `\d` = ASCII, `\s` = himpunan whitespace ECMAScript, `\b` = batas
//! `[A-Za-z0-9_]`, backref ke grup yang tidak ikut cocok (`\17`) = string kosong.
//! Opsi `strict` (cek tanggal kalender): [`is_iso8601_strict`] / [`strict_calendar`] (dipakai
//! mz-admin-jobs, mz-leave-holidays, mz-leaves). `mz-admin-misc` (audit-logs) dan `mz-admin-company`
//! punya varian sendiri yang perilakunya BERBEDA (tahun < 100, hanya `YYYY-MM-DD`) — sengaja tidak
//! disatukan.

use chrono::{Duration, NaiveDate, NaiveDateTime};

/// `validator.isISO8601(s)` (non-strict).
pub fn is_iso8601(s: &str) -> bool {
    let c: Vec<char> = s.chars().collect();
    Matcher { c: &c }.test()
}

struct Matcher<'a> {
    c: &'a [char],
}

/// Isi `(:?)` grup 17: `None` = grup tidak ikut cocok (backref cocok dengan string kosong).
type Colon = Option<bool>;

impl Matcher<'_> {
    fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }

    fn digit(&self, i: usize) -> Option<u32> {
        self.at(i).filter(char::is_ascii_digit)?.to_digit(10)
    }

    /// Dua digit `ab` dengan `a` dalam `first` lalu `b` dalam `second`.
    fn two(&self, i: usize, ok: impl Fn(u32, u32) -> bool) -> bool {
        matches!((self.digit(i), self.digit(i + 1)), (Some(a), Some(b)) if ok(a, b))
    }

    fn is(&self, i: usize, set: &[char]) -> bool {
        self.at(i).is_some_and(|ch| set.contains(&ch))
    }

    fn end(&self, i: usize) -> bool {
        i == self.c.len()
    }

    fn test(&self) -> bool {
        // `[\+-]?\d{4}`: tanda wajib dikonsumsi bila ada (\d tidak cocok dengan tanda).
        let start = usize::from(self.is(0, &['+', '-']));
        if !(start..start + 4).all(|i| self.digit(i).is_some()) {
            return false;
        }
        let p = start + 4;
        // `(?!\d{2}\b)`
        let word = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
        if self.digit(p).is_some()
            && self.digit(p + 1).is_some()
            && self.at(p + 2).is_none_or(|ch| !word(ch))
        {
            return false;
        }
        // Seluruh sisanya opsional.
        self.end(p) || self.rest(p)
    }

    /// `(-?)(tanggal)(waktu)?` lalu `$`.
    fn rest(&self, p: usize) -> bool {
        let dashes: &[bool] = if self.is(p, &['-']) {
            &[true, false]
        } else {
            &[false]
        };
        dashes.iter().any(|&dash| {
            let q = p + usize::from(dash);
            self.date_ends(q, dash)
                .into_iter()
                .any(|e| self.end(e) || self.time(e))
        })
    }

    /// Semua posisi akhir alternatif tanggal mulai `q`.
    fn date_ends(&self, q: usize, dash: bool) -> Vec<usize> {
        let mut ends = Vec::new();
        // `(0[1-9]|1[0-2])(\3([12]\d|0[1-9]|3[01]))?`
        if self.two(q, |a, b| (a == 0 && b >= 1) || (a == 1 && b <= 2)) {
            ends.push(q + 2);
            let d = q + 2 + usize::from(dash);
            if (!dash || self.is(q + 2, &['-']))
                && self.two(d, |a, b| {
                    a == 1 || a == 2 || (a == 0 && b >= 1) || (a == 3 && b <= 1)
                })
            {
                ends.push(d + 2);
            }
        }
        // `W([0-4]\d|5[0-3])(-?[1-7])?`
        if self.is(q, &['W']) && self.two(q + 1, |a, b| a <= 4 || (a == 5 && b <= 3)) {
            let w = q + 3;
            ends.push(w);
            let weekday = |i: usize| self.digit(i).is_some_and(|d| (1..=7).contains(&d));
            if weekday(w) {
                ends.push(w + 1);
            }
            if self.is(w, &['-']) && weekday(w + 1) {
                ends.push(w + 2);
            }
        }
        // `(00[1-9]|0[1-9]\d|[12]\d{2}|3([0-5]\d|6[1-6]))`
        if let (Some(a), Some(b), Some(c)) = (self.digit(q), self.digit(q + 1), self.digit(q + 2)) {
            let ordinal = match a {
                0 => b >= 1 || c >= 1,
                1 | 2 => true,
                3 => b <= 5 || (b == 6 && (1..=6).contains(&c)),
                _ => false,
            };
            if ordinal {
                ends.push(q + 3);
            }
        }
        ends
    }

    /// `[T\s](jam pecahan?)?(\17 detik pecahan?)?(zona)?` lalu `$`.
    fn time(&self, p: usize) -> bool {
        if !self.at(p).is_some_and(|ch| ch == 'T' || js_space(ch)) {
            return false;
        }
        let p = p + 1;
        // Grup 13 opsional: (posisi akhir, isi grup 17).
        let mut clocks: Vec<(usize, Colon)> = vec![(p, None)];
        for (e, colon) in self.clock_ends(p) {
            clocks.push((e, colon));
            clocks.extend(self.fractions(e, true).into_iter().map(|f| (f, colon)));
        }
        clocks.into_iter().any(|(e, colon)| {
            let mut secs = vec![e];
            // `\17[0-5]\d([\.,]\d+)?`
            let s = e + usize::from(colon == Some(true));
            if (colon != Some(true) || self.is(e, &[':'])) && self.two(s, |a, _| a <= 5) {
                secs.push(s + 2);
                secs.extend(self.fractions(s + 2, false));
            }
            secs.into_iter().any(|e| self.zone_then_end(e))
        })
    }

    /// `([01]\d|2[0-3])((:?)[0-5]\d)?|24:?00`
    fn clock_ends(&self, p: usize) -> Vec<(usize, Colon)> {
        let mut ends = Vec::new();
        if self.two(p, |a, b| a <= 1 || (a == 2 && b <= 3)) {
            ends.push((p + 2, None));
            for colon in [true, false] {
                let m = p + 2 + usize::from(colon);
                if (!colon || self.is(p + 2, &[':'])) && self.two(m, |a, _| a <= 5) {
                    ends.push((m + 2, Some(colon)));
                }
            }
        }
        if self.two(p, |a, b| a == 2 && b == 4) {
            for colon in [true, false] {
                let z = p + 2 + usize::from(colon);
                if (!colon || self.is(p + 2, &[':'])) && self.two(z, |a, b| a == 0 && b == 0) {
                    ends.push((z + 2, None));
                }
            }
        }
        ends
    }

    /// `[\.,]\d+` (semua panjang), dengan `(?!:)` sesudahnya bila `no_colon`.
    fn fractions(&self, p: usize, no_colon: bool) -> Vec<usize> {
        if !self.is(p, &['.', ',']) {
            return Vec::new();
        }
        let n = (p + 1..).take_while(|&i| self.digit(i).is_some()).count();
        (p + 2..=p + 1 + n)
            .filter(|&e| !(no_colon && self.is(e, &[':'])))
            .collect()
    }

    /// `([zZ]|([\+-])([01]\d|2[0-3]):?([0-5]\d)?)?$`
    fn zone_then_end(&self, p: usize) -> bool {
        if self.end(p) {
            return true;
        }
        if self.is(p, &['z', 'Z']) {
            return self.end(p + 1);
        }
        if !(self.is(p, &['+', '-']) && self.two(p + 1, |a, b| a <= 1 || (a == 2 && b <= 3))) {
            return false;
        }
        let h = p + 3;
        [h, h + usize::from(self.is(h, &[':']))]
            .into_iter()
            .any(|m| self.end(m) || (self.two(m, |a, _| a <= 5) && self.end(m + 2)))
    }
}

/// `\s` ECMAScript: WhiteSpace + LineTerminator.
fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `@IsISO8601()`/`@IsDateString()` + `new Date(value)` (argumen tanggal ke Prisma).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateArg {
    /// Lolos validator dan bisa dibaca `new Date(...)` (UTC; server prod ber-TZ UTC).
    Ok(NaiveDateTime),
    /// Lolos validator tetapi `new Date(...)` Invalid Date (Node: galat Prisma/`RangeError`).
    Unreadable,
}

/// `None` = gagal [`is_iso8601`] (400 ValidationPipe). Nilai tanggal dibaca dengan pendekatan
/// `new Date` untuk bentuk ISO yang dikirim klien; parser legacy V8 yang longgar TIDAK diport,
/// sehingga beberapa bentuk aneh yang terbaca V8 (`+2026`, `2026-001`, pemisah tab/`\u3000`, spasi
/// di akhir) menjadi `Unreadable`, dan beberapa yang Invalid Date di V8 (`T08`, `T0800`, `+07`,
/// pecahan koma) terbaca di sini. Lihat test `date_arg_vs_node`.
pub fn parse_date_arg(s: &str) -> Option<DateArg> {
    if !is_iso8601(s) {
        return None;
    }
    Some(js_date_value(s).unwrap_or(DateArg::Unreadable))
}

fn digits(s: &str, n: usize) -> Option<(i64, &str)> {
    let head = s.get(..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, s.get(n..)?))
}

fn js_date_value(s: &str) -> Option<DateArg> {
    // Tahun (+/- opsional), lalu `-MM(-DD)?` atau `MM(DD)?`.
    let (signed, rest) = match s.strip_prefix(['+', '-']) {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (year, mut rest) = digits(rest, 4)?;
    // `new Date` hanya membaca bentuk ISO bertanda hubung (atau `YYYY` saja).
    let mut readable = !signed;
    let (mut month, mut day) = (1_i64, 1_i64);
    if !rest.is_empty() && !rest.starts_with(['T', ' ']) {
        let sep = rest.starts_with('-');
        let body = if sep { rest.get(1..)? } else { rest };
        let (m, after) = digits(body, 2)?;
        if !(1..=12).contains(&m) {
            return None;
        }
        month = m;
        rest = after;
        readable &= sep;
        if !rest.is_empty() && !rest.starts_with(['T', ' ']) {
            let body = if sep { rest.strip_prefix('-')? } else { rest };
            let (d, after) = digits(body, 2)?;
            if !(1..=31).contains(&d) {
                return None;
            }
            day = d;
            rest = after;
        }
    }
    let date = if readable {
        NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, u32::try_from(month).ok()?, 1)
            .map(|d| d + Duration::days(day - 1))
    } else {
        None
    };
    // Spasi di akhir diabaikan V8 (`"2026-11-16 "` = tengah malam).
    if rest.chars().all(js_space) {
        let value = date.and_then(|d| d.and_hms_opt(0, 0, 0));
        return Some(value.map_or(DateArg::Unreadable, DateArg::Ok));
    }

    // Waktu V8: `HH:mm[:ss[.fff]]` (menit wajib dengan `:`; bentuk padat `T0800` = Invalid
    // Date). Pemisah `T` atau whitespace JS apa pun.
    let (hour, mut t) = digits(rest.strip_prefix(|c: char| c == 'T' || js_space(c))?, 2)?;
    if hour > 24 {
        return None;
    }
    let (mut second, mut millis) = (0_i64, 0_i64);
    let (minute, after) = digits(t.strip_prefix(':')?, 2)?;
    t = after;
    let mut has_seconds = false;
    if let Some(after) = t.strip_prefix(':') {
        let (sec, after) = digits(after, 2)?;
        second = sec;
        t = after;
        has_seconds = true;
    }
    if minute > 59 || second > 59 {
        return None;
    }
    // Pecahan hanya sesudah detik dan hanya `.` (`,5`/`T08:30.5` = Invalid Date).
    if let Some(frac) = t.strip_prefix('.').filter(|_| has_seconds) {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let head: String = frac.chars().take(n.min(3)).collect();
        millis = format!("{head:0<3}").parse::<i64>().ok()?;
        t = frac.get(n..)?;
    }
    if hour == 24 && (minute != 0 || second != 0 || millis != 0) {
        return None;
    }

    // Zona: `Z`, `+HH:mm`, `+HHmm` (`+HH` saja = Invalid Date).
    let mut offset_minutes = 0_i64;
    if !t.is_empty() && t != "Z" && t != "z" {
        let sign = match t.chars().next()? {
            '+' => 1,
            '-' => -1,
            _ => return None,
        };
        let (oh, z) = digits(t.get(1..)?, 2)?;
        if oh > 23 {
            return None;
        }
        let z = z.strip_prefix(':').unwrap_or(z);
        let (om, z) = digits(z, 2)?;
        if om > 59 || !z.is_empty() {
            return None;
        }
        offset_minutes = sign * (oh * 60 + om);
    }

    let naive = date.and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| {
        d + Duration::hours(hour)
            + Duration::minutes(minute)
            + Duration::seconds(second)
            + Duration::milliseconds(millis)
            - Duration::minutes(offset_minutes)
    });
    Some(naive.map_or(DateArg::Unreadable, DateArg::Ok))
}

/// validator.js `isISO8601(str, { strict: true })`: format ISO + tanggal kalender sah.
pub fn is_iso8601_strict(s: &str) -> bool {
    is_iso8601(s) && strict_calendar(s)
}

/// validator.js `isISO8601(str, { strict: true })` → `isValidDate`: tanggal kalender harus ada
/// (`2026-02-30` ditolak), tanggal ordinal `YYYY-DDD` ≤ 365/366. Anggap `s` sudah lolos
/// [`is_iso8601`].
pub fn strict_calendar(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| -> Option<u32> {
        let part = s.get(from..from + n)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let ordinal = |sep: usize| -> Option<bool> {
        let year = digits(0, 4)?;
        let day = digits(4 + sep, 3)?;
        let rest = s.get(7 + sep..)?;
        if !(rest.is_empty() || rest.starts_with('T') || rest.starts_with(' ')) {
            return None;
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        Some(day <= if leap { 366 } else { 365 })
    };
    if b.get(4) == Some(&b'-')
        && let Some(ok) = ordinal(1)
    {
        return ok;
    }
    if let Some(ok) = ordinal(0) {
        return ok;
    }
    let (y, m, d) = if b.get(4) == Some(&b'-') {
        (digits(0, 4), digits(5, 2), digits(8, 2))
    } else {
        (digits(0, 4), digits(4, 2), digits(6, 2))
    };
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) if m > 0 && d > 0 => {
            i32::try_from(y).is_ok_and(|y| chrono::NaiveDate::from_ymd_opt(y, m, d).is_some())
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::is_iso8601;

    /// Keluaran `validator.isISO8601` 13.15.35 dari `node_modules` Mudah-API (2026-11-16).
    const NODE: &[(&str, bool)] = &[
        ("2026", true),
        ("+2026", true),
        ("-2026", true),
        ("2026-", false),
        ("202611", false),
        ("2026-11", true),
        ("20261116", true),
        ("2026-11-16", true),
        ("2026-1116", false),
        ("202611-16", false),
        ("2026-02-31", true),
        ("2026-13-01", false),
        ("2026-00-10", false),
        ("2026-11-32", false),
        ("2026-W01", true),
        ("2026W01", true),
        ("2026-W53", true),
        ("2026-W54", false),
        ("2026-W01-1", true),
        ("2026W011", true),
        ("2026-W01-8", false),
        ("2026-001", true),
        ("2026001", true),
        ("2026-366", true),
        ("2026-367", false),
        ("2026-000", false),
        ("2026-11-16T", true),
        ("2026-11-16 ", true),
        ("2026-11-16T08", true),
        ("2026-11-16T0800", true),
        ("2026-11-16T08:00", true),
        ("2026-11-16T08:0", false),
        ("2026-11-16T08:00:00", true),
        ("2026-11-16T080000", true),
        ("2026-11-16T08:0000", false),
        ("2026-11-16T0800:00", false),
        ("2026-11-16T08:00:00.123Z", true),
        ("2026-11-16T08:00:00,5Z", true),
        ("2026-11-16T08:00:00.Z", false),
        ("2026-11-16T08.5", true),
        ("2026-11-16T08:30.5", true),
        ("2026-11-16T08:30.5:00", false),
        ("2026-11-16T08:30.12:00", false),
        ("2026-11-16T24:00", true),
        ("2026-11-16T2400", true),
        ("2026-11-16T24:00:00", false),
        ("2026-11-16T24:01", false),
        ("2026-11-16T25:00", false),
        ("2026-11-16T23:60", false),
        ("2026-11-16T08:00Z", true),
        ("2026-11-16T08:00z", true),
        ("2026-11-16T08:00+07", true),
        ("2026-11-16T08:00+0700", true),
        ("2026-11-16T08:00+07:00", true),
        ("2026-11-16T08:00-07:30", true),
        ("2026-11-16T08:00+24:00", false),
        ("2026-11-16T08:00+07:60", false),
        ("2026-11-16T08:00+7", false),
        ("2026-11-16TZ", true),
        ("2026-11-16T+07:00", true),
        ("2026-11-16\t08:00", true),
        ("2026-11-16\u{a0}08:00", true),
        ("2026-11-16\u{3000}08:00", true),
        ("2026-11-16\u{feff}08:00", true),
        ("2026-11-16\u{85}08:00", false),
        ("2026-11-16\n08:00", true),
        ("202611T10", true),
        ("202611 10", false),
        ("2026-11-16T08:00:00.123456789+07:00", true),
        ("2026-11-16T08:00:00Z ", false),
        ("x2026", false),
        ("2026-11-16x", false),
        ("", false),
        ("+20261116", true),
        ("2026-11-16T08:00:60", false),
        ("20261116T080000Z", true),
        ("2026-11T08:00", true),
        ("2026T08", false),
        ("2026-11-16T08:00:00.5.5", false),
        ("2026-11-16t08:00", false),
        ("2026-11-16T8:00", false),
        ("2026-1-16", false),
        ("\u{661}\u{662}\u{663}\u{664}-11-16", false),
        ("2026-11-16T08:00:00+07:00:00", false),
    ];

    #[test]
    fn matches_validator_js() {
        for &(input, want) in NODE {
            assert_eq!(is_iso8601(input), want, "input {input:?}");
        }
    }

    /// `new Date(s)` Node (TZ=UTC) untuk input yang lolos `isISO8601`; `U` = Invalid Date.
    const NODE_DATES: &[(&str, &str)] = &[
        ("2026", "2026-01-01T00:00:00.000Z"),
        ("+2026", "2026-01-01T00:00:00.000Z"),
        ("-2026", "2026-01-01T00:00:00.000Z"),
        ("2026-11", "2026-11-01T00:00:00.000Z"),
        ("20261116", "U"),
        ("2026-11-16", "2026-11-16T00:00:00.000Z"),
        ("2026-02-31", "2026-03-03T00:00:00.000Z"),
        ("2026-W01", "U"),
        ("2026W01", "U"),
        ("2026-W53", "U"),
        ("2026-W01-1", "U"),
        ("2026W011", "U"),
        ("2026-001", "2026-01-01T00:00:00.000Z"),
        ("2026001", "U"),
        ("2026-366", "U"),
        ("2026-11-16T", "U"),
        ("2026-11-16 ", "2026-11-16T00:00:00.000Z"),
        ("2026-11-16T08", "U"),
        ("2026-11-16T0800", "U"),
        ("2026-11-16T08:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16T08:00:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16T080000", "U"),
        ("2026-11-16T08:00:00.123Z", "2026-11-16T08:00:00.123Z"),
        ("2026-11-16T08:00:00,5Z", "U"),
        ("2026-11-16T08.5", "U"),
        ("2026-11-16T08:30.5", "U"),
        ("2026-11-16T24:00", "2026-11-17T00:00:00.000Z"),
        ("2026-11-16T2400", "U"),
        ("2026-11-16T08:00Z", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16T08:00z", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16T08:00+07", "U"),
        ("2026-11-16T08:00+0700", "2026-11-16T01:00:00.000Z"),
        ("2026-11-16T08:00+07:00", "2026-11-16T01:00:00.000Z"),
        ("2026-11-16T08:00-07:30", "2026-11-16T15:30:00.000Z"),
        ("2026-11-16TZ", "U"),
        ("2026-11-16T+07:00", "U"),
        ("2026-11-16\u{9}08:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16\u{a0}08:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16\u{3000}08:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16\u{feff}08:00", "2026-11-16T08:00:00.000Z"),
        ("2026-11-16\u{a}08:00", "2026-11-16T08:00:00.000Z"),
        ("202611T10", "U"),
        (
            "2026-11-16T08:00:00.123456789+07:00",
            "2026-11-16T01:00:00.123Z",
        ),
        ("+20261116", "U"),
        ("20261116T080000Z", "U"),
        ("2026-11T08:00", "2026-11-01T08:00:00.000Z"),
    ];

    /// Selisih yang DIKETAHUI (parser legacy V8 tidak diport; lihat `parse_date_arg`).
    const KNOWN_DIFF: &[&str] = &["+2026", "-2026", "2026-001"];

    #[test]
    fn date_arg_vs_node() {
        let mut diffs = Vec::new();
        for &(input, want) in NODE_DATES {
            let got = match super::parse_date_arg(input) {
                Some(super::DateArg::Ok(d)) => d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                Some(super::DateArg::Unreadable) => "U".to_owned(),
                None => "INVALID".to_owned(),
            };
            if got != want {
                diffs.push((input, format!("{input:?} node={want} rust={got}")));
            }
        }
        let unknown: Vec<&String> = diffs
            .iter()
            .filter(|(i, _)| !KNOWN_DIFF.contains(i))
            .map(|(_, m)| m)
            .collect();
        assert!(unknown.is_empty(), "selisih baru: {unknown:#?}");
        // Selisih yang sudah diperbaiki harus dikeluarkan dari KNOWN_DIFF.
        assert_eq!(diffs.len(), KNOWN_DIFF.len(), "{diffs:#?}");
    }
}
