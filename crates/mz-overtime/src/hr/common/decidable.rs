//! Port `src/overtime/overtime-decidable.ts`: predikat "bisa diputuskan" satu-satunya
//! (FX-5.1/5.2), dipakai tombol di daftar Menunggu (`canDecide`) DAN hitungan badge
//! `pendingApprovalCount` di `GET /status`, supaya keduanya tak bisa menyimpang.
//!
//! Cerminan penjaga `guardDecision`: SELF + PENDING_APPROVAL, bukan pengajuan sendiri
//! (kecuali approver tunggal), dan tenggat belum lewat.

use chrono::NaiveDateTime;
use sqlx::{Postgres, QueryBuilder};

use super::enums::{OvertimeRequestKind, OvertimeRequestStatus};

/// `isDecidable(row, approverId, soleApprover, now)`.
pub fn is_decidable(
    kind: OvertimeRequestKind,
    status: OvertimeRequestStatus,
    user_id: i32,
    response_deadline: NaiveDateTime,
    approver_id: i32,
    sole_approver: bool,
    now: NaiveDateTime,
) -> bool {
    kind == OvertimeRequestKind::SELF
        && status == OvertimeRequestStatus::PENDING_APPROVAL
        && (user_id != approver_id || sole_approver)
        && now < response_deadline
}

/// `decidableWhere(companyId, approverId, soleApprover, now)` sebagai potongan SQL yang
/// dipasang ke `qb` (nilai lewat `push_bind`). `alias` = prefiks kolom `overtimeRequest`
/// (mis. `"r."`, atau `""` tanpa alias). Tanpa kurung luar; pemanggil menambah `AND`.
pub fn push_decidable_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    alias: &str,
    company_id: i32,
    approver_id: i32,
    sole_approver: bool,
    now: NaiveDateTime,
) {
    qb.push(format!(r#"{alias}"companyId" = "#))
        .push_bind(company_id)
        .push(format!(
            r#" AND {alias}kind = 'SELF' AND {alias}status = 'PENDING_APPROVAL' AND {alias}"responseDeadline" > "#
        ))
        .push_bind(now);
    if !sole_approver {
        qb.push(format!(r#" AND {alias}"userId" <> "#))
            .push_bind(approver_id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn ts(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn decidable_rules() {
        let now = ts("2026-10-22T03:00:00Z");
        let future = ts("2026-10-22T04:00:00Z");
        let ok = |kind, status, user, deadline, sole| {
            is_decidable(kind, status, user, deadline, 9, sole, now)
        };
        use OvertimeRequestKind::*;
        use OvertimeRequestStatus::*;
        assert!(ok(SELF, PENDING_APPROVAL, 7, future, false));
        assert!(!ok(ORDER, PENDING_APPROVAL, 7, future, false));
        assert!(!ok(SELF, APPROVED, 7, future, false));
        assert!(!ok(SELF, PENDING_APPROVAL, 9, future, false));
        assert!(ok(SELF, PENDING_APPROVAL, 9, future, true));
        assert!(!ok(SELF, PENDING_APPROVAL, 7, now, false));
    }

    #[test]
    fn where_sql_shape() {
        let now = ts("2026-10-22T03:00:00Z");
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 WHERE ");
        push_decidable_where(&mut qb, "r.", 1, 9, false, now);
        assert_eq!(
            qb.sql(),
            r#"SELECT 1 WHERE r."companyId" = $1 AND r.kind = 'SELF' AND r.status = 'PENDING_APPROVAL' AND r."responseDeadline" > $2 AND r."userId" <> $3"#
        );
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_decidable_where(&mut qb, "", 1, 9, true, now);
        assert!(!qb.sql().contains("userId"));
    }
}
