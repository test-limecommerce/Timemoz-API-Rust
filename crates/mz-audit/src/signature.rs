//! Port `src/audit-logs/audit-soft-delete-signature.ts` (§3.3a): soft delete yang
//! sampai sebagai `update` dikenali sebagai `delete`/`restore`.

use serde_json::Value;

use crate::row::Row;
use crate::scope::{Action, EntityType};

/// `STATUS_DELETED` company/users/accessLevel — ketiganya 0.
const STATUS_DELETED: i64 = 0;

#[derive(Clone, Copy)]
enum Signature {
    /// `deletedAt`: null → terisi = hapus. `extra` = kolom wajib tambahan untuk restore.
    DeletedAt {
        extra: &'static [&'static str],
    },
    /// `isActive`: true → false = hapus.
    IsActive,
    /// `status` → nilai terhapus.
    Status(i64),
    Never,
}

fn signature_for(entity_type: EntityType) -> Signature {
    use EntityType as E;
    const WITH_ACTIVE: Signature = Signature::DeletedAt {
        extra: &["isActive"],
    };
    const PLAIN: Signature = Signature::DeletedAt { extra: &[] };
    match entity_type {
        E::Company | E::Employee | E::Role => Signature::Status(STATUS_DELETED),
        E::Store | E::StoreVisitForm | E::Customer | E::Product | E::PriceList => WITH_ACTIVE,
        E::StoreVisitPlan | E::PriceListItem | E::Division => PLAIN,
        E::Order => Signature::Never,
        E::StoreGroup
        | E::Geofence
        | E::NegativeZone
        | E::CustomerType
        | E::CustomerGroup
        | E::CustomerChannel
        | E::CustomerSubChannel
        | E::ProductCategory
        | E::ProductSubCategory
        | E::ProductBrand
        | E::ProductPrincipal
        | E::Uom
        | E::TaxRate
        | E::ProductCustomField
        | E::OrderType
        | E::OrderReason
        | E::OrderPaymentTerm
        | E::OrderPaymentMethod
        | E::OrderCustomFieldDefinition
        | E::VisitType
        | E::VisitFrequency => Signature::IsActive,
    }
}

impl Signature {
    fn markers(self) -> &'static [&'static str] {
        match self {
            Signature::DeletedAt { .. } => &["deletedAt"],
            Signature::IsActive => &["isActive"],
            Signature::Status(_) => &["status"],
            Signature::Never => &[],
        }
    }

    fn required(self) -> Vec<&'static str> {
        match self {
            Signature::DeletedAt { extra } => {
                let mut v = vec!["deletedAt"];
                v.extend_from_slice(extra);
                v
            }
            other => other.markers().to_vec(),
        }
    }

    fn is_delete(self, before: &Row, after: &Row) -> bool {
        match self {
            Signature::DeletedAt { .. } => {
                is_nullish(before.get("deletedAt")) && !is_nullish(after.get("deletedAt"))
            }
            Signature::IsActive => {
                before.get("isActive") == Some(&Value::Bool(true))
                    && after.get("isActive") == Some(&Value::Bool(false))
            }
            Signature::Status(deleted) => match (num(before), num(after)) {
                (Some(b), Some(a)) => b != deleted as f64 && a == deleted as f64,
                _ => false,
            },
            Signature::Never => false,
        }
    }

    fn is_restore(self, before: &Row, after: &Row) -> bool {
        match self {
            Signature::DeletedAt { .. } => {
                !is_nullish(before.get("deletedAt")) && is_nullish(after.get("deletedAt"))
            }
            Signature::IsActive => {
                before.get("isActive") == Some(&Value::Bool(false))
                    && after.get("isActive") == Some(&Value::Bool(true))
            }
            Signature::Status(deleted) => match (num(before), num(after)) {
                (Some(b), Some(a)) => b == deleted as f64 && a != deleted as f64,
                _ => false,
            },
            Signature::Never => false,
        }
    }
}

fn is_nullish(v: Option<&Value>) -> bool {
    v.is_none_or(Value::is_null)
}

/// `typeof row.status === 'number'`.
fn num(row: &Row) -> Option<f64> {
    row.get("status").and_then(Value::as_f64)
}

/// `detectTransition(entityType, before, after)`.
pub fn detect_transition(
    entity_type: EntityType,
    before: Option<&Row>,
    after: Option<&Row>,
) -> Option<Action> {
    let (before, after) = (before?, after?);
    let sig = signature_for(entity_type);
    if !sig
        .markers()
        .iter()
        .all(|f| before.contains(f) && after.contains(f))
    {
        return None;
    }
    if sig.is_delete(before, after) {
        Some(Action::Delete)
    } else if sig.is_restore(before, after) {
        Some(Action::Restore)
    } else {
        None
    }
}

/// `hasRequiredFields(entityType, before)` — syarat 4 §5.1.
pub fn has_required_fields(entity_type: EntityType, before: Option<&Row>) -> bool {
    let Some(before) = before else { return false };
    signature_for(entity_type)
        .required()
        .iter()
        .all(|f| before.contains(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(pairs: &[(&str, Value)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn company_status_to_deleted_is_delete_and_back_is_restore() {
        let live = r(&[("status", json!(1))]);
        let gone = r(&[("status", json!(0))]);
        assert_eq!(
            detect_transition(EntityType::Company, Some(&live), Some(&gone)),
            Some(Action::Delete)
        );
        assert_eq!(
            detect_transition(EntityType::Company, Some(&gone), Some(&live)),
            Some(Action::Restore)
        );
        // Bukan 3 (STATUS_DISABLED milik users).
        let three = r(&[("status", json!(3))]);
        assert_eq!(
            detect_transition(EntityType::Company, Some(&live), Some(&three)),
            None
        );
    }

    #[test]
    fn employee_signature_is_directional() {
        let invited = r(&[("status", json!(2))]);
        let active = r(&[("status", json!(1))]);
        let deleted = r(&[("status", json!(0))]);
        let disabled = r(&[("status", json!(3))]);
        assert_eq!(
            detect_transition(EntityType::Employee, Some(&invited), Some(&active)),
            None
        );
        assert_eq!(
            detect_transition(EntityType::Employee, Some(&active), Some(&deleted)),
            Some(Action::Delete)
        );
        assert_eq!(
            detect_transition(EntityType::Employee, Some(&deleted), Some(&active)),
            Some(Action::Restore)
        );
        assert_eq!(
            detect_transition(EntityType::Employee, Some(&active), Some(&disabled)),
            None
        );
    }

    #[test]
    fn store_deleted_at_decides() {
        let before = r(&[("deletedAt", Value::Null), ("isActive", json!(true))]);
        let off = r(&[("deletedAt", Value::Null), ("isActive", json!(false))]);
        let gone = r(&[
            ("deletedAt", json!("2026-08-19T00:00:00.000Z")),
            ("isActive", json!(false)),
        ]);
        assert_eq!(
            detect_transition(EntityType::Store, Some(&before), Some(&off)),
            None
        );
        assert_eq!(
            detect_transition(EntityType::Store, Some(&before), Some(&gone)),
            Some(Action::Delete)
        );
        assert_eq!(
            detect_transition(EntityType::StoreVisitForm, Some(&gone), Some(&before)),
            Some(Action::Restore)
        );
    }

    #[test]
    fn is_active_entities() {
        let on = r(&[("isActive", json!(true))]);
        let off = r(&[("isActive", json!(false))]);
        for e in [
            EntityType::StoreGroup,
            EntityType::Geofence,
            EntityType::NegativeZone,
            EntityType::CustomerType,
        ] {
            assert_eq!(
                detect_transition(e, Some(&on), Some(&off)),
                Some(Action::Delete)
            );
            assert_eq!(
                detect_transition(e, Some(&off), Some(&on)),
                Some(Action::Restore)
            );
        }
    }

    #[test]
    fn incomplete_snapshot_has_no_transition() {
        let gone = r(&[("deletedAt", json!("2026-08-19T00:00:00.000Z"))]);
        assert_eq!(
            detect_transition(EntityType::StoreVisitPlan, None, Some(&gone)),
            None
        );
        // Penanda yang tidak ikut terbaca tidak boleh lolos sebagai hapus.
        assert_eq!(
            detect_transition(EntityType::StoreVisitPlan, Some(&Row::new()), Some(&gone)),
            None
        );
        assert_eq!(
            detect_transition(
                EntityType::StoreVisitPlan,
                Some(&r(&[("deletedAt", Value::Null)])),
                Some(&gone)
            ),
            Some(Action::Delete)
        );
    }

    #[test]
    fn required_fields() {
        let only_deleted = r(&[("deletedAt", Value::Null)]);
        assert!(!has_required_fields(EntityType::Store, Some(&only_deleted)));
        assert!(has_required_fields(
            EntityType::Store,
            Some(&r(&[("deletedAt", Value::Null), ("isActive", json!(true))]))
        ));
        assert!(has_required_fields(
            EntityType::StoreVisitPlan,
            Some(&only_deleted)
        ));
        assert!(!has_required_fields(EntityType::StoreVisitPlan, None));
        assert!(has_required_fields(EntityType::Order, Some(&Row::new())));
    }
}
