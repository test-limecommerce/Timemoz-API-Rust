//! Port `src/common/pdf/report-pdf-format.ts` + `report-pdf-theme.ts`: formatter teks PDF (locale `en`
//! untuk laporan kerja, `id` untuk laporan lembur lewat [`ReportFormat`]) dan palet warna. Tanggal "YYYY-MM-DD" ditafsirkan sebagai tanggal kalender (UTC).

use chrono::{DateTime, Datelike, NaiveDate, Utc};

pub mod theme {
    pub const NAVY: &str = "#0f2a5f";
    pub const NAVY_BOX: &str = "#1e3a8a";
    pub const NAVY_TEXT: &str = "#c7d2fe";
    pub const NAVY_MUTED: &str = "#a5b4fc";
    pub const PRIMARY: &str = "#1d4ed8";
    pub const PRIMARY_SOFT: &str = "#e0e9ff";
    pub const DARK: &str = "#0f172a";
    pub const TEXT: &str = "#334155";
    pub const MUTED: &str = "#64748b";
    pub const FAINT: &str = "#94a3b8";
    pub const BORDER: &str = "#e2e8f0";
    pub const SURFACE: &str = "#f8fafc";
    pub const SURFACE2: &str = "#f1f5f9";
    pub const WHITE: &str = "#ffffff";
    pub const CYAN: &str = "#0891b2";
    pub const VIOLET: &str = "#7c3aed";
    pub const GREEN: &str = "#059669";
    pub const AMBER: &str = "#d97706";

    pub const PROJECT_COLORS: [&str; 10] = [
        "#1d4ed8", "#0891b2", "#7c3aed", "#059669", "#d97706", "#db2777", "#4f46e5", "#0d9488",
        "#64748b", "#ea580c",
    ];

    /// Margin kiri/kanan/atas.
    pub const M: f64 = 40.0;
    /// Ruang bawah untuk footer (dua baris).
    pub const BOTTOM_RESERVE: f64 = 62.0;
}

const MONTHS_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_ID: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
];
const WEEKDAYS_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAYS_ID: [&str; 7] = ["Min", "Sen", "Sel", "Rab", "Kam", "Jum", "Sab"];

/// `cleanText`: buang karakter yang tidak bisa dirender font standar.
pub fn clean_text(value: Option<&str>) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in value.unwrap_or("").chars() {
        if matches!(c, '\r' | '\n' | '\t') {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
            continue;
        }
        in_ws = false;
        if matches!(c, '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' | '–' | '—' | '‘' | '’' | '“' | '”' | '•' | '…')
        {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// `parseDate`: "YYYY-MM-DD" (komponen hilang/tidak valid -> 1).
pub fn parse_date(date: &str) -> NaiveDate {
    let mut parts = date.split('-').map(|p| p.parse::<i32>().unwrap_or(0));
    let y = parts.next().unwrap_or(1970);
    let m = parts.next().filter(|m| *m != 0).unwrap_or(1);
    let d = parts.next().filter(|d| *d != 0).unwrap_or(1);
    let m = u32::try_from(m).unwrap_or(1);
    let d = u32::try_from(d).unwrap_or(1);
    // JS `Date.UTC` meluapkan hari (mis. 31 Feb -> 3 Mar).
    NaiveDate::from_ymd_opt(y, m.clamp(1, 12), 1)
        .and_then(|first| first.checked_add_signed(chrono::Duration::days(i64::from(d) - 1)))
        .unwrap_or_default()
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `enumerateDays`: semua tanggal dari `start` sampai `end` inklusif, maks 3700 hari.
pub fn enumerate_days(start: &str, end: &str) -> Vec<String> {
    let from = parse_date(start);
    let to = parse_date(end);
    let mut days = Vec::new();
    let mut cur = from;
    while cur <= to && days.len() < 3700 {
        days.push(ymd(cur));
        match cur.succ_opt() {
            Some(n) => cur = n,
            None => break,
        }
    }
    if days.is_empty() {
        days.push(start.to_owned());
    }
    days
}

fn month_name(locale: Locale, d: NaiveDate) -> &'static str {
    let months = match locale {
        Locale::En => &MONTHS_EN,
        Locale::Id => &MONTHS_ID,
    };
    months.get(d.month0() as usize).copied().unwrap_or("")
}

fn weekday_name(locale: Locale, d: NaiveDate) -> &'static str {
    let days = match locale {
        Locale::En => &WEEKDAYS_EN,
        Locale::Id => &WEEKDAYS_ID,
    };
    days.get(d.weekday().num_days_from_sunday() as usize)
        .copied()
        .unwrap_or("")
}

/// `fDate`: `05 Oct 2026` / `05 Okt 2026`.
pub fn f_date_in(locale: Locale, date: &str) -> String {
    let d = parse_date(date);
    format!("{:02} {} {}", d.day(), month_name(locale, d), d.year())
}

/// `fDay`: `Mon, 05 Oct` / `Sen, 05 Okt`.
pub fn f_day_in(locale: Locale, date: &str) -> String {
    let d = parse_date(date);
    format!(
        "{}, {:02} {}",
        weekday_name(locale, d),
        d.day(),
        month_name(locale, d)
    )
}

/// `fDayShort`: `05 Oct` / `05 Okt`.
pub fn f_day_short_in(locale: Locale, date: &str) -> String {
    let d = parse_date(date);
    format!("{:02} {}", d.day(), month_name(locale, d))
}

/// `fGeneratedAt`: `05 Oct 2026, 09:15 WIB` di zona company. Zona dicari seperti
/// `moment.tz.zone` (tak peka huruf, `/`≈`_`); UTC bila kosong/tidak dikenal.
pub fn f_generated_at_in(
    locale: Locale,
    generated_at: DateTime<Utc>,
    time_zone: Option<&str>,
) -> String {
    match time_zone.and_then(mz_core::tz::find_zone) {
        Some(tz) => {
            let local = generated_at.with_timezone(&tz);
            format!(
                "{:02} {} {}, {} {}",
                local.day(),
                month_name(locale, local.date_naive()),
                local.year(),
                local.format("%H:%M"),
                local.format("%Z")
            )
        }
        None => format!(
            "{:02} {} {}, {} UTC",
            generated_at.day(),
            month_name(locale, generated_at.date_naive()),
            generated_at.year(),
            generated_at.format("%H:%M")
        ),
    }
}

/// `05 Oct 2026`
pub fn f_date(date: &str) -> String {
    f_date_in(Locale::En, date)
}

/// `Mon, 05 Oct`
pub fn f_day(date: &str) -> String {
    f_day_in(Locale::En, date)
}

/// `05 Oct`
pub fn f_day_short(date: &str) -> String {
    f_day_short_in(Locale::En, date)
}

/// `fGeneratedAt(…, 'en')`.
pub fn f_generated_at(generated_at: DateTime<Utc>, time_zone: Option<&str>) -> String {
    f_generated_at_in(Locale::En, generated_at, time_zone)
}

/// `ReportLocale` (`en` | `id`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Locale {
    #[default]
    En,
    Id,
}

impl Locale {
    /// `HOUR_SUFFIX`.
    pub fn hour_suffix(self) -> &'static str {
        match self {
            Self::En => "h",
            Self::Id => "j",
        }
    }
}

/// `fDur(seconds, locale)`: detik dibulatkan ke menit: `11h 05m` (`11j 05m` untuk `id`),
/// `45m` bila < 1 jam.
pub fn f_dur_in(locale: Locale, seconds: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let total_minutes = ((seconds.max(0.0) / 60.0) + 0.5).floor() as i64;
    let h = total_minutes / 60;
    let m = total_minutes % 60;
    if h > 0 {
        format!("{h}{} {m:02}m", locale.hour_suffix())
    } else {
        format!("{m}m")
    }
}

/// `fDur(seconds, 'en')`.
pub fn f_dur(seconds: f64) -> String {
    f_dur_in(Locale::En, seconds)
}

/// `reportFormat(locale)`: formatter yang terikat ke satu locale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportFormat(pub Locale);

impl ReportFormat {
    pub fn f_date(self, date: &str) -> String {
        f_date_in(self.0, date)
    }

    pub fn f_day(self, date: &str) -> String {
        f_day_in(self.0, date)
    }

    pub fn f_day_short(self, date: &str) -> String {
        f_day_short_in(self.0, date)
    }

    pub fn f_generated_at(self, generated_at: DateTime<Utc>, time_zone: Option<&str>) -> String {
        f_generated_at_in(self.0, generated_at, time_zone)
    }

    pub fn f_dur(self, seconds: f64) -> String {
        f_dur_in(self.0, seconds)
    }

    pub fn chart_buckets(
        self,
        period_days: &[String],
        value_of: impl Fn(&str) -> f64,
    ) -> (Vec<ChartBucket>, bool) {
        chart_buckets_in(self.0, period_days, value_of)
    }
}

/// `pct`: rasio -> persen bulat.
pub fn pct(ratio: f64) -> String {
    let r = if ratio.is_finite() { ratio } else { 0.0 };
    format!("{}%", ((r * 100.0) + 0.5).floor())
}

/// Inisial dua kata pertama; `?` bila kosong.
pub fn initials(name: &str) -> String {
    let out: String = clean_text(Some(name))
        .split(' ')
        .filter(|p| !p.is_empty())
        .take(2)
        .filter_map(|p| p.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    if out.is_empty() { "?".to_owned() } else { out }
}

/// `ReportChartBucket`: satu batang grafik; `parts` (opsional) = susunan warna dari bawah
/// ke atas.
#[derive(Debug, Clone)]
pub struct ChartBucket {
    pub label: String,
    pub seconds: f64,
    pub parts: Option<Vec<ChartPart>>,
}

/// Satu bagian batang bertumpuk.
#[derive(Debug, Clone, Copy)]
pub struct ChartPart {
    pub seconds: f64,
    pub color: &'static str,
}

/// `chartBuckets`: > 31 hari diringkas per minggu (mulai Senin); > 7 hari label pendek.
pub fn chart_buckets(
    period_days: &[String],
    value_of: impl Fn(&str) -> f64,
) -> (Vec<ChartBucket>, bool) {
    chart_buckets_in(Locale::En, period_days, value_of)
}

/// `chartBuckets(periodDays, valueOf, reportFormat(locale))`.
pub fn chart_buckets_in(
    locale: Locale,
    period_days: &[String],
    value_of: impl Fn(&str) -> f64,
) -> (Vec<ChartBucket>, bool) {
    let weekly = period_days.len() > 31;
    if !weekly {
        let short = period_days.len() > 7;
        let buckets = period_days
            .iter()
            .map(|d| ChartBucket {
                label: if short {
                    f_day_short_in(locale, d)
                } else {
                    f_day_in(locale, d)
                },
                seconds: value_of(d),
                parts: None,
            })
            .collect();
        return (buckets, weekly);
    }
    let mut order: Vec<String> = Vec::new();
    let mut weeks: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for date in period_days {
        let d = parse_date(date);
        let back = i64::from(d.weekday().num_days_from_monday());
        let monday = d
            .checked_sub_signed(chrono::Duration::days(back))
            .unwrap_or(d);
        let key = ymd(monday);
        if !weeks.contains_key(&key) {
            order.push(key.clone());
        }
        *weeks.entry(key).or_insert(0.0) += value_of(date);
    }
    let buckets = order
        .into_iter()
        .map(|k| {
            let seconds = weeks.get(&k).copied().unwrap_or(0.0);
            ChartBucket {
                label: f_day_short_in(locale, &k),
                seconds,
                parts: None,
            }
        })
        .collect();
    (buckets, weekly)
}

/// Pembulatan JS `Math.round` untuk pecahan positif.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cleans_text() {
        assert_eq!(clean_text(Some("a\r\n\tb 日本 é — ok")), "a b  é — ok");
        assert_eq!(clean_text(None), "");
        assert_eq!(clean_text(Some("😀 hi")), "hi");
    }

    #[test]
    fn date_formats() {
        assert_eq!(f_date("2026-10-05"), "05 Oct 2026");
        assert_eq!(f_day("2026-10-05"), "Mon, 05 Oct");
        assert_eq!(f_day_short("2026-01-31"), "31 Jan");
    }

    #[test]
    fn enumerates() {
        assert_eq!(
            enumerate_days("2026-02-27", "2026-03-02"),
            vec!["2026-02-27", "2026-02-28", "2026-03-01", "2026-03-02"]
        );
        assert_eq!(
            enumerate_days("2026-03-02", "2026-02-27"),
            vec!["2026-03-02"]
        );
    }

    #[test]
    fn durations_and_pct() {
        assert_eq!(f_dur(0.0), "0m");
        assert_eq!(f_dur(2700.0), "45m");
        assert_eq!(f_dur(39_900.0), "11h 05m");
        assert_eq!(f_dur(59.0), "1m");
        assert_eq!(f_dur(29.0), "0m");
        assert_eq!(pct(0.456), "46%");
        assert_eq!(pct(f64::NAN), "0%");
    }

    #[test]
    fn initials_of() {
        assert_eq!(initials("Maria Wijaya Kusuma"), "MW");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("budi"), "B");
    }

    #[test]
    fn generated_at_in_zone() {
        let t = Utc
            .with_ymd_and_hms(2026, 10, 5, 2, 15, 0)
            .single()
            .unwrap_or_default();
        assert_eq!(
            f_generated_at(t, Some("Asia/Jakarta")),
            "05 Oct 2026, 09:15 WIB"
        );
        assert_eq!(
            f_generated_at(t, Some("Nope/Zone")),
            "05 Oct 2026, 02:15 UTC"
        );
        assert_eq!(f_generated_at(t, None), "05 Oct 2026, 02:15 UTC");
    }

    #[test]
    fn buckets_daily_and_weekly() {
        let days = enumerate_days("2026-10-05", "2026-10-07");
        let (b, weekly) = chart_buckets(&days, |d| if d == "2026-10-06" { 3600.0 } else { 0.0 });
        assert!(!weekly);
        assert_eq!(b[1].label, "Tue, 06 Oct");
        assert_eq!(b[1].seconds, 3600.0);
        let long = enumerate_days("2026-10-01", "2026-11-15");
        let (w, weekly) = chart_buckets(&long, |_| 60.0);
        assert!(weekly);
        assert_eq!(w[0].label, "28 Sep");
        assert_eq!(w[0].seconds, 60.0 * 4.0);
    }

    #[test]
    fn id_locale_like_report_format_id() {
        let id = ReportFormat(Locale::Id);
        assert_eq!(id.f_date("2026-10-05"), "05 Okt 2026");
        assert_eq!(id.f_day("2026-10-20"), "Sel, 20 Okt");
        assert_eq!(id.f_day_short("2026-05-31"), "31 Mei");
        assert_eq!(id.f_dur(0.0), "0m");
        assert_eq!(id.f_dur(2700.0), "45m");
        assert_eq!(id.f_dur(39_900.0), "11j 05m");
        let t = Utc
            .with_ymd_and_hms(2026, 10, 26, 5, 24, 0)
            .single()
            .unwrap_or_default();
        assert_eq!(
            id.f_generated_at(t, Some("Asia/Jakarta")),
            "26 Okt 2026, 12:24 WIB"
        );
        // moment.tz.zone tak peka huruf.
        assert_eq!(
            id.f_generated_at(t, Some("asia/jakarta")),
            "26 Okt 2026, 12:24 WIB"
        );
        assert_eq!(
            id.f_generated_at(t, Some("Nope/Zone")),
            "26 Okt 2026, 05:24 UTC"
        );
        let days: Vec<String> = (1..=9).map(|d| format!("2026-10-{d:02}")).collect();
        let (b, weekly) = id.chart_buckets(&days, |_| 60.0);
        assert!(!weekly);
        assert_eq!(b.first().map(|x| x.label.as_str()), Some("01 Okt"));
    }
}
