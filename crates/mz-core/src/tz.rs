//! Lookup nama zona `moment-timezone` (`moment.tz.zone(name)`, `normalizeName`): tanpa
//! membedakan huruf besar dan `/` setara `_` (`asia_jakarta` = `Asia/Jakarta`; diverifikasi
//! dengan moment-timezone Mudah-API 2026-11-16).
//!
//! Bukan untuk `@IsTimeZone()` / `Intl.DateTimeFormat` (tidak menyamakan `/` dengan `_`).

use chrono_tz::Tz;

/// `moment.tz.zone(name)`; `None` = tidak dikenal.
pub fn find_zone(name: &str) -> Option<Tz> {
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(tz);
    }
    let norm = |s: &str| s.to_ascii_lowercase().replace('/', "_");
    let wanted = norm(name);
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .find(|tz| norm(tz.name()) == wanted)
}

/// `tz && moment.tz.zone(tz) ? tz : 'UTC'`.
pub fn zone_or_utc(name: Option<&str>) -> Tz {
    name.filter(|n| !n.is_empty())
        .and_then(find_zone)
        .unwrap_or(Tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_moment() {
        let jkt = Some(chrono_tz::Asia::Jakarta);
        for n in [
            "Asia/Jakarta",
            "asia_jakarta",
            "Asia_Jakarta",
            "asia/jakarta",
            "ASIA/JAKARTA",
        ] {
            assert_eq!(find_zone(n), jkt, "{n}");
        }
        assert_eq!(
            find_zone("america_argentina_buenos_aires"),
            Some(chrono_tz::America::Argentina::Buenos_Aires)
        );
        assert_eq!(find_zone("Etc/GMT+7"), Some(chrono_tz::Etc::GMTPlus7));
        assert_eq!(find_zone("foo"), None);
        assert_eq!(zone_or_utc(Some("")), Tz::UTC);
        assert_eq!(zone_or_utc(Some("foo")), Tz::UTC);
        assert_eq!(zone_or_utc(None), Tz::UTC);
    }
}
