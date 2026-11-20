//! Pembacaan kebijakan cuti yang dipakai lintas modul (`leave-policies.service.ts`:
//! `isCarryOverExpired`, `getPolicyApprovers`, `propagateSickTotal`,
//! `applySickLeaveFromLeaveType`) + `leave-visibility.ts`.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use mz_overtime::zone::{local_day_start_ms, ms_of};
use serde::Serialize;
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::enums::LeaveSickMode;
use crate::models::{LeavePolicy, LeaveTier, POLICY_COLUMNS, TIER_COLUMNS};

/// Zona company seperti `moment.tz(x, company.timeZone)`: nama tak dikenal → UTC.
pub fn zone(time_zone: &str) -> Tz {
    mz_core::tz::zone_or_utc(Some(time_zone))
}

/// `moment.tz(now, timeZone)` sebagai jam dinding lokal.
pub fn local_now(now: NaiveDateTime, time_zone: &str) -> NaiveDateTime {
    use chrono::TimeZone;
    zone(time_zone).from_utc_datetime(&now).naive_local()
}

/// `moment.tz(now, timeZone).year()`.
pub fn local_year(now: NaiveDateTime, time_zone: &str) -> i32 {
    local_now(now, time_zone).year()
}

/// Awal hari lokal `date` di zona company (epoch ms).
pub fn local_day_start(date: NaiveDate, time_zone: &str) -> i64 {
    local_day_start_ms(date, zone(time_zone))
}

/// `isCarryOverExpired(policy, year, timeZone, now)`: sudah lewat tanggal 1 bulan
/// `carryOverExpiryMonth` tahun `year` (tengah malam zona company). `timeZone` dipakai apa
/// adanya (tanpa fallback Asia/Jakarta: 'UTC' pilihan yang sah).
pub fn is_carry_over_expired(
    carry_over_expiry_month: Option<i32>,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> bool {
    let Some(month) = carry_over_expiry_month.filter(|m| *m != 0) else {
        return false;
    };
    // Bulan di luar 1..12 → moment overflow ke tahun berikutnya/sebelumnya.
    let m0 = month - 1;
    let y = year + m0.div_euclid(12);
    let m = u32::try_from(m0.rem_euclid(12) + 1).unwrap_or(1);
    let Some(first) = NaiveDate::from_ymd_opt(y, m, 1) else {
        return false;
    };
    ms_of(now) >= local_day_start(first, time_zone)
}

/// `isCarryOverExpired` dengan `Option<&LeavePolicy>`.
pub fn policy_carry_over_expired(
    policy: Option<&LeavePolicy>,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> bool {
    is_carry_over_expired(
        policy.and_then(|p| p.carry_over_expiry_month),
        year,
        time_zone,
        now,
    )
}

/// `leavePolicy.findUnique({ where: { companyId } })`.
pub async fn load_policy(
    exec: impl PgExecutor<'_>,
    company_id: i32,
) -> sqlx::Result<Option<LeavePolicy>> {
    sqlx::query_as(&format!(
        r#"SELECT {POLICY_COLUMNS} FROM "leavePolicy" p WHERE p."companyId" = $1"#
    ))
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

/// `company.timeZone` (`None` = company tidak ada).
pub async fn company_time_zone(
    exec: impl PgExecutor<'_>,
    company_id: i32,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(exec)
        .await
}

/// `company.findUniqueOrThrow({ select: { timeZone } })`: company hilang → `RowNotFound`.
pub async fn require_time_zone(exec: impl PgExecutor<'_>, company_id: i32) -> sqlx::Result<String> {
    company_time_zone(exec, company_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// Override + tier karyawan (`employeeLeavePolicy`, `employeeLeaveTier` + `tier`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntitlementInputs {
    /// `employeeLeavePolicy.annualLeaveDays` per user.
    pub override_days: HashMap<i32, i32>,
    /// Tier terpasang per user.
    pub tiers: HashMap<i32, LeaveTier>,
}

impl EntitlementInputs {
    pub fn override_of(&self, user_id: i32) -> Option<i32> {
        self.override_days.get(&user_id).copied()
    }

    pub fn tier_days_of(&self, user_id: i32) -> Option<i32> {
        self.tiers.get(&user_id).map(|t| t.annual_leave_days)
    }
}

/// Dimuat sekali per panggilan, bukan per karyawan (`findMany ... userId in`).
pub async fn load_entitlement_inputs(
    conn: &mut PgConnection,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<EntitlementInputs> {
    let overrides: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT "userId", "annualLeaveDays" FROM "employeeLeavePolicy"
           WHERE "companyId" = $1 AND "userId" = ANY($2)"#,
    )
    .bind(company_id)
    .bind(user_ids)
    .fetch_all(&mut *conn)
    .await?;
    let tiers = load_tiers(&mut *conn, company_id, user_ids).await?;
    Ok(EntitlementInputs {
        override_days: overrides.into_iter().collect(),
        tiers,
    })
}

#[derive(sqlx::FromRow)]
struct TierAssignment {
    assigned_user_id: i32,
    #[sqlx(flatten)]
    tier: LeaveTier,
}

async fn load_tiers(
    conn: &mut PgConnection,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, LeaveTier>> {
    let rows: Vec<TierAssignment> = sqlx::query_as(&format!(
        r#"SELECT e."userId" AS assigned_user_id, {TIER_COLUMNS}
           FROM "employeeLeaveTier" e JOIN "leaveTier" t ON t.id = e."tierId"
           WHERE e."companyId" = $1 AND e."userId" = ANY($2)"#
    ))
    .bind(company_id)
    .bind(user_ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.assigned_user_id, r.tier))
        .collect())
}

/// Override + tier satu karyawan (`findUnique` per `userId_companyId`).
pub async fn load_entitlement_of(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<(Option<i32>, Option<LeaveTier>)> {
    let mut inputs = load_entitlement_inputs(conn, company_id, &[user_id]).await?;
    Ok((inputs.override_of(user_id), inputs.tiers.remove(&user_id)))
}

/// `{ id, name, username }` user approver kebijakan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct PolicyApproverUser {
    pub id: i32,
    pub name: String,
    pub username: String,
}

/// `getPolicyApprovers(companyId)`: user approver kebijakan, urut nama naik. Tanpa policy → `[]`.
pub async fn get_policy_approvers(
    exec: impl PgExecutor<'_>,
    company_id: i32,
) -> sqlx::Result<Vec<PolicyApproverUser>> {
    sqlx::query_as(
        r#"SELECT u.id, u.name, u.username
           FROM "leavePolicy" p
           JOIN "leavePolicyApprover" a ON a."policyId" = p.id
           JOIN users u ON u.id = a."userId"
           WHERE p."companyId" = $1
           ORDER BY u.name ASC"#,
    )
    .bind(company_id)
    .fetch_all(exec)
    .await
}

/// `isPolicyApprover(prisma, userId, companyId)` (`leave-visibility.ts`).
pub async fn is_policy_approver(
    exec: impl PgExecutor<'_>,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "leavePolicyApprover"
                          WHERE "userId" = $1 AND "companyId" = $2)"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_one(exec)
    .await
}

/// `approverSql(alias, userId, policyApprover)` / `approverWhere`: hanya approver yang
/// ditugaskan; approver kebijakan juga melihat pengajuan tanpa approver (data lama).
/// `alias` = alias tabel `leaveRequest` (mis. `lr`, `"leaveRequest"`).
pub fn push_approver_sql(
    qb: &mut QueryBuilder<'_, Postgres>,
    alias: &str,
    user_id: i32,
    policy_approver: bool,
) {
    let col = format!(r#"{alias}."approverId""#);
    if policy_approver {
        qb.push(format!("({col} = "))
            .push_bind(user_id)
            .push(format!(" OR {col} IS NULL)"));
    } else {
        qb.push(format!("{col} = ")).push_bind(user_id);
    }
}

/// `propagateSickTotal(tx, companyId, days)`: `sickLeaveTotal` saldo tahun berjalan (zona
/// company) disamakan; baris yang pernah di-adjust manual (`field = 'sickLeaveTotal'`) dilewati.
/// Kuota di bawah pemakaian di-clamp per baris (`GREATEST(days, sickLeaveUsed)`) supaya sisa
/// tak pernah negatif dan satu karyawan tak menggagalkan penyimpanan kebijakan.
pub async fn propagate_sick_total(
    conn: &mut PgConnection,
    company_id: i32,
    days: i32,
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    let tz = require_time_zone(&mut *conn, company_id).await?;
    let year = local_year(now, &tz);
    let done = sqlx::query(
        r#"UPDATE "leaveBalance" b
              SET "sickLeaveTotal" = GREATEST($3::double precision, b."sickLeaveUsed"), "updatedAt" = $4
           WHERE b."companyId" = $1 AND b.year = $2
             AND NOT EXISTS (SELECT 1 FROM "leaveBalanceAdjustment" a
                             WHERE a."balanceId" = b.id AND a.field = 'sickLeaveTotal')"#,
    )
    .bind(company_id)
    .bind(year)
    .bind(f64::from(days))
    .bind(now)
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

/// `applySickLeaveFromLeaveType(tx, companyId, annualQuota)`: kuota jenis cuti bersumber
/// SICK diubah → `null` = UNLIMITED, angka = LIMITED dengan jatah itu (upsert `leavePolicy`).
/// Pakai koneksi transaksi pemanggil agar atomik dengan update jenis cuti.
pub async fn apply_sick_leave_from_leave_type(
    conn: &mut PgConnection,
    company_id: i32,
    annual_quota: Option<i32>,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    let mode = if annual_quota.is_none() {
        LeaveSickMode::UNLIMITED
    } else {
        LeaveSickMode::LIMITED
    };
    sqlx::query(
        r#"INSERT INTO "leavePolicy" ("companyId", "sickLeaveMode", "sickLeaveDays")
           VALUES ($1, $2::"LeaveSickMode", $3)
           ON CONFLICT ("companyId") DO UPDATE
             SET "sickLeaveMode" = EXCLUDED."sickLeaveMode",
                 "sickLeaveDays" = EXCLUDED."sickLeaveDays",
                 "updatedAt" = $4"#,
    )
    .bind(company_id)
    .bind(mode.as_str())
    .bind(annual_quota)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if let Some(days) = annual_quota {
        propagate_sick_total(conn, company_id, days, now).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    // leave-year.service.spec.ts › expireCarryOver (batas tanggal 1, Asia/Jakarta)
    #[test]
    fn carry_over_expiry_boundary() {
        let jkt = "Asia/Jakarta";
        assert!(!is_carry_over_expired(
            Some(3),
            2026,
            jkt,
            at("2026-02-28T16:59:00Z")
        ));
        assert!(is_carry_over_expired(
            Some(3),
            2026,
            jkt,
            at("2026-02-28T17:00:00Z")
        ));
        assert!(!is_carry_over_expired(
            Some(3),
            2026,
            "UTC",
            at("2026-02-28T17:00:00Z")
        ));
        assert!(!is_carry_over_expired(
            None,
            2026,
            jkt,
            at("2026-12-31T00:00:00Z")
        ));
        // Tahun saldo lampau: sudah lama kedaluwarsa.
        assert!(is_carry_over_expired(
            Some(3),
            2025,
            jkt,
            at("2026-01-01T00:00:00Z")
        ));
        // Zona tak dikenal → UTC.
        assert!(is_carry_over_expired(
            Some(3),
            2026,
            "Bogus/Zone",
            at("2026-03-01T00:00:00Z")
        ));
        // moment overflow bulan 13 → Januari tahun berikutnya.
        assert!(!is_carry_over_expired(
            Some(13),
            2026,
            "UTC",
            at("2026-12-31T00:00:00Z")
        ));
        assert!(is_carry_over_expired(
            Some(13),
            2026,
            "UTC",
            at("2027-01-01T00:00:00Z")
        ));
    }

    #[test]
    fn local_year_follows_zone() {
        assert_eq!(local_year(at("2025-12-31T17:00:00Z"), "Asia/Jakarta"), 2026);
        assert_eq!(local_year(at("2025-12-31T17:00:00Z"), "UTC"), 2025);
    }

    #[test]
    fn approver_sql() {
        let mut qb = QueryBuilder::<Postgres>::new("WHERE ");
        push_approver_sql(&mut qb, "lr1", 7, true);
        assert_eq!(
            qb.sql(),
            r#"WHERE (lr1."approverId" = $1 OR lr1."approverId" IS NULL)"#
        );
        let mut qb = QueryBuilder::<Postgres>::new("WHERE ");
        push_approver_sql(&mut qb, "lr", 7, false);
        assert_eq!(qb.sql(), r#"WHERE lr."approverId" = $1"#);
    }
}
