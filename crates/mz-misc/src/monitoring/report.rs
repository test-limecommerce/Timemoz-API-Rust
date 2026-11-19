//! Bagian murni `monitoring-report.service.ts`: env, validasi webhook, ambang alert, dan
//! payload webhook Discord. Semua teks (nama field, label, format angka) adalah kontrak.

use chrono::{DateTime, Utc};
use mz_core::jsfmt;
use serde_json::{Value, json};

/// Node: src/monitoring/monitoring-report.service.ts:16 `BOT_NAME`.
pub const BOT_NAME: &str = "Timemoz Monitoring";
/// Node: src/monitoring/monitoring-report.service.ts:17 `DEFAULT_INTERVAL_MINUTES`.
pub const DEFAULT_INTERVAL_MINUTES: u32 = 10;
/// Node: src/monitoring/monitoring-report.service.ts:25 `ACTIVE_TRACKING_WINDOW_MS`.
///
/// Segmen dianggap "sedang tracking" bila `endAtUtc`-nya tidak lebih lama dari jendela ini.
/// Sengaja tidak diikat ke interval laporan.
pub const ACTIVE_TRACKING_WINDOW_MS: i64 = 10 * 60_000;

/// Node: src/monitoring/monitoring-report.service.ts:27-29.
const WARN_PERCENT: f64 = 70.0;
const CRITICAL_PERCENT: f64 = 85.0;
pub const COLOR_OK: u32 = 0x2e_cc71;
pub const COLOR_WARN: u32 = 0xf1_c40f;
pub const COLOR_CRITICAL: u32 = 0xe7_4c3c;

/// Node: src/monitoring/monitoring-report.service.ts:31 `ALERT_THRESHOLD` (cpu/mem/disk = 80).
/// Lewat ambang ini (lebih dari, bukan sama dengan) → tag penerima alert.
pub const ALERT_THRESHOLD: f64 = 80.0;

/// Node: src/monitoring/digitalocean-metrics.client.ts:17 `ResourceUsage`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResourceUsage {
    pub cpu_percent: Option<f64>,
    pub mem_percent: Option<f64>,
    pub disk_percent: Option<f64>,
}

impl ResourceUsage {
    /// `EMPTY_USAGE`.
    pub const EMPTY: Self = Self {
        cpu_percent: None,
        mem_percent: None,
        disk_percent: None,
    };

    fn values(&self) -> [Option<f64>; 3] {
        [self.cpu_percent, self.mem_percent, self.disk_percent]
    }
}

/// Node: src/monitoring/monitoring-report.service.ts:33 `MonitoringReport`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitoringReport {
    pub at: DateTime<Utc>,
    pub active_user_tracking: Option<i64>,
    pub active_sales_on_visit: Option<i64>,
    pub server: ResourceUsage,
    pub database: ResourceUsage,
}

/// Node: src/monitoring/monitoring-report.service.ts:42 `intervalMinutesFromEnv`.
/// `MONITORING_INTERVAL_MINUTES` (bilangan bulat 1–1440, lewat `Number(raw)` JS), default 10.
pub fn interval_minutes_from_env(raw: Option<&str>) -> u32 {
    let Some(raw) = raw.filter(|r| !jsfmt::trim(r).is_empty()) else {
        return DEFAULT_INTERVAL_MINUTES;
    };
    let n = jsfmt::number(raw);
    if n.is_finite() && n.fract() == 0.0 && (1.0..=1440.0).contains(&n) {
        n as u32
    } else {
        DEFAULT_INTERVAL_MINUTES
    }
}

/// Node: src/monitoring/monitoring-report.service.ts:56 `isDiscordWebhookUrl`.
/// Laporan berisi angka infrastruktur; jangan sampai terkirim ke host lain karena salah isi env.
pub fn is_discord_webhook_url(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some_and(|h| {
            matches!(
                h,
                "discord.com" | "discordapp.com" | "ptb.discord.com" | "canary.discord.com"
            )
        })
        && url.path().starts_with("/api/webhooks/")
}

/// Node: src/monitoring/monitoring-report.service.ts:79 `alertUserIdsFromEnv`.
/// `MONITORING_ALERT_DISCORD_USER_IDS`: Discord user ID dipisah koma; entri yang bukan
/// snowflake (`/^\d{17,20}$/`) dibuang, duplikat dibuang (urutan pertama dipertahankan).
pub fn alert_user_ids_from_env(raw: Option<&str>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for s in raw.unwrap_or_default().split(',') {
        let s = jsfmt::trim(s);
        let snowflake = (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
        if snowflake && !ids.iter().any(|i| i == s) {
            ids.push(s.to_owned());
        }
    }
    ids
}

/// `fmtPercent`: `n.toFixed(1) + '%'`, null → `n/a`.
fn fmt_percent(n: Option<f64>) -> String {
    n.map_or_else(
        || "n/a".to_owned(),
        |n| format!("{}%", jsfmt::to_fixed(n, 1)),
    )
}

/// `fmtCount`: `n.toLocaleString('id-ID')` (bilangan bulat, pemisah ribuan `.`), null → `n/a`.
fn fmt_count(n: Option<i64>) -> String {
    let Some(n) = n else {
        return "n/a".to_owned();
    };
    let digits = n.unsigned_abs().to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

/// Node: src/monitoring/monitoring-report.service.ts:121 `usageLines`.
fn usage_lines(usage: &ResourceUsage) -> String {
    [
        format!("CPU  : {}", fmt_percent(usage.cpu_percent)),
        format!("RAM  : {}", fmt_percent(usage.mem_percent)),
        format!("Disk : {}", fmt_percent(usage.disk_percent)),
    ]
    .join("\n")
}

/// Node: src/monitoring/monitoring-report.service.ts:91 `alertBreaches`.
/// Daftar metrik yang melewati [`ALERT_THRESHOLD`], mis. `"Server CPU 80.1% (> 80%)"`.
pub fn alert_breaches(report: &MonitoringReport) -> Vec<String> {
    let mut breaches = Vec::new();
    for (name, usage) in [("Server", &report.server), ("Database", &report.database)] {
        for (metric, value) in [
            ("CPU", usage.cpu_percent),
            ("RAM", usage.mem_percent),
            ("Disk", usage.disk_percent),
        ] {
            if let Some(v) = value
                && v > ALERT_THRESHOLD
            {
                breaches.push(format!(
                    "{name} {metric} {} (> {}%)",
                    fmt_percent(Some(v)),
                    jsfmt::num_string(ALERT_THRESHOLD)
                ));
            }
        }
    }
    breaches
}

/// Warna embed mengikuti pemakaian tertinggi (semua null → hijau).
fn embed_color(report: &MonitoringReport) -> u32 {
    let peak = report
        .server
        .values()
        .into_iter()
        .chain(report.database.values())
        .flatten()
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
        })
        .unwrap_or(0.0);
    if peak >= CRITICAL_PERCENT {
        COLOR_CRITICAL
    } else if peak >= WARN_PERCENT {
        COLOR_WARN
    } else {
        COLOR_OK
    }
}

/// Node: src/monitoring/monitoring-report.service.ts:134 `buildDiscordPayload`.
///
/// Satu embed, warna mengikuti pemakaian tertinggi. Bila ada metrik lewat
/// [`ALERT_THRESHOLD`] dan ada penerima, `alert_user_ids` di-tag lewat `content` (mention di
/// dalam embed tidak memicu notifikasi). Urutan kunci sama dengan objek Node.
pub fn build_discord_payload(report: &MonitoringReport, alert_user_ids: &[String]) -> Value {
    let breaches = if alert_user_ids.is_empty() {
        Vec::new()
    } else {
        alert_breaches(report)
    };

    let mut payload = serde_json::Map::new();
    payload.insert("username".into(), json!(BOT_NAME));
    if !breaches.is_empty() {
        let mentions: Vec<String> = alert_user_ids.iter().map(|id| format!("<@{id}>")).collect();
        // "⚠️ "
        let content = format!(
            "{}\n\u{26a0}\u{fe0f} {}",
            mentions.join(" "),
            breaches.join(", ")
        );
        payload.insert("content".into(), json!(content));
    }
    // Hanya user penerima alert yang boleh ter-mention; @everyone/role tidak.
    let mut allowed = serde_json::Map::new();
    allowed.insert("parse".into(), json!([]));
    if !breaches.is_empty() {
        allowed.insert("users".into(), json!(alert_user_ids));
    }
    payload.insert("allowed_mentions".into(), Value::Object(allowed));
    payload.insert(
        "embeds".into(),
        json!([{
            "title": "Laporan Monitoring Timemoz",
            "color": embed_color(report),
            "timestamp": mz_core::time::to_js_iso(&report.at),
            "fields": [
                {
                    // "👥 User sedang tracking"
                    "name": "\u{1f465} User sedang tracking",
                    "value": fmt_count(report.active_user_tracking),
                    "inline": true,
                },
                {
                    // "🛍️ Sales sedang kunjungan"
                    "name": "\u{1f6cd}\u{fe0f} Sales sedang kunjungan",
                    "value": fmt_count(report.active_sales_on_visit),
                    "inline": true,
                },
                {
                    // "🖥️ Server (Droplet)"
                    "name": "\u{1f5a5}\u{fe0f} Server (Droplet)",
                    "value": format!("```\n{}\n```", usage_lines(&report.server)),
                    "inline": false,
                },
                {
                    // "🗄️ Database"
                    "name": "\u{1f5c4}\u{fe0f} Database",
                    "value": format!("```\n{}\n```", usage_lines(&report.database)),
                    "inline": false,
                },
            ],
        }]),
    );
    Value::Object(payload)
}

/// Port `monitoring-report.service.spec.ts` (bagian murni).
#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const WEBHOOK: &str = "https://discord.com/api/webhooks/123/abc";
    const ALICE: &str = "123456789012345678";
    const BOB: &str = "876543210987654321";

    fn usage(cpu: Option<f64>, mem: Option<f64>, disk: Option<f64>) -> ResourceUsage {
        ResourceUsage {
            cpu_percent: cpu,
            mem_percent: mem,
            disk_percent: disk,
        }
    }

    fn u(cpu: f64, mem: f64, disk: f64) -> ResourceUsage {
        usage(Some(cpu), Some(mem), Some(disk))
    }

    fn report() -> MonitoringReport {
        MonitoringReport {
            at: Utc.with_ymd_and_hms(2026, 10, 23, 4, 20, 0).unwrap(),
            active_user_tracking: Some(1234),
            active_sales_on_visit: Some(37),
            server: u(21.5, 38.4, 22.6),
            database: u(24.5, 66.9, 14.9),
        }
    }

    fn ids() -> Vec<String> {
        vec![ALICE.to_owned(), BOB.to_owned()]
    }

    #[test]
    fn interval_minutes() {
        let cases: [(Option<&str>, u32); 13] = [
            (None, 10),
            (Some(""), 10),
            (Some("  "), 10),
            (Some("5"), 5),
            (Some("60"), 60),
            (Some("1440"), 1440),
            (Some("0"), 10),
            (Some("-5"), 10),
            (Some("2.5"), 10),
            (Some("1441"), 10),
            (Some("abc"), 10),
            // `Number()` JS: spasi dibuang, `5.0` bilangan bulat.
            (Some(" 15 "), 15),
            (Some("5.0"), 5),
        ];
        for (raw, expected) in cases {
            assert_eq!(interval_minutes_from_env(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn discord_webhook_url() {
        let cases = [
            (WEBHOOK, true),
            ("https://discordapp.com/api/webhooks/1/x", true),
            ("https://canary.discord.com/api/webhooks/1/x", true),
            ("https://ptb.discord.com/api/webhooks/1/x", true),
            ("https://DISCORD.com/api/webhooks/1/x", true),
            ("http://discord.com/api/webhooks/1/x", false),
            ("https://discord.com.evil.com/api/webhooks/1/x", false),
            ("https://evil.com/api/webhooks/1/x", false),
            ("https://discord.com/channels/1", false),
            ("bukan url", false),
        ];
        for (url, expected) in cases {
            assert_eq!(is_discord_webhook_url(url), expected, "{url}");
        }
    }

    #[test]
    fn alert_user_ids() {
        let cases: [(Option<String>, Vec<String>); 5] = [
            (None, vec![]),
            (Some(String::new()), vec![]),
            (Some(format!("{ALICE}, {BOB}")), ids()),
            (Some(format!("{ALICE},{ALICE}")), vec![ALICE.to_owned()]),
            (
                Some(format!("{ALICE},@everyone,<@1>,abc")),
                vec![ALICE.to_owned()],
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(alert_user_ids_from_env(raw.as_deref()), expected, "{raw:?}");
        }
    }

    #[test]
    fn alert_all_at_threshold_no_content_no_mention() {
        let mut r = report();
        r.server = u(80.0, 80.0, 80.0);
        r.database = u(80.0, 80.0, 80.0);
        let p = build_discord_payload(&r, &ids());
        assert!(p.get("content").is_none());
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn alert_breach_tags_both() {
        let cases: [(&str, ResourceUsage, ResourceUsage, &str); 5] = [
            (
                "CPU server",
                u(80.1, 10.0, 10.0),
                u(1.0, 1.0, 1.0),
                "Server CPU 80.1%",
            ),
            (
                "RAM server",
                u(10.0, 90.0, 10.0),
                u(1.0, 1.0, 1.0),
                "Server RAM 90.0%",
            ),
            (
                "Disk server",
                u(10.0, 10.0, 80.5),
                u(1.0, 1.0, 1.0),
                "Server Disk 80.5%",
            ),
            (
                "CPU DB",
                u(1.0, 1.0, 1.0),
                u(81.0, 10.0, 10.0),
                "Database CPU 81.0%",
            ),
            (
                "Disk DB",
                u(1.0, 1.0, 1.0),
                u(10.0, 10.0, 81.0),
                "Database Disk 81.0%",
            ),
        ];
        for (label, server, database, text) in cases {
            let mut r = report();
            r.server = server;
            r.database = database;
            let p = build_discord_payload(&r, &ids());
            let content = p["content"].as_str().unwrap();
            assert!(content.contains(&format!("<@{ALICE}> <@{BOB}>")), "{label}");
            assert!(content.contains(text), "{label}: {content}");
            assert_eq!(
                p["allowed_mentions"],
                json!({ "parse": [], "users": [ALICE, BOB] }),
                "{label}"
            );
        }
    }

    #[test]
    fn alert_content_exact() {
        let mut r = report();
        r.server = u(80.1, 90.0, 10.0);
        let p = build_discord_payload(&r, &ids());
        assert_eq!(
            p["content"],
            format!(
                "<@{ALICE}> <@{BOB}>\n\u{26a0}\u{fe0f} Server CPU 80.1% (> 80%), Server RAM 90.0% (> 80%)"
            )
        );
        // Urutan kunci = objek Node.
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["username", "content", "allowed_mentions", "embeds"]);
    }

    #[test]
    fn disk_78_no_alert() {
        let mut r = report();
        r.server = u(10.0, 10.0, 78.0);
        assert!(build_discord_payload(&r, &ids()).get("content").is_none());
    }

    #[test]
    fn breach_without_recipients_no_content() {
        let mut r = report();
        r.server = u(99.0, 99.0, 99.0);
        let p = build_discord_payload(&r, &[]);
        assert!(p.get("content").is_none());
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn bot_name_without_mentions() {
        let p = build_discord_payload(&report(), &[]);
        assert_eq!(p["username"], "Timemoz Monitoring");
        assert_eq!(BOT_NAME, "Timemoz Monitoring");
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn full_payload_exact() {
        let p = build_discord_payload(&report(), &[]);
        assert_eq!(
            p,
            json!({
                "username": "Timemoz Monitoring",
                "allowed_mentions": { "parse": [] },
                "embeds": [{
                    "title": "Laporan Monitoring Timemoz",
                    "color": 0x2e_cc71,
                    "timestamp": "2026-10-23T04:20:00.000Z",
                    "fields": [
                        { "name": "👥 User sedang tracking", "value": "1.234", "inline": true },
                        { "name": "🛍️ Sales sedang kunjungan", "value": "37", "inline": true },
                        {
                            "name": "🖥️ Server (Droplet)",
                            "value": "```\nCPU  : 21.5%\nRAM  : 38.4%\nDisk : 22.6%\n```",
                            "inline": false
                        },
                        {
                            "name": "🗄️ Database",
                            "value": "```\nCPU  : 24.5%\nRAM  : 66.9%\nDisk : 14.9%\n```",
                            "inline": false
                        }
                    ]
                }]
            })
        );
    }

    #[test]
    fn null_shows_na() {
        let mut r = report();
        r.active_user_tracking = None;
        r.database = ResourceUsage::EMPTY;
        let p = build_discord_payload(&r, &[]);
        let fields = &p["embeds"][0]["fields"];
        assert_eq!(fields[0]["value"], "n/a");
        assert!(fields[3]["value"].as_str().unwrap().contains("CPU  : n/a"));
    }

    #[test]
    fn colors() {
        let cases = [
            (u(10.0, 69.9, 5.0), COLOR_OK),
            (u(10.0, 70.0, 5.0), COLOR_WARN),
            (u(10.0, 20.0, 85.0), COLOR_CRITICAL),
        ];
        for (db, color) in cases {
            let mut r = report();
            r.database = db;
            assert_eq!(build_discord_payload(&r, &[])["embeds"][0]["color"], color);
        }
        let mut r = report();
        r.server = ResourceUsage::EMPTY;
        r.database = ResourceUsage::EMPTY;
        assert_eq!(
            build_discord_payload(&r, &[])["embeds"][0]["color"],
            COLOR_OK
        );
    }

    #[test]
    fn number_formats() {
        assert_eq!(fmt_count(Some(0)), "0");
        assert_eq!(fmt_count(Some(999)), "999");
        assert_eq!(fmt_count(Some(1_234_567)), "1.234.567");
        // `toFixed` membulatkan setengah ke atas (Rust `{:.1}` ke genap).
        assert_eq!(fmt_percent(Some(22.25)), "22.3%");
        assert_eq!(fmt_percent(Some(0.0)), "0.0%");
    }
}
