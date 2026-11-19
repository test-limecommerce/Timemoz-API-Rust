//! Uji integrasi domain cuti terhadap Postgres nyata: saldo (bawa/hangus/pro-rata), tahun
//! cuti, akrual, peralihan kebijakan, potongan libur, hitung ulang, cadangan cuti bersama.
//! Kasus mengikuti `*.spec.ts` Node (leave-year, leave-accrual, holiday-deduction).
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-leave --test it_leave_core`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use mz_leave::balance::{find_balance, get_or_create_balance, lock_balance};
use mz_leave::holiday_deduction::{
    DeductionRun, apply_due, recalc_approved_for_dates, reconcile_holiday,
};
use mz_leave::schedule::{
    accrual_catch_up, ensure_year_open, expire_carry_over, top_up_to_upfront,
};
use mz_leave::view::{BalanceViewCtx, get_scheduled_holiday_deductions, to_balance_view};
use mz_testkit::TestApp;

const JKT: &str = "Asia/Jakarta";
/// Jumat 2 Okt 2026, 12:00 WIB (holiday-deduction.service.spec.ts `NOW`).
const NOW: &str = "2026-10-02T05:00:00Z";

async fn app() -> Option<TestApp> {
    TestApp::new(no_routes).await
}

// ── leave-year.service.spec.ts › carry-over and forfeit when a year opens ────

#[tokio::test]
async fn year_open_carries_over_and_forfeits() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 4, "2024-01-01T00:00:00").await;
    policy(
        &app,
        co.id,
        r#""carryOverEnabled", "maxCarryOverDays""#,
        "true, 6",
    )
    .await;
    let [a, b, c, e] = [co.users[0], co.users[1], co.users[2], co.users[3]];
    let prev_a = balance(&app, co.id, a, 2025, 12.0, 3.0, 0.0, 0.0).await;
    balance(&app, co.id, b, 2025, 12.0, 8.0, 0.0, 0.0).await;
    let prev_c = balance(&app, co.id, c, 2025, 12.0, 12.0, 2.0, 0.0).await;

    // cap 6 dengan 9 sisa → +6 bawa, −3 hangus.
    let created = get_or_create_balance(app.db(), a, co.id, 2026)
        .await
        .unwrap();
    assert_eq!(
        (created.carry_over_days, created.annual_leave_total),
        (6.0, 12.0)
    );
    let adj = adjustments(&app, a).await;
    assert_eq!(adj.len(), 2);
    assert_eq!(
        (adj[0].0.as_str(), adj[0].1, adj[0].2.as_str()),
        ("carryOverDays", 6.0, "CARRY_OVER")
    );
    assert_eq!(
        adj[0].3.as_deref(),
        Some(format!("carry:{}", created.id).as_str())
    );
    assert_eq!(adj[0].5, "Sisa cuti dibawa dari 2025");
    assert_eq!(
        (adj[1].0.as_str(), adj[1].1, adj[1].2.as_str()),
        ("annualLeaveTotal", -3.0, "FORFEIT")
    );
    assert_eq!(
        adj[1].3.as_deref(),
        Some(format!("forfeit:{prev_a}").as_str())
    );
    assert_eq!(adj[1].5, "Sisa cuti hangus (melebihi batas bawa 6 hari)");
    let forfeit_balance: i32 = app
        .scalar(&format!(r#"SELECT "balanceId" FROM "leaveBalanceAdjustment" WHERE "dedupeKey" = 'forfeit:{prev_a}'"#))
        .await;
    assert_eq!(forfeit_balance, prev_a, "hangus dicatat di saldo Y-1");
    // Total Y-1 tidak dikurangi.
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "annualLeaveTotal" FROM "leaveBalance" WHERE id = {prev_a}"#)
        )
        .await,
        12.0
    );
    // Ulang: baris sama, tanpa riwayat baru.
    let again = get_or_create_balance(app.db(), a, co.id, 2026)
        .await
        .unwrap();
    assert_eq!(again.id, created.id);
    assert_eq!(adjustments(&app, a).await.len(), 2);

    // 4 sisa → +4 bawa, tanpa hangus.
    let created = get_or_create_balance(app.db(), b, co.id, 2026)
        .await
        .unwrap();
    assert_eq!(created.carry_over_days, 4.0);
    let adj = adjustments(&app, b).await;
    assert_eq!(
        adj.iter().map(|a| a.2.as_str()).collect::<Vec<_>>(),
        vec!["CARRY_OVER"]
    );

    // Sisa bawaan Y-1 yang tak terpakai ikut hangus.
    get_or_create_balance(app.db(), c, co.id, 2026)
        .await
        .unwrap();
    let adj = adjustments(&app, c).await;
    assert_eq!(adj.len(), 1);
    assert_eq!((adj[0].1, adj[0].2.as_str()), (-2.0, "FORFEIT"));
    assert_eq!(
        adj[0].3.as_deref(),
        Some(format!("forfeit:{prev_c}").as_str())
    );

    // Tanpa tahun sebelumnya: tanpa riwayat.
    get_or_create_balance(app.db(), e, co.id, 2026)
        .await
        .unwrap();
    assert!(adjustments(&app, e).await.is_empty());
}

#[tokio::test]
async fn year_open_without_carry_over_forfeits_everything() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 1, "2024-01-01T00:00:00").await;
    policy(&app, co.id, "", "").await;
    balance(&app, co.id, co.user(), 2025, 12.0, 3.0, 0.0, 0.0).await;
    let created = get_or_create_balance(app.db(), co.user(), co.id, 2026)
        .await
        .unwrap();
    assert_eq!(created.carry_over_days, 0.0);
    let adj = adjustments(&app, co.user()).await;
    assert_eq!(adj.len(), 1);
    assert_eq!(
        (adj[0].1, adj[0].5.as_str()),
        (-9.0, "Sisa cuti hangus (bawa sisa cuti tidak aktif)")
    );
}

#[tokio::test]
async fn new_balance_pro_rata_monthly_tier_and_sick() {
    let Some(app) = app().await else { return };
    // Bergabung 1 Okt 2026 00:30 WIB (= 30 Sep 17:30Z): bulan lokal Oktober (0-based 9).
    let co = company(&app, JKT, 1, "2026-09-30T17:30:00").await;
    policy(
        &app,
        co.id,
        r#""sickLeaveMode", "sickLeaveDays""#,
        "'LIMITED', 5",
    )
    .await;
    let b = get_or_create_balance(app.db(), co.user(), co.id, 2026)
        .await
        .unwrap();
    assert_eq!(
        (
            b.annual_leave_total,
            b.accrued_through_month,
            b.sick_leave_total
        ),
        (3.0, 0, 5.0)
    );
    // Tahun lain: jatah penuh.
    let b = get_or_create_balance(app.db(), co.user(), co.id, 2027)
        .await
        .unwrap();
    assert_eq!(b.annual_leave_total, 12.0);

    // MONTHLY: total 0, penanda = bulan bergabung.
    let co = company(&app, JKT, 1, "2026-09-30T17:30:00").await;
    policy(
        &app,
        co.id,
        r#""accrualMode", "monthlyAccrualDays""#,
        "'MONTHLY', 1",
    )
    .await;
    let b = get_or_create_balance(app.db(), co.user(), co.id, 2026)
        .await
        .unwrap();
    assert_eq!((b.annual_leave_total, b.accrued_through_month), (0.0, 9));

    // TIER_BASED + tier terpasang, ANNIVERSARY → tanpa pro-rata.
    let co = company(&app, JKT, 1, "2026-09-30T17:30:00").await;
    let pid = policy(
        &app,
        co.id,
        r#""entitlementType", "resetType""#,
        "'TIER_BASED', 'ANNIVERSARY'",
    )
    .await;
    let tier: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveTier" ("companyId", "policyId", name, "annualLeaveDays") VALUES ({}, {pid}, 'Senior', 18) RETURNING id"#,
            co.id
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "employeeLeaveTier" ("userId", "companyId", "tierId") VALUES ({}, {}, {tier})"#,
        co.user(),
        co.id
    ))
    .await;
    let b = get_or_create_balance(app.db(), co.user(), co.id, 2026)
        .await
        .unwrap();
    assert_eq!(b.annual_leave_total, 18.0);

    // Saldo tidak ada → lock_balance RowNotFound.
    let mut tx = app.db().begin().await.unwrap();
    assert!(matches!(
        lock_balance(&mut tx, -1).await,
        Err(sqlx::Error::RowNotFound)
    ));
}

#[tokio::test]
async fn ensure_year_open_creates_missing_balances() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 2, "2025-01-01T00:00:00").await;
    policy(&app, co.id, "", "").await;
    balance(&app, co.id, co.users[0], 2026, 12.0, 0.0, 0.0, 0.0).await;
    // 1 Jan 2026 07:00 WIB.
    let created = ensure_year_open(app.db(), co.id, JKT, at("2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(created, 1);
    assert!(
        find_balance(app.db(), co.users[1], co.id, 2026)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        ensure_year_open(app.db(), co.id, JKT, at("2026-01-01T00:00:00Z"))
            .await
            .unwrap(),
        0
    );
    // 31 Des 2025 23:00 UTC = 2026 di Jakarta, 2025 di UTC.
    assert_eq!(
        ensure_year_open(app.db(), co.id, "UTC", at("2025-12-31T23:00:00Z"))
            .await
            .unwrap(),
        2
    );
}

// ── leave-year.service.spec.ts › expireCarryOver ─────────────────────────────

#[tokio::test]
async fn expire_carry_over_on_first_of_month() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 2, "2025-01-01T00:00:00").await;
    policy(&app, co.id, r#""carryOverExpiryMonth""#, "3").await;
    let id = balance(&app, co.id, co.users[0], 2026, 12.0, 0.0, 5.0, 2.0).await;
    // Sudah selesai (carryOverDays == used): bukan kandidat.
    balance(&app, co.id, co.users[1], 2026, 12.0, 0.0, 2.0, 2.0).await;

    // 28 Feb 23:59 WIB: belum.
    assert_eq!(
        expire_carry_over(app.db(), co.id, JKT, at("2026-02-28T16:59:00Z"))
            .await
            .unwrap(),
        0
    );
    // 1 Mar 00:00 WIB.
    assert_eq!(
        expire_carry_over(app.db(), co.id, JKT, at("2026-02-28T17:00:00Z"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "carryOverDays" FROM "leaveBalance" WHERE id = {id}"#)
        )
        .await,
        2.0
    );
    let adj = adjustments(&app, co.users[0]).await;
    assert_eq!(adj.len(), 1);
    assert_eq!(
        (adj[0].0.as_str(), adj[0].1, adj[0].2.as_str()),
        ("carryOverDays", -3.0, "CARRY_OVER_EXPIRY")
    );
    assert_eq!(
        adj[0].3.as_deref(),
        Some(format!("co-expiry:{id}").as_str())
    );
    assert_eq!(adj[0].5, "Cuti bawaan kedaluwarsa");
    // Ulang: tak ada kandidat lagi.
    assert_eq!(
        expire_carry_over(app.db(), co.id, JKT, at("2026-04-01T00:00:00Z"))
            .await
            .unwrap(),
        0
    );
    // dedupeKey ganda ditelan (baris dibuka lagi secara manual).
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "carryOverDays" = 4 WHERE id = {id}"#
    ))
    .await;
    assert_eq!(
        expire_carry_over(app.db(), co.id, JKT, at("2026-04-01T00:00:00Z"))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "carryOverDays" FROM "leaveBalance" WHERE id = {id}"#)
        )
        .await,
        4.0
    );

    // Tanpa bulan kedaluwarsa: no-op.
    let co2 = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    policy(&app, co2.id, "", "").await;
    balance(&app, co2.id, co2.user(), 2026, 12.0, 0.0, 5.0, 0.0).await;
    assert_eq!(
        expire_carry_over(app.db(), co2.id, JKT, at("2026-12-31T00:00:00Z"))
            .await
            .unwrap(),
        0
    );
}

// ── leave-accrual.service.spec.ts ────────────────────────────────────────────

async fn monthly_company(app: &TestApp, tz: &str, joined: &str) -> Co {
    let co = company(app, tz, 1, joined).await;
    policy(
        app,
        co.id,
        r#""accrualMode", "monthlyAccrualDays""#,
        "'MONTHLY', 1",
    )
    .await;
    co
}

async fn accrual_state(app: &TestApp, id: i32) -> (f64, i32) {
    sqlx::query_as(
        r#"SELECT "annualLeaveTotal", "accruedThroughMonth" FROM "leaveBalance" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn accrual_credits_completed_months_once() {
    let Some(app) = app().await else { return };
    let co = monthly_company(&app, JKT, "2025-01-01T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 0.0, 0.0, 0.0, 0.0).await;
    let now = at("2026-04-10T05:00:00Z");
    assert_eq!(
        accrual_catch_up(app.db(), co.id, JKT, now).await.unwrap(),
        3
    );
    assert_eq!(accrual_state(&app, id).await, (3.0, 3));
    let adj = adjustments(&app, co.user()).await;
    let keys: Vec<_> = adj
        .iter()
        .map(|a| a.3.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        keys,
        vec![
            format!("accrual:{id}:2026-01"),
            format!("accrual:{id}:2026-02"),
            format!("accrual:{id}:2026-03")
        ]
    );
    assert_eq!(
        (adj[0].1, adj[0].2.as_str(), adj[0].5.as_str()),
        (1.0, "MONTHLY_ACCRUAL", "Akrual bulanan — Januari 2026")
    );
    // Ulang dengan keadaan sama: no-op.
    assert_eq!(
        accrual_catch_up(app.db(), co.id, JKT, now).await.unwrap(),
        0
    );
    assert_eq!(accrual_state(&app, id).await, (3.0, 3));
}

#[tokio::test]
async fn accrual_caps_at_entitlement_and_respects_join_date() {
    let Some(app) = app().await else { return };
    // 11.5 → 12 → tidak lebih; penanda tetap maju walau kredit 0.
    let co = monthly_company(&app, JKT, "2025-01-01T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 11.5, 0.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "accruedThroughMonth" = 5 WHERE id = {id}"#
    ))
    .await;
    accrual_catch_up(app.db(), co.id, JKT, at("2026-09-10T05:00:00Z"))
        .await
        .unwrap();
    assert_eq!(accrual_state(&app, id).await, (12.0, 8));
    let adj = adjustments(&app, co.user()).await;
    assert_eq!(adj.len(), 1);
    assert_eq!(adj[0].1, 0.5);

    // Bulan yang berakhir sebelum bergabung: maju tanpa kredit.
    let co = monthly_company(&app, JKT, "2026-03-15T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 0.0, 0.0, 0.0, 0.0).await;
    accrual_catch_up(app.db(), co.id, JKT, at("2026-04-10T05:00:00Z"))
        .await
        .unwrap();
    let keys: Vec<_> = adjustments(&app, co.user())
        .await
        .into_iter()
        .map(|a| a.3.unwrap_or_default())
        .collect();
    assert_eq!(keys, vec![format!("accrual:{id}:2026-03")]);
    assert_eq!(accrual_state(&app, id).await, (1.0, 3));

    // Policy non-MONTHLY dilewati.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    policy(&app, co.id, "", "").await;
    balance(&app, co.id, co.user(), 2026, 0.0, 0.0, 0.0, 0.0).await;
    assert_eq!(
        accrual_catch_up(app.db(), co.id, JKT, at("2026-04-10T05:00:00Z"))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn accrual_uses_company_zone_and_survives_duplicate_key() {
    let Some(app) = app().await else { return };
    // 2026-05-31T18:00Z = 1 Juni WIB → Mei dikreditkan.
    let co = monthly_company(&app, JKT, "2025-01-01T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 4.0, 0.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "accruedThroughMonth" = 4 WHERE id = {id}"#
    ))
    .await;
    accrual_catch_up(app.db(), co.id, JKT, at("2026-05-31T18:00:00Z"))
        .await
        .unwrap();
    let keys: Vec<_> = adjustments(&app, co.user())
        .await
        .into_iter()
        .map(|a| a.3.unwrap_or_default())
        .collect();
    assert_eq!(keys, vec![format!("accrual:{id}:2026-05")]);

    // Di UTC pada instan yang sama Mei belum.
    let co = monthly_company(&app, "UTC", "2025-01-01T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 4.0, 0.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "accruedThroughMonth" = 4 WHERE id = {id}"#
    ))
    .await;
    accrual_catch_up(app.db(), co.id, "UTC", at("2026-05-31T18:00:00Z"))
        .await
        .unwrap();
    assert!(adjustments(&app, co.user()).await.is_empty());

    // dedupeKey ganda (P2002) → penanda dimajukan, bukan gagal.
    let co = monthly_company(&app, JKT, "2025-01-01T00:00:00").await;
    let id = balance(&app, co.id, co.user(), 2026, 0.0, 0.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason, source, "dedupeKey")
           VALUES ({id}, {}, {}, 'annualLeaveTotal', 1, 'lama', 'MONTHLY_ACCRUAL', 'accrual:{id}:2026-01')"#,
        co.user(),
        co.id
    ))
    .await;
    assert_eq!(
        accrual_catch_up(app.db(), co.id, JKT, at("2026-02-10T05:00:00Z"))
            .await
            .unwrap(),
        0
    );
    assert_eq!(accrual_state(&app, id).await, (0.0, 1));
}

// ── leave-policy-switch.service.ts ───────────────────────────────────────────

#[tokio::test]
async fn policy_switch_tops_up_once() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 2, "2025-01-01T00:00:00").await;
    let late = add_user(&app, co.id, "Baru", "2026-09-30T17:30:00").await;
    policy(&app, co.id, "", "").await;
    let a = balance(&app, co.id, co.users[0], 2026, 3.0, 0.0, 0.0, 0.0).await;
    balance(&app, co.id, co.users[1], 2026, 12.0, 0.0, 0.0, 0.0).await;
    let c = balance(&app, co.id, late, 2026, 0.0, 0.0, 0.0, 0.0).await;
    let now = at("2026-10-15T00:00:00Z");
    assert_eq!(top_up_to_upfront(app.db(), co.id, now).await.unwrap(), 2);
    assert_eq!(accrual_state(&app, a).await.0, 12.0);
    assert_eq!(
        accrual_state(&app, c).await.0,
        3.0,
        "pro-rata tahun bergabung"
    );
    let adj = adjustments(&app, co.users[0]).await;
    assert_eq!(
        (adj[0].1, adj[0].5.as_str(), adj[0].2.as_str()),
        (9.0, "Beralih ke jatah penuh", "MONTHLY_ACCRUAL")
    );
    assert!(
        adj[0]
            .3
            .as_deref()
            .unwrap()
            .starts_with(&format!("upfront-topup:{a}:"))
    );
    assert_eq!(top_up_to_upfront(app.db(), co.id, now).await.unwrap(), 0);

    // Masih MONTHLY → batal.
    let co = monthly_company(&app, JKT, "2025-01-01T00:00:00").await;
    balance(&app, co.id, co.user(), 2026, 3.0, 0.0, 0.0, 0.0).await;
    assert_eq!(top_up_to_upfront(app.db(), co.id, now).await.unwrap(), 0);
}

// ── holiday-deduction.service.spec.ts ────────────────────────────────────────

async fn deduction_rows(
    app: &TestApp,
    user: i32,
) -> Vec<(Option<i32>, String, f64, f64, chrono::NaiveDate)> {
    sqlx::query_as(
        r#"SELECT "holidayId", status, "carryOverDeducted", "annualDeducted", "holidayDate"
           FROM "holidayLeaveDeduction" WHERE "userId" = $1 ORDER BY id"#,
    )
    .bind(user)
    .fetch_all(app.db())
    .await
    .unwrap()
}

async fn used(app: &TestApp, id: i32) -> (f64, f64) {
    sqlx::query_as(r#"SELECT "annualLeaveUsed", "carryOverUsed" FROM "leaveBalance" WHERE id = $1"#)
        .bind(id)
        .fetch_one(app.db())
        .await
        .unwrap()
}

#[tokio::test]
async fn holiday_apply_once_carry_first_partial_and_zero() {
    let Some(app) = app().await else { return };
    let now = at(NOW);
    // Sekali saja; ulang = no-op.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    let h = holiday(&app, co.id, "Libur 1", "2026-10-01", true).await;
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun {
            reversed: 0,
            applied: 1
        }
    );
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun::default()
    );
    assert_eq!(used(&app, b).await, (1.0, 0.0));
    let rows = deduction_rows(&app, co.user()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0, rows[0].1.as_str()), (Some(h), "APPLIED"));
    let adj = adjustments(&app, co.user()).await;
    assert_eq!(adj.len(), 1);
    assert_eq!(
        (adj[0].0.as_str(), adj[0].1, adj[0].2.as_str(), adj[0].4),
        ("annualLeaveUsed", 1.0, "HOLIDAY_DEDUCTION", Some(1.0))
    );
    assert_eq!(adj[0].5, "Cuti bersama: Libur 1");

    // Bawaan dulu.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 2.0, 0.0).await;
    holiday(&app, co.id, "Libur", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    assert_eq!(used(&app, b).await, (0.0, 1.0));

    // Hanya yang tersedia; intended dicatat.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 11.5, 0.0, 0.0).await;
    holiday(&app, co.id, "Libur", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    assert_eq!(used(&app, b).await.0, 12.0);
    let adj = adjustments(&app, co.user()).await;
    assert_eq!((adj.len(), adj[0].1, adj[0].4), (1, 0.5, Some(1.0)));
    assert!(
        adj[0].5.contains("terpotong 0.5 dari 1 hari"),
        "{}",
        adj[0].5
    );

    // Nol tersedia: baris APPLIED tetap ditulis, tanpa riwayat; ulang tidak menulis lagi.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 3.0, 3.0, 0.0, 0.0).await;
    holiday(&app, co.id, "Libur", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    assert_eq!(used(&app, b).await.0, 3.0);
    assert!(adjustments(&app, co.user()).await.is_empty());
    let rows = deduction_rows(&app, co.user()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2, rows[0].3),
        ("APPLIED", 0.0, 0.0)
    );
}

#[tokio::test]
async fn holiday_never_applies_weekend_future_or_before_join() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    // Bergabung sesudah akhir hari libur (lokal) → tidak dipotong.
    let late = add_user(&app, co.id, "Baru", "2026-10-01T17:00:00").await;
    holiday(&app, co.id, "Sabtu", "2026-09-26", true).await;
    holiday(&app, co.id, "Depan", "2026-10-05", true).await;
    holiday(&app, co.id, "Bukan pemotong", "2026-09-30", false).await;
    let h = holiday(&app, co.id, "Kamis", "2026-10-01", true).await;
    let run = apply_due(app.db(), co.id, JKT, at(NOW)).await.unwrap();
    assert_eq!(run.applied, 1);
    assert_eq!(
        deduction_rows(&app, co.user())
            .await
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        vec![Some(h)]
    );
    assert!(deduction_rows(&app, late).await.is_empty());
    // Saldo dibuat otomatis (getOrCreateBalance) untuk yang dipotong.
    assert!(
        find_balance(app.db(), co.user(), co.id, 2026)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn holiday_reverse_cases() {
    let Some(app) = app().await else { return };
    let now = at(NOW);
    // Refund split persis saat deductsLeave dimatikan.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.5, 0.0).await;
    let h = holiday(&app, co.id, "Libur 1", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    assert_eq!(used(&app, b).await, (0.5, 0.5));
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET "deductsLeave" = false WHERE id = {h}"#
    ))
    .await;
    let run = reconcile_holiday(app.db(), co.id, Some(h), JKT, now)
        .await
        .unwrap();
    assert_eq!(
        run,
        DeductionRun {
            reversed: 1,
            applied: 0
        }
    );
    assert_eq!(used(&app, b).await, (0.0, 0.0));
    assert_eq!(deduction_rows(&app, co.user()).await[0].1, "REVERSED");
    let rev: Vec<_> = adjustments(&app, co.user())
        .await
        .into_iter()
        .filter(|a| a.2 == "HOLIDAY_REVERSAL")
        .map(|a| (a.0, a.1, a.5))
        .collect();
    assert_eq!(
        rev,
        vec![
            (
                "carryOverUsed".to_owned(),
                -0.5,
                "Pembatalan potongan cuti bersama: Libur 1".to_owned()
            ),
            (
                "annualLeaveUsed".to_owned(),
                -0.5,
                "Pembatalan potongan cuti bersama: Libur 1".to_owned()
            ),
        ]
    );
    // on/off/on → tepat satu APPLIED.
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET "deductsLeave" = true WHERE id = {h}"#
    ))
    .await;
    reconcile_holiday(app.db(), co.id, Some(h), JKT, now)
        .await
        .unwrap();
    let rows = deduction_rows(&app, co.user()).await;
    assert_eq!((rows.len(), rows[0].1.as_str()), (1, "APPLIED"));
    assert_eq!(used(&app, b).await, (0.5, 0.5));

    // Tanggal bergeser → dibatalkan lalu diterapkan ulang pada tanggal baru.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    let h = holiday(&app, co.id, "Libur", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET date = '2026-09-30' WHERE id = {h}"#
    ))
    .await;
    reconcile_holiday(app.db(), co.id, Some(h), JKT, now)
        .await
        .unwrap();
    let rows = deduction_rows(&app, co.user()).await;
    assert_eq!(
        (rows.len(), rows[0].1.as_str(), rows[0].4),
        (1, "APPLIED", d("2026-09-30"))
    );
    assert_eq!(used(&app, b).await.0, 1.0);

    // Pindah ke Sabtu → dibatalkan, tidak diterapkan ulang.
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET date = '2026-09-26' WHERE id = {h}"#
    ))
    .await;
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun {
            reversed: 1,
            applied: 0
        }
    );
    assert_eq!(used(&app, b).await.0, 0.0);

    // Baris yatim (holidayId null) dibatalkan; libur dinonaktifkan dibatalkan; reconcile
    // berskop hanya menyentuh libur itu + yatim.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 3.0, 0.0, 0.0).await;
    let h1 = holiday(&app, co.id, "Libur 1", "2026-10-01", false).await;
    let h2 = holiday(&app, co.id, "Libur 2", "2026-10-01", false).await;
    for (hid, name) in [(Some(h1), "Libur 1"), (Some(h2), "Libur 2"), (None, "Lama")] {
        let hid = hid.map_or("NULL".to_owned(), |h| h.to_string());
        app.exec(&format!(
            r#"INSERT INTO "holidayLeaveDeduction" ("companyId", "holidayId", "userId", "balanceId", "holidayName",
                 "holidayDate", status, "intendedDays", "annualDeducted", "appliedAt")
               VALUES ({}, {hid}, {}, {b}, '{name}', '2026-10-01', 'APPLIED', 1, 1, now())"#,
            co.id,
            co.user()
        ))
        .await;
    }
    reconcile_holiday(app.db(), co.id, Some(h1), JKT, now)
        .await
        .unwrap();
    let statuses: Vec<_> = deduction_rows(&app, co.user())
        .await
        .into_iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(statuses, vec!["REVERSED", "APPLIED", "REVERSED"]);
    let orphan = adjustments(&app, co.user())
        .await
        .into_iter()
        .find(|a| a.5.ends_with("Lama"))
        .unwrap();
    assert_eq!((orphan.1, orphan.2.as_str()), (-1.0, "HOLIDAY_REVERSAL"));
    assert_eq!(used(&app, b).await.0, 1.0);
    // Tanpa skop: sisanya ikut dibatalkan.
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap().reversed,
        1
    );

    // Potongan yang masih benar tidak disentuh.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    holiday(&app, co.id, "Libur", "2026-10-01", true).await;
    apply_due(app.db(), co.id, JKT, now).await.unwrap();
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun::default()
    );
}

// › work pattern (specs/004 US6)
#[tokio::test]
async fn holiday_follows_work_pattern() {
    let Some(app) = app().await else { return };
    let now = at(NOW);
    // Pola 6 hari: cuti bersama Sabtu dipotong; Minggu tidak.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    six_day_pattern(&app, co.id, "2026-01-05", "2025-12-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    let sat = holiday(&app, co.id, "Cuti Bersama", "2026-09-26", true).await;
    holiday(&app, co.id, "Minggu", "2026-09-27", true).await;
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun {
            reversed: 0,
            applied: 1
        }
    );
    assert_eq!(deduction_rows(&app, co.user()).await[0].0, Some(sat));
    assert_eq!(used(&app, b).await.0, 1.0);
    // Potongan Sabtu yang sah tetap (reverse memakai predikat pola).
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun::default()
    );

    // Default (tanpa pola), libur Sabtu yang tidak berubah: SQL kandidat (tanpa ISODOW, sama
    // dengan Node) tidak memilihnya, jadi potongan lama tetap.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 1.0, 0.0, 0.0).await;
    let h = holiday(&app, co.id, "Cuti Bersama", "2026-09-26", true).await;
    app.exec(&format!(
        r#"INSERT INTO "holidayLeaveDeduction" ("companyId", "holidayId", "userId", "balanceId", "holidayName",
             "holidayDate", status, "intendedDays", "annualDeducted", "appliedAt")
           VALUES ({}, {h}, {}, {b}, 'Cuti Bersama', '2026-09-26', 'APPLIED', 1, 1, now())"#,
        co.id,
        co.user()
    ))
    .await;
    assert_eq!(
        apply_due(app.db(), co.id, JKT, now).await.unwrap(),
        DeductionRun::default()
    );

    // Pola terjadwal: hanya Sabtu sesudah effectiveFrom yang dipotong.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    six_day_pattern(&app, co.id, "2026-10-05", "2026-09-30T00:00:00").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    holiday(&app, co.id, "Sabtu lama", "2026-10-03", true).await;
    let later = holiday(&app, co.id, "Sabtu baru", "2026-10-10", true).await;
    let run = apply_due(app.db(), co.id, JKT, at("2026-10-12T05:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        run,
        DeductionRun {
            reversed: 0,
            applied: 1
        }
    );
    assert_eq!(deduction_rows(&app, co.user()).await[0].0, Some(later));
    assert_eq!(used(&app, b).await.0, 1.0);
}

// › recalcApprovedForDates
#[tokio::test]
async fn recalc_approved_leave_after_holiday_change() {
    let Some(app) = app().await else { return };
    // Libur ditambah di dalam rentang → refund 1 hari.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let t = leave_type(&app, co.id, true, "ANNUAL").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 5.0, 0.0, 0.0).await;
    holiday(&app, co.id, "Baru", "2026-09-30", false).await;
    let r = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-09-28",
        "2026-10-02",
        5.0,
        "APPROVED",
        None,
    )
    .await;
    assert_eq!(
        recalc_approved_for_dates(app.db(), co.id, &[d("2026-09-30")], JKT)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {r}"#)
        )
        .await,
        4.0
    );
    assert_eq!(used(&app, b).await.0, 4.0);
    let adj = adjustments(&app, co.user()).await;
    assert_eq!(
        (adj[0].0.as_str(), adj[0].1, adj[0].2.as_str(), adj[0].4),
        ("annualLeaveUsed", -1.0, "LEAVE_RECALC", None)
    );
    assert_eq!(
        adj[0].5,
        format!("Hitung ulang pengajuan #{r} karena perubahan hari libur")
    );
    let req_link: i32 = app
        .scalar(&format!(
            r#"SELECT "leaveRequestId" FROM "leaveBalanceAdjustment" WHERE "userId" = {}"#,
            co.user()
        ))
        .await;
    assert_eq!(req_link, r);
    // Ulang: tidak ada selisih.
    assert_eq!(
        recalc_approved_for_dates(app.db(), co.id, &[d("2026-09-30")], JKT)
            .await
            .unwrap(),
        0
    );

    // Libur dihapus → tagih hanya yang tersedia, intended dicatat.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let t = leave_type(&app, co.id, true, "ANNUAL").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 11.5, 0.0, 0.0).await;
    let r = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-09-28",
        "2026-10-02",
        4.0,
        "APPROVED",
        None,
    )
    .await;
    recalc_approved_for_dates(app.db(), co.id, &[d("2026-09-30")], JKT)
        .await
        .unwrap();
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {r}"#)
        )
        .await,
        5.0
    );
    assert_eq!(used(&app, b).await.0, 12.0);
    let adj = adjustments(&app, co.user()).await;
    assert_eq!((adj[0].1, adj[0].4), (0.5, Some(1.0)));

    // Jenis yang tidak memotong saldo: hanya totalDays. PENDING diabaikan.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    let t = leave_type(&app, co.id, false, "ANNUAL").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    holiday(&app, co.id, "Baru", "2026-09-30", false).await;
    let r = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-09-28",
        "2026-10-02",
        5.0,
        "APPROVED",
        None,
    )
    .await;
    let p = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-09-28",
        "2026-10-02",
        5.0,
        "PENDING",
        None,
    )
    .await;
    recalc_approved_for_dates(app.db(), co.id, &[d("2026-09-30"), d("2026-09-30")], JKT)
        .await
        .unwrap();
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {r}"#)
        )
        .await,
        4.0
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {p}"#)
        )
        .await,
        5.0
    );
    assert_eq!(used(&app, b).await.0, 0.0);
    assert!(adjustments(&app, co.user()).await.is_empty());

    // Sakit UNLIMITED (legacy SICK_LEAVE tanpa jenis): pemakaian sakit dicatat tanpa batas.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    policy(&app, co.id, r#""sickLeaveMode""#, "'UNLIMITED'").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 0.0, 0.0, 0.0).await;
    let r = request(
        &app,
        co.id,
        co.user(),
        None,
        "2026-09-28",
        "2026-10-02",
        4.0,
        "APPROVED",
        None,
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "leaveType" = 'SICK_LEAVE' WHERE id = {r}"#
    ))
    .await;
    recalc_approved_for_dates(app.db(), co.id, &[d("2026-09-30")], JKT)
        .await
        .unwrap();
    let sick: f64 = f(
        &app,
        &format!(r#"SELECT "sickLeaveUsed" FROM "leaveBalance" WHERE id = {b}"#),
    )
    .await;
    assert_eq!(sick, 1.0);
}

#[tokio::test]
async fn recalc_uses_pattern_as_of_review() {
    let Some(app) = app().await else { return };
    // Pola dibuat SESUDAH persetujuan → Sabtu tidak dihitung ulang.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    six_day_pattern(&app, co.id, "2026-09-28", "2026-09-26T00:00:00").await;
    let t = leave_type(&app, co.id, true, "ANNUAL").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 1.0, 0.0, 0.0).await;
    let r = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-10-02",
        "2026-10-03",
        1.0,
        "APPROVED",
        Some("2026-09-25T03:00:00"),
    )
    .await;
    assert_eq!(
        recalc_approved_for_dates(app.db(), co.id, &[d("2026-10-02")], JKT)
            .await
            .unwrap(),
        0
    );
    assert_eq!(used(&app, b).await.0, 1.0);

    // Pola dibuat SEBELUM persetujuan → Sabtu dihitung.
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    six_day_pattern(&app, co.id, "2026-09-28", "2026-09-20T00:00:00").await;
    let t = leave_type(&app, co.id, true, "ANNUAL").await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 1.0, 0.0, 0.0).await;
    let r2 = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        "2026-10-02",
        "2026-10-03",
        1.0,
        "APPROVED",
        Some("2026-09-25T03:00:00"),
    )
    .await;
    assert_eq!(
        recalc_approved_for_dates(app.db(), co.id, &[d("2026-10-02")], JKT)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {r2}"#)
        )
        .await,
        2.0
    );
    assert_eq!(used(&app, b).await.0, 2.0);
    let _ = r;
}

// ── loadScheduledHolidays / getScheduledHolidayDeductions / toBalanceView ────

#[tokio::test]
async fn scheduled_holidays_and_balance_view() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 1, "2025-01-01T00:00:00").await;
    holiday(&app, co.id, "Lewat", "2026-10-01", true).await;
    holiday(&app, co.id, "Hari ini", "2026-10-02", true).await;
    let a = holiday(&app, co.id, "Senin", "2026-10-05", true).await;
    holiday(&app, co.id, "Sabtu", "2026-10-10", true).await;
    holiday(&app, co.id, "Tak memotong", "2026-10-06", false).await;
    let applied = holiday(&app, co.id, "Sudah", "2026-10-07", true).await;
    holiday(&app, co.id, "Tahun depan", "2027-01-04", true).await;
    let b = balance(&app, co.id, co.user(), 2026, 12.0, 2.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"INSERT INTO "holidayLeaveDeduction" ("companyId", "holidayId", "userId", "balanceId", "holidayName",
             "holidayDate", status, "intendedDays", "annualDeducted", "appliedAt")
           VALUES ({}, {applied}, {}, {b}, 'Sudah', '2026-10-07', 'APPLIED', 1, 1, now())"#,
        co.id,
        co.user()
    ))
    .await;
    let now = at(NOW);
    let mut conn = app.db().acquire().await.unwrap();
    let list = get_scheduled_holiday_deductions(&mut conn, co.user(), co.id, 2026, JKT, now)
        .await
        .unwrap();
    let ids: Vec<i32> = list.iter().map(|h| h.holiday_id).collect();
    assert_eq!(ids, vec![a]);
    assert_eq!(
        serde_json::to_string(&list[0]).unwrap(),
        format!(r#"{{"holidayId":{a},"name":"Senin","date":"2026-10-05","days":1}}"#)
    );
    // Tahun depan: mulai awal tahun itu.
    let next = get_scheduled_holiday_deductions(&mut conn, co.user(), co.id, 2027, JKT, now)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    // Tahun lampau: kosong.
    assert!(
        get_scheduled_holiday_deductions(&mut conn, co.user(), co.id, 2025, JKT, now)
            .await
            .unwrap()
            .is_empty()
    );

    let balance = find_balance(app.db(), co.user(), co.id, 2026)
        .await
        .unwrap()
        .unwrap();
    let view = to_balance_view(
        &balance,
        &BalanceViewCtx {
            policy: None,
            override_days: None,
            tier_days: None,
            time_zone: JKT,
            scheduled_holiday_days: list.len(),
            now,
        },
    );
    assert_eq!(
        (
            view.requestable,
            view.entitlement,
            view.carry_over_available
        ),
        (9.0, 12.0, 0.0)
    );
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["accrualMode"], "UPFRONT");
    assert_eq!(json["annualLeaveTotal"], 12);
}

#[tokio::test]
async fn policy_helpers_against_db() {
    let Some(app) = app().await else { return };
    let co = company(&app, JKT, 3, "2025-01-01T00:00:00").await;
    let pid = policy(
        &app,
        co.id,
        r#""sickLeaveMode", "sickLeaveDays""#,
        "'LIMITED', 4",
    )
    .await;
    app.exec(&format!(
        r#"UPDATE users SET name = 'Zed' WHERE id = {}"#,
        co.users[0]
    ))
    .await;
    app.exec(&format!(
        r#"UPDATE users SET name = 'Ani' WHERE id = {}"#,
        co.users[1]
    ))
    .await;
    for u in [co.users[0], co.users[1]] {
        app.exec(&format!(
            r#"INSERT INTO "leavePolicyApprover" ("policyId", "userId", "companyId") VALUES ({pid}, {u}, {})"#,
            co.id
        ))
        .await;
    }
    let approvers = mz_leave::policy::get_policy_approvers(app.db(), co.id)
        .await
        .unwrap();
    assert_eq!(
        approvers
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Ani", "Zed"]
    );
    assert!(
        mz_leave::policy::is_policy_approver(app.db(), co.users[0], co.id)
            .await
            .unwrap()
    );
    assert!(
        !mz_leave::policy::is_policy_approver(app.db(), co.users[2], co.id)
            .await
            .unwrap()
    );
    let p = mz_leave::policy::load_policy(app.db(), co.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.sick_leave_days, p.sick_leave_mode.as_str()),
        (Some(4), "LIMITED")
    );
    assert!(
        mz_leave::policy::load_policy(app.db(), -1)
            .await
            .unwrap()
            .is_none()
    );

    // propagateSickTotal: baris yang pernah di-adjust manual dilewati.
    let now = at(NOW);
    let b0 = balance(&app, co.id, co.users[0], 2026, 12.0, 0.0, 0.0, 0.0).await;
    let b1 = balance(&app, co.id, co.users[1], 2026, 12.0, 0.0, 0.0, 0.0).await;
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason)
           VALUES ({b1}, {}, {}, 'sickLeaveTotal', 2, 'manual')"#,
        co.users[1],
        co.id
    ))
    .await;
    let mut tx = app.db().begin().await.unwrap();
    mz_leave::policy::apply_sick_leave_from_leave_type(&mut tx, co.id, Some(7), now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let sick = |id: i32| format!(r#"SELECT "sickLeaveTotal" FROM "leaveBalance" WHERE id = {id}"#);
    assert_eq!(f(&app, &sick(b0)).await, 7.0);
    assert_eq!(f(&app, &sick(b1)).await, 0.0);
    let p = mz_leave::policy::load_policy(app.db(), co.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.sick_leave_days, p.sick_leave_mode.as_str()),
        (Some(7), "LIMITED")
    );
    let mut tx = app.db().begin().await.unwrap();
    mz_leave::policy::apply_sick_leave_from_leave_type(&mut tx, co.id, None, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let p = mz_leave::policy::load_policy(app.db(), co.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.sick_leave_days, p.sick_leave_mode.as_str()),
        (None, "UNLIMITED")
    );
    // Company tanpa policy → dibuat.
    let co2 = company(&app, JKT, 0, "2025-01-01T00:00:00").await;
    let mut tx = app.db().begin().await.unwrap();
    mz_leave::policy::apply_sick_leave_from_leave_type(&mut tx, co2.id, Some(3), now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        mz_leave::policy::load_policy(app.db(), co2.id)
            .await
            .unwrap()
            .is_some()
    );

    // loadHolidayIndex: hanya aktif, dalam rentang.
    holiday(&app, co.id, "A", "2026-10-01", false).await;
    let h = holiday(&app, co.id, "B", "2026-10-01", true).await;
    let off = holiday(&app, co.id, "C", "2026-10-02", true).await;
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET "isActive" = false WHERE id = {off}"#
    ))
    .await;
    let mut conn = app.db().acquire().await.unwrap();
    let idx =
        mz_leave::calendar::load_holiday_index(&mut conn, co.id, d("2026-09-01"), d("2026-10-31"))
            .await
            .unwrap();
    assert_eq!(idx.excluded_dates.len(), 1);
    assert_eq!(idx.by_date.get(&d("2026-10-01")).map(|e| e.id), Some(h));
}
