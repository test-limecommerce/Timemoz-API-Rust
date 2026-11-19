//! Baris tabel dibaca sebagai `to_jsonb(row)` (kolom camelCase persis seperti hasil
//! Prisma), lalu kolom tanggalnya disamakan dengan `Date.toJSON()`.

use chrono::NaiveDateTime;
use mz_core::time::JsDate;
use serde_json::Value;

/// Model `scheduledReport` menurut urutan field `prisma/schema.prisma`.
pub const SCHEDULED_REPORT_KEYS: &[&str] = &[
    "id",
    "uuid",
    "name",
    "sendTime",
    "frequency",
    "days",
    "emails",
    "subject",
    "message",
    "userIds",
    "projectIds",
    "sections",
    "isActive",
    "lastSentAt",
    "companyId",
    "createdById",
    "createdAt",
    "updatedAt",
];

/// Model `managerSummaryConfig` menurut urutan field `prisma/schema.prisma`.
pub const MANAGER_SUMMARY_KEYS: &[&str] = &[
    "id",
    "uuid",
    "companyId",
    "isDailyEnabled",
    "isWeeklyEnabled",
    "sendTime",
    "includeWeekends",
    "emails",
    "userIds",
    "lowHoursThreshold",
    "disabledFlags",
    "lastDailySentAt",
    "lastWeeklySentAt",
    "createdById",
    "createdAt",
    "updatedAt",
];

/// Susun ulang kunci tingkat atas: `keys` yang ada lebih dulu menurut urutan `keys` (urutan field
/// model Prisma), sisanya mengikuti urutan semula. `to_jsonb(row)` mengurutkan kunci menurut
/// panjang lalu abjad, bukan menurut Prisma. Nilai (termasuk kolom `Json` bersarang) tak diubah.
pub fn order(v: &mut Value, keys: &[&str]) {
    if let Value::Object(m) = v {
        let mut old = std::mem::take(m);
        for k in keys {
            // `shift_remove`: `remove` pada `preserve_order` = `swap_remove` (mengacak urutan).
            if let Some(val) = old.shift_remove(*k) {
                m.insert((*k).to_owned(), val);
            }
        }
        for (k, val) in old {
            m.insert(k, val);
        }
    }
}

/// Teks `timestamp` jsonb Postgres → `2026-11-16T08:00:00.000Z`.
pub fn js_date(s: &str) -> Option<String> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|dt| JsDate(dt).to_js_string())
}

/// Ubah kolom tanggal bernama `keys` pada objek tingkat atas.
pub fn fix_dates(v: &mut Value, keys: &[&str]) {
    if let Value::Object(m) = v {
        for k in keys {
            if let Some(Value::String(s)) = m.get_mut(*k)
                && let Some(n) = js_date(s)
            {
                *s = n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn order_follows_prisma_field_order() {
        let mut v = json!({
            "id": 1, "uuid": "u", "name": "n", "emails": [], "isActive": true, "sendTime": "08:00",
            "createdAt": "x", "extra": 1
        });
        order(&mut v, SCHEDULED_REPORT_KEYS);
        let keys: Vec<&str> = v
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(
            keys,
            [
                "id",
                "uuid",
                "name",
                "sendTime",
                "emails",
                "isActive",
                "createdAt",
                "extra"
            ]
        );
    }

    #[test]
    fn normalises_timestamps() {
        let mut v = json!({
            "createdAt": "2026-11-16T08:00:00.5", "updatedAt": "2026-11-16T08:00:00",
            "lastSentAt": null, "name": "2026-11-16T08:00:00"
        });
        fix_dates(&mut v, &["createdAt", "updatedAt", "lastSentAt"]);
        assert_eq!(v["createdAt"], "2026-11-16T08:00:00.500Z");
        assert_eq!(v["updatedAt"], "2026-11-16T08:00:00.000Z");
        assert!(v["lastSentAt"].is_null());
        assert_eq!(v["name"], "2026-11-16T08:00:00");
    }
}
