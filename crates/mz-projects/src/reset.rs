//! Port `ProjectBudgetService.runDailyReset` + `resetBudget` (Node:
//! `src/project-budget/project-budget.service.ts:543-579`) dan cron hariannya
//! (`project-budget.cron.ts:13`). Kueri & kolom reset identik dengan Node
//! (termasuk `currentBurnedCost = 0` dan syarat jam-ATAU-biaya, diperbaiki di kedua sisi 2026-11-17).

use chrono::{DateTime, Datelike, Utc, Weekday};
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_jobs::CRON_TZ;

use crate::budget::{BUDGET_SELECT, BudgetRow};

/// Siklus yang direset pada hari tertentu (`resetCycle` di Prisma).
pub const WEEKLY: &str = "WEEKLY";
pub const MONTHLY: &str = "MONTHLY";

/// Logika "Senin" dan "tanggal 1" dihitung menurut WIB (zona cron), bukan
/// zona proses: 00:05 WIB Senin masih Minggu di UTC. Kedua siklus bisa jatuh
/// pada hari yang sama (mis. 1 Juni Senin) dan keduanya dijalankan.
pub fn due_cycles(now: DateTime<Utc>) -> Vec<&'static str> {
    let local = now.with_timezone(&CRON_TZ);
    let mut due = Vec::new();
    if local.weekday() == Weekday::Mon {
        due.push(WEEKLY);
    }
    if local.day() == 1 {
        due.push(MONTHLY);
    }
    due
}

/// `runDailyReset`: dipanggil cron 00:05 WIB.
pub async fn run_daily_reset(state: AppState) -> anyhow::Result<()> {
    for cycle in due_cycles(Utc::now()) {
        reset_cycle(&state, cycle).await?;
    }
    Ok(())
}

/// Reset semua budget aktif (`isEnabled`, `budgetHours > 0` ATAU `budgetAmount > 0`;
/// budget COST boleh tanpa batas jam, sama dengan syarat `recalculateBurn`) dengan `resetCycle`
/// tertentu. Galat per budget ditelan + log, seperti satu item gagal tidak
/// menghentikan yang lain. Galat kueri pencarian dikembalikan ke pemanggil.
pub async fn reset_cycle(state: &AppState, cycle: &str) -> anyhow::Result<()> {
    let sql = format!(
        r#"{BUDGET_SELECT} WHERE b."isEnabled" = true AND b."resetCycle"::text = $1
           AND (b."budgetHours" > 0 OR COALESCE(b."budgetAmount", 0) > 0)"#
    );
    let budgets = sqlx::query_as::<_, BudgetRow>(&sql)
        .bind(cycle)
        .fetch_all(&state.db)
        .await?;
    for budget in budgets {
        if let Err(e) = reset_budget(state, budget.id).await {
            tracing::error!(
                budget_id = budget.id,
                project_id = budget.project_id,
                cycle,
                error = %e,
                "reset budget gagal"
            );
        }
    }
    Ok(())
}

/// `resetBudget`: nol-kan jam & biaya terbakar dan siklus, bersihkan penanda alert.
pub async fn reset_budget(state: &AppState, budget_id: i32) -> Result<(), sqlx::Error> {
    let now = now_naive();
    sqlx::query(
        r#"UPDATE "projectBudget" SET "currentBurnedSeconds" = 0, "currentBurnedCost" = 0, "cycleStartAt" = $2,
           "lastResetAt" = $2, "thresholdAlertSentAt" = NULL, "budgetReachedSentAt" = NULL,
           "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(budget_id)
    .bind(now)
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap_or_default()
    }

    #[test]
    fn monday_wib_is_weekly_even_when_utc_is_sunday() {
        // 2026-11-16 00:05 WIB (Senin) = 2026-11-15 17:05 UTC (Minggu).
        assert_eq!(due_cycles(utc(2026, 11, 15, 17, 5)), vec![WEEKLY]);
    }

    #[test]
    fn utc_monday_evening_is_already_tuesday_wib() {
        // 2026-11-16 20:00 UTC = 2026-11-17 03:00 WIB (Selasa): bukan WEEKLY.
        assert!(due_cycles(utc(2026, 11, 16, 20, 0)).is_empty());
    }

    #[test]
    fn first_of_month_wib_is_monthly() {
        // 2026-12-01 00:05 WIB = 2026-11-30 17:05 UTC (masih tanggal 30 di UTC).
        assert_eq!(due_cycles(utc(2026, 11, 30, 17, 5)), vec![MONTHLY]);
    }

    #[test]
    fn first_of_month_on_monday_runs_both() {
        // 2026-06-01 adalah Senin. 00:05 WIB = 2026-05-31 17:05 UTC (Minggu, tgl 31).
        assert_eq!(due_cycles(utc(2026, 5, 31, 17, 5)), vec![WEEKLY, MONTHLY]);
    }

    #[test]
    fn ordinary_day_has_no_reset() {
        // Selasa 17 Nov 2026 00:05 WIB = 16 Nov 17:05 UTC.
        assert!(due_cycles(utc(2026, 11, 16, 17, 5)).is_empty());
    }
}
