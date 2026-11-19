//! Tanggal dengan serialisasi persis `Date.prototype.toJSON()`:
//! `2026-11-16T08:00:00.000Z` (selalu 3 digit milidetik, selalu `Z`).
//!
//! Kolom `DateTime` Prisma di Postgres bertipe `timestamp(3)` TANPA zona waktu,
//! dan Prisma memperlakukan isinya sebagai UTC. Karena itu tipe dasarnya
//! `NaiveDateTime` (UTC), bukan `DateTime<Utc>`.

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct JsDate(pub NaiveDateTime);

impl JsDate {
    pub fn now() -> Self {
        Self(now_naive())
    }
    pub fn utc(&self) -> DateTime<Utc> {
        Utc.from_utc_datetime(&self.0)
    }
    pub fn timestamp_millis(&self) -> i64 {
        self.0.and_utc().timestamp_millis()
    }
    pub fn from_millis(ms: i64) -> Option<Self> {
        DateTime::<Utc>::from_timestamp_millis(ms).map(|d| Self(d.naive_utc()))
    }
    pub fn to_js_string(&self) -> String {
        to_js_iso(&self.utc())
    }
}

/// `new Date()` dibulatkan ke milidetik, sama dengan presisi `timestamp(3)`.
pub fn now_naive() -> NaiveDateTime {
    let now = Utc::now();
    DateTime::<Utc>::from_timestamp_millis(now.timestamp_millis())
        .unwrap_or(now)
        .naive_utc()
}

pub fn to_js_iso(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parser longgar setara `new Date(string)` untuk input ISO dari klien:
/// `2026-11-16`, `2026-11-16T08:00:00`, `...Z`, `...+07:00`.
pub fn parse_js_date(raw: &str) -> Option<NaiveDateTime> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc).naive_utc());
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(raw, fmt) {
            // `new Date('2026-11-16T08:00:00')` tanpa zona = waktu LOKAL proses
            // Node. Server produksi berjalan dengan TZ=UTC, jadi sama dengan UTC.
            return Some(dt);
        }
    }
    // `new Date('2026-11-16')` (date-only) = tengah malam UTC.
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// Hari sejak 1970-01-01 untuk tanggal proleptik Gregorian (Hinnant `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Kursor kecil untuk parser tanggal.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek().is_some_and(|x| x.eq_ignore_ascii_case(&c)) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    /// Tepat `n` digit.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let s = self.b.get(self.i..self.i + n)?;
        if !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i += n;
        s.iter().try_fold(0i64, |a, d| {
            a.checked_mul(10)?.checked_add(i64::from(d - b'0'))
        })
    }
    /// 1..=`max` digit.
    fn digits_upto(&mut self, max: usize) -> Option<(i64, usize)> {
        let start = self.i;
        while self.i - start < max && self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let n = self.i - start;
        if n == 0 {
            return None;
        }
        let v = self.b.get(start..self.i)?.iter().try_fold(0i64, |a, d| {
            a.checked_mul(10)?.checked_add(i64::from(d - b'0'))
        })?;
        Some((v, n))
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
}

struct Parts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
    /// Offset zona dalam menit; `None` = tanpa zona.
    offset: Option<i64>,
}

/// `HH:MM[:SS[.f+]]` lalu zona opsional (`Z`, `±HH:MM`, `±HHMM`).
fn parse_time(c: &mut Cur<'_>, p: &mut Parts, hour_width_flexible: bool) -> Option<()> {
    p.hour = if hour_width_flexible {
        c.digits_upto(2)?.0
    } else {
        c.digits(2)?
    };
    if !c.eat(b':') {
        return None;
    }
    p.minute = c.digits(2)?;
    if c.eat(b':') {
        p.second = c.digits(2)?;
        if c.eat(b'.') {
            let (v, n) = c.digits_upto(usize::MAX)?;
            let _ = v;
            // Hanya 3 digit pertama yang dipakai (dipotong, seperti V8).
            let start = c.i - n;
            let first: i64 =
                c.b.get(start..start + n.min(3))?
                    .iter()
                    .fold(0, |a, d| a * 10 + i64::from(d - b'0'));
            p.millis = first * 10i64.pow(3 - n.min(3) as u32);
        }
    }
    if c.eat(b'Z') {
        p.offset = Some(0);
    } else if let Some(sign) = c.peek().filter(|&x| x == b'+' || x == b'-') {
        c.i += 1;
        let h = c.digits(2)?;
        c.eat(b':');
        let m = c.digits(2)?;
        if h > 23 || m > 59 {
            return None;
        }
        let total = h * 60 + m;
        p.offset = Some(if sign == b'-' { -total } else { total });
    }
    Some(())
}

/// Epoch ms dari bagian tanggal; validasi rentang ala V8 (hari 1..31 untuk semua bulan dan
/// melimpah, jam 24 hanya `24:00:00.000`).
fn compose(p: &Parts, date_only_utc: bool) -> Option<i64> {
    if !(1..=12).contains(&p.month)
        || !(1..=31).contains(&p.day)
        || !(0..=24).contains(&p.hour)
        || !(0..=59).contains(&p.minute)
        || !(0..=59).contains(&p.second)
        || (p.hour == 24 && (p.minute != 0 || p.second != 0 || p.millis != 0))
    {
        return None;
    }
    let days = days_from_civil(p.year, p.month, 1) + p.day - 1;
    let mut ms =
        days * 86_400_000 + p.hour * 3_600_000 + p.minute * 60_000 + p.second * 1000 + p.millis;
    // Tanpa zona: date-only = UTC; dengan jam = waktu lokal proses (produksi TZ=UTC).
    let _ = date_only_utc;
    if let Some(off) = p.offset {
        ms -= off * 60_000;
    }
    (ms.abs() <= 8_640_000_000_000_000).then_some(ms)
}

/// Format ISO ES (`YYYY`, `±YYYYYY`, `-MM`, `-DD`, `THH:MM[:SS[.f]]`, zona).
fn parse_es_iso(s: &str) -> Option<i64> {
    let mut c = Cur {
        b: s.as_bytes(),
        i: 0,
    };
    let mut p = Parts {
        year: 0,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
        offset: None,
    };
    p.year = match c.peek() {
        Some(sign @ (b'+' | b'-')) => {
            c.i += 1;
            let y = c.digits(6)?;
            if sign == b'-' {
                if y == 0 {
                    return None;
                }
                -y
            } else {
                y
            }
        }
        _ => c.digits(4)?,
    };
    if c.eat(b'-') {
        p.month = c.digits(2)?;
        if c.eat(b'-') {
            p.day = c.digits(2)?;
        }
    }
    let mut date_only = true;
    if c.eat(b'T') {
        date_only = false;
        parse_time(&mut c, &mut p, false)?;
    }
    if !c.done() {
        return None;
    }
    compose(&p, date_only)
}

/// Subset parser "legacy" V8 untuk bentuk `Y-M-D[( |T)H:MM[:SS[.f]]][Z|±HH:MM]` (tahun ≥ 3 digit,
/// bulan/hari 1–2 digit), yang dipakai bila format ISO gagal: `2026-3-1`, `20260-01-01`,
/// `2026-03-01 10:00`.
fn parse_legacy(s: &str) -> Option<i64> {
    let mut c = Cur {
        b: s.as_bytes(),
        i: 0,
    };
    let (year, n) = c.digits_upto(usize::MAX)?;
    if n < 3 || !c.eat(b'-') {
        return None;
    }
    let mut p = Parts {
        year,
        month: c.digits_upto(2)?.0,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
        offset: None,
    };
    if !c.eat(b'-') {
        return None;
    }
    p.day = c.digits_upto(2)?.0;
    if c.eat(b' ') || c.eat(b'T') {
        while c.eat(b' ') {}
        parse_time(&mut c, &mut p, true)?;
    }
    if !c.done() {
        return None;
    }
    compose(&p, false)
}

/// `Date.parse(raw)` V8 (ms epoch; `None` = NaN) untuk bentuk tanggal numerik: format ISO ES
/// lengkap (hari 29–31 melimpah ke bulan berikut, `T24:00`, `HH:MM` dengan zona, tahun `±YYYYYY`)
/// plus subset fallback legacy (lihat [`parse_legacy`]). Tanpa zona = UTC (produksi berjalan
/// dengan TZ=UTC). Bentuk teks (`Mar 1 2026`) tidak didukung → `None`.
///
/// Sengaja TERPISAH dari [`parse_js_date`] (40+ pemanggil dengan paritas yang sudah dikunci).
pub fn v8_date_parse(raw: &str) -> Option<i64> {
    let s = crate::jsfmt::trim(raw);
    parse_es_iso(s).or_else(|| parse_legacy(s))
}

/// [`v8_date_parse`] sebagai `NaiveDateTime` UTC (`None` juga bila di luar rentang chrono).
pub fn v8_date(raw: &str) -> Option<NaiveDateTime> {
    v8_date_parse(raw)
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|d| d.naive_utc())
}

impl Serialize for JsDate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_js_string())
    }
}

impl<'de> Deserialize<'de> for JsDate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        parse_js_date(&raw)
            .map(JsDate)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid date: {raw}")))
    }
}

impl From<NaiveDateTime> for JsDate {
    fn from(v: NaiveDateTime) -> Self {
        Self(v)
    }
}

/// `timelineDurationSeconds`: kolom UTC diutamakan; `floor(ms / 1000)`, minimal 0. Rumah bersama
/// (dulu salinan di mz-screenshots dan mz-divisions).
pub fn timeline_duration_seconds(
    start_at: JsDate,
    end_at: JsDate,
    start_at_utc: Option<JsDate>,
    end_at_utc: Option<JsDate>,
) -> i64 {
    let start = start_at_utc.unwrap_or(start_at);
    let end = end_at_utc.unwrap_or(end_at);
    (end.timestamp_millis() - start.timestamp_millis())
        .div_euclid(1000)
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_like_js() {
        let dt = parse_js_date("2026-11-16T08:00:00Z").map(JsDate);
        let json = serde_json::to_string(&dt).unwrap_or_default();
        assert_eq!(json, "\"2026-11-16T08:00:00.000Z\"");
    }

    #[test]
    fn offset_is_normalised_to_utc() {
        let dt = parse_js_date("2026-11-16T08:00:00+07:00").map(JsDate);
        assert_eq!(
            dt.map(|d| d.to_js_string()).as_deref(),
            Some("2026-11-16T01:00:00.000Z")
        );
    }

    #[test]
    fn date_only_is_utc_midnight() {
        let dt = parse_js_date("2026-11-16").map(JsDate);
        assert_eq!(
            dt.map(|d| d.to_js_string()).as_deref(),
            Some("2026-11-16T00:00:00.000Z")
        );
    }
    #[test]
    fn v8_date_parse_matches_node() {
        let iso = |s: &str| {
            v8_date_parse(s)
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .map(|d| to_js_iso(&d))
        };
        let cases: &[(&str, Option<&str>)] = &[
            ("2026-02-30", Some("2026-03-02T00:00:00.000Z")),
            ("2026-04-31", Some("2026-05-01T00:00:00.000Z")),
            ("2026-02-32", None),
            ("2026-13-01", None),
            ("2026-00-10", None),
            ("2026-02-00", None),
            ("2026-03-01T24:00:00Z", Some("2026-03-02T00:00:00.000Z")),
            ("2026-03-01T24:01", None),
            ("2026-03-01T25:00", None),
            ("2026-03-01T23:60", None),
            ("2026-03-01T03:46Z", Some("2026-03-01T03:46:00.000Z")),
            ("2026-03-01T03:46+07:00", Some("2026-02-28T20:46:00.000Z")),
            ("2026-03-01T10:00+0700", Some("2026-03-01T03:00:00.000Z")),
            (
                "2026-03-01T10:00:00.123456Z",
                Some("2026-03-01T10:00:00.123Z"),
            ),
            ("2026-03-01T03:46:00.5Z", Some("2026-03-01T03:46:00.500Z")),
            ("2026-03-01t10:00z", Some("2026-03-01T10:00:00.000Z")),
            ("2026-03-01T10", None),
            ("2026-03-01T10:00+07", None),
            ("2026-03-01T", None),
            ("2026-03-01T10:00:00,5Z", None),
            ("2026", Some("2026-01-01T00:00:00.000Z")),
            ("2026-02", Some("2026-02-01T00:00:00.000Z")),
            ("+002026-02-30", Some("2026-03-02T00:00:00.000Z")),
            (" 2026-03-01", Some("2026-03-01T00:00:00.000Z")),
            ("2026-2-30", Some("2026-03-02T00:00:00.000Z")),
            ("2026-2-32", None),
            ("2026-02-30 10:00", Some("2026-03-02T10:00:00.000Z")),
            ("2026-03-01 24:00", Some("2026-03-02T00:00:00.000Z")),
            ("2026-03-01 10:00Z", Some("2026-03-01T10:00:00.000Z")),
            ("2026-03-01 10:00:00.5", Some("2026-03-01T10:00:00.500Z")),
            ("20260301", None),
            ("2020-W01-1", None),
        ];
        for (input, want) in cases {
            assert_eq!(iso(input).as_deref(), *want, "{input}");
        }
        assert_eq!(v8_date_parse("-002026-01-01"), Some(-126_101_577_600_000));
        assert_eq!(v8_date_parse("99999-01-01"), Some(3_093_496_444_800_000));
        // Di luar rentang `Date` JS.
        assert_eq!(v8_date_parse("+275760-09-14"), None);
        assert_eq!(v8_date_parse("275760-09-13"), Some(8_640_000_000_000_000));
    }
}
