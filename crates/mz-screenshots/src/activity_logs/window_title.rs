//! Port `src/activity-logs/window-title.ts`: normalisasi judul jendela dan
//! penggabungan sesi berurutan saat baca.

use std::collections::HashMap;

use mz_core::time::JsDate;

/// `window_title` dipotong ke 1024 karakter sebelum dinormalisasi (cegah biaya berlebih).
const MAX_INPUT: usize = 1024;

/// Klien desktop mengirim log tiap ~30 detik; jeda sampai 90 detik masih satu sesi.
pub const MERGE_GAP_MS: i64 = 90_000;

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => s.get(..i).unwrap_or(s),
        None => s,
    }
}

/// Posisi `ZOOM_MARKER = /\s@\s?[\d.,]+\s?%/` pertama (indeks byte spasi pembuka).
fn find_zoom_marker(input: &str) -> Option<usize> {
    for (i, c) in input.char_indices() {
        if !c.is_whitespace() {
            continue;
        }
        let rest = input.get(i + c.len_utf8()..)?;
        let Some(after_at) = rest.strip_prefix('@') else {
            continue;
        };
        let mut s = after_at;
        if let Some(c2) = s.chars().next()
            && c2.is_whitespace()
        {
            s = s.get(c2.len_utf8()..).unwrap_or("");
        }
        let digits = s
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_digit() || *ch == '.' || *ch == ',')
            .last();
        let Some((last_i, last_c)) = digits else {
            continue;
        };
        let mut s = s.get(last_i + last_c.len_utf8()..).unwrap_or("");
        if let Some(c3) = s.chars().next()
            && c3.is_whitespace()
        {
            s = s.get(c3.len_utf8()..).unwrap_or("");
        }
        if s.starts_with('%') {
            return Some(i);
        }
    }
    None
}

/// `/^\(\d+\+?\)\s+/` dibuang.
fn strip_leading_counter(s: &str) -> &str {
    let Some(rest) = s.strip_prefix('(') else {
        return s;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return s;
    }
    let mut rest = rest.get(digits..).unwrap_or("");
    if let Some(r) = rest.strip_prefix('+') {
        rest = r;
    }
    let Some(rest) = rest.strip_prefix(')') else {
        return s;
    };
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        // `\s+` wajib minimal satu spasi.
        return s;
    }
    trimmed
}

/// `/^(?:\*|●|•)\s*/` dibuang.
fn strip_leading_dirty(s: &str) -> &str {
    for marker in ['*', '●', '•'] {
        if let Some(rest) = s.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    s
}

/// `normalizeWindowTitle`: zoom/mode warna, penanda belum disimpan, dan penghitung notifikasi dibuang.
pub fn normalize_window_title(raw: &str) -> String {
    let input = truncate_chars(raw, MAX_INPUT);
    let base = match find_zoom_marker(input) {
        Some(i) => input.get(..i).unwrap_or(input),
        None => input,
    };
    let s = strip_leading_counter(base.trim());
    let s = strip_leading_dirty(s).trim();
    let normalized = match s.strip_suffix('*') {
        Some(head) => head.trim_end(),
        None => s,
    };
    if normalized.is_empty() {
        input.trim().to_owned()
    } else {
        normalized.to_owned()
    }
}

/// `continues(previousEndAt, nextStartAt)`.
pub fn continues(previous_end: &JsDate, next_start: &JsDate) -> bool {
    next_start.timestamp_millis() <= previous_end.timestamp_millis() + MERGE_GAP_MS
}

/// Baris `app`/`url` (tabel keduanya dipakai bersama; kolom yang tak relevan `None`).
#[derive(Debug, Clone)]
pub struct Session {
    pub id: i32,
    pub tracking_timeline_id: i32,
    pub uuid: String,
    pub name: Option<String>,
    pub title: String,
    pub url: Option<String>,
    pub host: Option<String>,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub timezone: String,
}

/// `groupContinuousSessions`: baris berurutan dalam satu timeline dengan kunci sama jadi satu baris.
/// `breakers` = (timelineId, startMs) baris tabel lain yang memutus grup bila mulai di jeda.
pub fn group_continuous_sessions(
    rows: &[Session],
    key_of: impl Fn(&Session) -> String,
    breakers: &[(i32, i64)],
) -> Vec<Session> {
    let mut sorted: Vec<Session> = rows.to_vec();
    sorted.sort_by(|a, b| {
        a.tracking_timeline_id
            .cmp(&b.tracking_timeline_id)
            .then(
                a.start_at
                    .timestamp_millis()
                    .cmp(&b.start_at.timestamp_millis()),
            )
            .then(a.id.cmp(&b.id))
    });
    let mut breaker_starts: HashMap<i32, Vec<i64>> = HashMap::new();
    for (timeline, start) in breakers {
        breaker_starts.entry(*timeline).or_default().push(*start);
    }
    for list in breaker_starts.values_mut() {
        list.sort_unstable();
    }
    // Breaker pertama >= from; memutus bila < to.
    let broken_between = |timeline: i32, from: i64, to: i64| -> bool {
        breaker_starts.get(&timeline).is_some_and(|list| {
            let idx = list.partition_point(|v| *v < from);
            list.get(idx).is_some_and(|v| *v < to)
        })
    };

    struct Group {
        row: Session,
        key: String,
        /// Urutan kemunculan judul (seri → yang muncul lebih dulu).
        titles: Vec<(String, usize)>,
    }
    let bump = |titles: &mut Vec<(String, usize)>, title: String| {
        if let Some(entry) = titles.iter_mut().find(|(t, _)| *t == title) {
            entry.1 += 1;
        } else {
            titles.push((title, 1));
        }
    };

    let mut groups: Vec<Group> = Vec::new();
    for row in sorted {
        let key = key_of(&row);
        let title = normalize_window_title(&row.title);
        if let Some(current) = groups.last_mut()
            && current.row.tracking_timeline_id == row.tracking_timeline_id
            && current.key == key
            && !broken_between(
                row.tracking_timeline_id,
                current.row.end_at.timestamp_millis(),
                row.start_at.timestamp_millis(),
            )
        {
            if row.end_at.timestamp_millis() > current.row.end_at.timestamp_millis() {
                current.row.end_at = row.end_at;
            }
            bump(&mut current.titles, title);
            continue;
        }
        groups.push(Group {
            row,
            key,
            titles: vec![(title, 1)],
        });
    }

    groups
        .into_iter()
        .map(|g| {
            let mut best = 0usize;
            let mut title = g.row.title.clone();
            for (candidate, count) in &g.titles {
                if *count > best {
                    title = candidate.clone();
                    best = *count;
                }
            }
            Session { title, ..g.row }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_titles() {
        let cases = [
            ("File.ai* @ 66,67 % (RGB/Preview)", "File.ai"),
            ("File.psd @ 33.3% (Layer 1, RGB/8) *", "File.psd"),
            ("*File.indd @ 75%", "File.indd"),
            ("(3) WhatsApp", "WhatsApp"),
            ("(99+) Inbox", "Inbox"),
            ("(3)WhatsApp", "(3)WhatsApp"),
            ("● main.rs - VS Code", "main.rs - VS Code"),
            ("File*", "File"),
            ("  plain  ", "plain"),
            ("Doc @ 50", "Doc @ 50"),
            ("a @ %", "a @ %"),
            ("*", "*"),
            ("", ""),
        ];
        for (raw, want) in cases {
            assert_eq!(normalize_window_title(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn long_titles_are_cut_before_matching() {
        let long = "x".repeat(2000);
        assert_eq!(normalize_window_title(&long).len(), MAX_INPUT);
    }

    fn at(ms: i64) -> JsDate {
        JsDate::from_millis(ms).unwrap_or_else(JsDate::now)
    }

    fn s(id: i32, timeline: i32, name: &str, title: &str, start: i64, end: i64) -> Session {
        Session {
            id,
            tracking_timeline_id: timeline,
            uuid: String::new(),
            name: Some(name.into()),
            title: title.into(),
            url: None,
            host: None,
            start_at: at(start),
            end_at: at(end),
            timezone: "UTC".into(),
        }
    }

    #[test]
    fn continuity_gap() {
        assert!(continues(&at(1_000), &at(91_000)));
        assert!(!continues(&at(1_000), &at(91_001)));
    }

    #[test]
    fn groups_by_key_and_picks_most_frequent_title() {
        let rows = vec![
            s(1, 1, "Code", "a.rs", 0, 10_000),
            s(2, 1, "Code", "b.rs", 10_000, 20_000),
            s(3, 1, "Code", "b.rs", 20_000, 30_000),
            s(4, 1, "Chrome", "tab", 30_000, 40_000),
            s(5, 2, "Chrome", "tab", 30_000, 40_000),
        ];
        let out = group_continuous_sessions(&rows, |r| r.name.clone().unwrap_or_default(), &[]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].title, "b.rs");
        assert_eq!(out[0].end_at.timestamp_millis(), 30_000);
        assert_eq!(out[1].name.as_deref(), Some("Chrome"));
    }

    #[test]
    fn breaker_in_gap_splits_group_but_not_inside() {
        let rows = vec![
            s(1, 1, "Code", "a", 0, 10_000),
            s(2, 1, "Code", "a", 20_000, 30_000),
        ];
        let key = |r: &Session| r.name.clone().unwrap_or_default();
        // breaker mulai di jeda [10_000, 20_000) -> putus
        assert_eq!(
            group_continuous_sessions(&rows, key, &[(1, 15_000)]).len(),
            2
        );
        // breaker di dalam rentang grup / lain timeline -> tidak putus
        assert_eq!(
            group_continuous_sessions(&rows, key, &[(1, 5_000)]).len(),
            1
        );
        assert_eq!(
            group_continuous_sessions(&rows, key, &[(2, 15_000)]).len(),
            1
        );
        // tepat di akhir grup (>= from) memutus; tepat di awal baris berikut tidak (< to)
        assert_eq!(
            group_continuous_sessions(&rows, key, &[(1, 10_000)]).len(),
            2
        );
        assert_eq!(
            group_continuous_sessions(&rows, key, &[(1, 20_000)]).len(),
            1
        );
    }
}
