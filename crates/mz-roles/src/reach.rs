//! Port `src/permissions/role-reach.ts` (`OUT_OF_REACH`, §15.7).
//!
//! Node menghitung `minReachableRank(feature, action)` saat runtime lewat refleksi seluruh graf
//! `@Module` (route inventory + `@Roles` + `@RequireFeature`/`@RequirePermission`). Rust tidak punya
//! graf itu, jadi hasilnya dibekukan di [`MIN_REACH`]: snapshot `tools/dump_node_registry.js`
//! (`tests/golden/reach.json`), dijaga test `golden_reach_matches_node`. Bila `@Roles` sebuah
//! endpoint Node berubah, jalankan ulang skripnya (lihat `plan/deviations/mz-roles.md`).

use mz_core::enums::{BaseRole, PermissionAction, rank_of};

/// `minReachableRank` per fitur: `[READ, WRITE, DELETE]`; `None` = tidak ada route yang
/// menggerbangi pasangan itu.
pub const MIN_REACH: &[(&str, [Option<u8>; 3])] = &[
    ("users", [Some(0), Some(2), Some(0)]),
    ("projects", [Some(0), Some(2), Some(2)]),
    ("roles", [Some(3), Some(3), Some(3)]),
    ("clock", [Some(1), Some(1), None]),
    ("geofences", [Some(1), Some(1), Some(3)]),
    ("negative-zones", [Some(1), Some(2), Some(2)]),
    ("kyc", [Some(1), Some(1), None]),
    ("leaves", [Some(1), Some(1), Some(1)]),
    ("leave-config", [Some(1), Some(2), Some(2)]),
    ("timesheets", [Some(0), Some(0), Some(0)]),
    ("screenshots", [Some(0), Some(0), Some(0)]),
    ("activity-logs", [Some(0), Some(0), None]),
    ("manual-time-approvals", [Some(0), Some(0), None]),
    ("manual-time-settings", [Some(2), Some(2), None]),
    ("divisions", [Some(0), Some(3), Some(3)]),
    ("overtime", [Some(1), Some(1), None]),
    ("overtime-report", [Some(2), None, None]),
    ("overtime-settings", [Some(3), Some(3), None]),
    ("stores", [Some(1), Some(1), Some(2)]),
    ("store-groups", [Some(2), Some(2), Some(2)]),
    ("store-visits", [Some(1), Some(1), None]),
    ("store-visit-plans", [Some(1), Some(2), Some(2)]),
    ("visit-masters", [Some(2), Some(2), Some(2)]),
    ("store-visit-forms", [Some(1), Some(1), Some(2)]),
    ("customers", [Some(2), Some(2), Some(2)]),
    ("customer-masters", [Some(2), Some(2), Some(2)]),
    ("products", [Some(1), Some(2), Some(2)]),
    ("product-masters", [Some(2), Some(2), Some(2)]),
    ("price-lists", [Some(2), Some(2), Some(2)]),
    ("order-masters", [Some(2), Some(2), Some(2)]),
    ("orders", [Some(1), Some(1), Some(2)]),
    ("order-approvals", [Some(2), Some(2), None]),
    ("order-deliveries", [Some(1), Some(1), None]),
    ("reports", [Some(0), Some(0), Some(1)]),
    ("audit-logs", [Some(2), Some(3), None]),
    ("billing", [Some(2), Some(3), None]),
    ("invoices", [Some(2), Some(2), None]),
    ("company-settings", [Some(0), Some(0), Some(3)]),
    ("integrations", [Some(0), Some(0), Some(2)]),
    ("calendar", [Some(0), Some(0), Some(0)]),
    ("table-fields", [Some(3), Some(3), Some(3)]),
    ("access-levels", [Some(4), Some(5), Some(5)]),
];

fn action_index(action: PermissionAction) -> usize {
    match action {
        PermissionAction::READ => 0,
        PermissionAction::WRITE => 1,
        PermissionAction::DELETE => 2,
    }
}

/// `minReachableRank(featureKey, action)`.
pub fn min_reachable_rank(feature_key: &str, action: PermissionAction) -> Option<u8> {
    MIN_REACH
        .iter()
        .find(|(k, _)| *k == feature_key)
        .and_then(|(_, ranks)| ranks.get(action_index(action)).copied().flatten())
}

fn base_rank(base: BaseRole) -> u8 {
    // `RANK[baseRole]` — BaseRole selalu salah satu dari tiga kode yang dikenal.
    rank_of(base.as_str()).unwrap_or(0)
}

/// `isOutOfReach(featureKey, action, baseRole)`.
pub fn is_out_of_reach(feature_key: &str, action: PermissionAction, base: BaseRole) -> bool {
    min_reachable_rank(feature_key, action).is_some_and(|min| min > base_rank(base))
}

/// `minBaseRoleFor`: `None` = tidak ada route; `Some("UNREACHABLE")` = di atas ADMIN.
pub fn min_base_role_for(feature_key: &str, action: PermissionAction) -> Option<&'static str> {
    let min = min_reachable_rank(feature_key, action)?;
    let found = [BaseRole::STAFF, BaseRole::MANAGER, BaseRole::ADMIN]
        .into_iter()
        .find(|b| base_rank(*b) >= min);
    Some(found.map_or("UNREACHABLE", |b| b.as_str()))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use mz_core::feature_registry::FEATURES;

    #[test]
    fn golden_reach_matches_node() {
        let node: Vec<(String, String, Option<u8>)> =
            serde_json::from_str(include_str!("../tests/golden/reach.json")).unwrap_or_default();
        assert_eq!(node.len(), FEATURES.len() * 3);
        for (key, action, rank) in node {
            let action: PermissionAction = action.parse().unwrap_or(PermissionAction::READ);
            assert_eq!(min_reachable_rank(&key, action), rank, "{key}:{action}");
        }
        let keys: Vec<&str> = MIN_REACH.iter().map(|(k, _)| *k).collect();
        let registry: Vec<&str> = FEATURES.iter().map(|f| f.key).collect();
        assert_eq!(keys, registry);
    }

    #[test]
    fn reach_semantics() {
        assert!(is_out_of_reach(
            "roles",
            PermissionAction::READ,
            BaseRole::MANAGER
        ));
        assert!(!is_out_of_reach(
            "roles",
            PermissionAction::READ,
            BaseRole::ADMIN
        ));
        assert!(!is_out_of_reach(
            "clock",
            PermissionAction::DELETE,
            BaseRole::STAFF
        ));
        assert_eq!(
            min_base_role_for("roles", PermissionAction::WRITE),
            Some("ADMIN")
        );
        assert_eq!(
            min_base_role_for("users", PermissionAction::READ),
            Some("STAFF")
        );
        assert_eq!(
            min_base_role_for("access-levels", PermissionAction::READ),
            Some("UNREACHABLE")
        );
        assert_eq!(min_base_role_for("clock", PermissionAction::DELETE), None);
    }
}
