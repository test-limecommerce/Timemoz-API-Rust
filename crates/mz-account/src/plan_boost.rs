//! Bagian `src/plan-limits/plan-boost.ts` yang dipakai `GET /auth/me`: `applyPlanBoosts`.

use serde_json::{Map, Value, json};

/// `SALES_BOOST_LIMITS`.
const SALES_LIMITS: [(&str, i64); 5] = [
    ("maxStores", 100),
    ("totalGeofence", 20),
    ("maxAutoCheckIns", 5000),
    ("maxFormTemplates", 20),
    ("visitHistoryDays", 365),
];

/// `raiseCount`: 0/negatif = tanpa batas, sudah lebih tinggi dari angka apa pun.
fn raise_count(current: i64, boost: i64) -> i64 {
    if current <= 0 {
        current
    } else {
        current.max(boost)
    }
}

/// `applyPlanBoosts(tier, products)`: `tier` = objek JSON entitlement paket; `products`
/// = `"SALES"` / `"TIME"` untuk boost yang menyala. SALES menaikkan batas & membuka gerbang;
/// TIME hanya membuka gerbang `divisions`.
pub fn apply_plan_boosts(tier: &mut Map<String, Value>, products: &[String]) {
    if products.iter().any(|p| p == "SALES") {
        for (field, boost) in SALES_LIMITS {
            let current = tier.get(field).and_then(Value::as_i64).unwrap_or(0);
            tier.insert(field.to_owned(), json!(raise_count(current, boost)));
        }
        for gate in [
            "auditLog",
            "divisions",
            "form",
            "timelineRoute",
            "product",
            "order",
        ] {
            tier.insert(gate.to_owned(), Value::Bool(true));
        }
    }
    if products.iter().any(|p| p == "TIME") {
        tier.insert("divisions".to_owned(), Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier() -> Map<String, Value> {
        let Value::Object(m) = json!({
            "maxStores": 10, "totalGeofence": 0, "maxAutoCheckIns": 9000, "maxFormTemplates": 1,
            "visitHistoryDays": 7, "auditLog": false, "divisions": false, "form": false,
            "timelineRoute": false, "product": false, "order": false
        }) else {
            return Map::new();
        };
        m
    }

    #[test]
    fn sales_boost_raises_but_never_lowers() {
        let mut t = tier();
        apply_plan_boosts(&mut t, &["SALES".to_owned()]);
        assert_eq!(t.get("maxStores"), Some(&json!(100)));
        assert_eq!(
            t.get("totalGeofence"),
            Some(&json!(0)),
            "0 = tanpa batas dibiarkan"
        );
        assert_eq!(t.get("maxAutoCheckIns"), Some(&json!(9000)));
        assert_eq!(t.get("visitHistoryDays"), Some(&json!(365)));
        assert_eq!(t.get("order"), Some(&json!(true)));
    }

    #[test]
    fn time_boost_only_opens_divisions() {
        let mut t = tier();
        apply_plan_boosts(&mut t, &["TIME".to_owned()]);
        assert_eq!(t.get("divisions"), Some(&json!(true)));
        assert_eq!(t.get("form"), Some(&json!(false)));
        assert_eq!(t.get("maxStores"), Some(&json!(10)));
    }
}
