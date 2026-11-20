//! Uji integrasi `/leave-policies*` (20 route) terhadap Postgres + Redis nyata: kebijakan &
//! approver, override karyawan, tier & penugasan, saldo (inisialisasi, daftar, adjust, riwayat),
//! efek samping (propagasi cuti sakit, `accruedThroughMonth`, enqueue `leave.policy-switch`).
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-leave-config --test it_leave_cfg_policies`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use chrono::{Datelike, NaiveDate, Weekday};
use common::*;
use mz_leave::jobs::LEAVE_SCHEDULE_QUEUE;
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

const POLICY_KEYS: &[&str] = &[
    "id",
    "companyId",
    "annualLeaveDays",
    "sickLeaveDays",
    "sickLeaveMode",
    "carryOverEnabled",
    "maxCarryOverDays",
    "carryOverExpiryMonth",
    "resetType",
    "entitlementType",
    "accrualMode",
    "monthlyAccrualDays",
    "createdAt",
    "updatedAt",
    "leaveTiers",
    "approvers",
];
const TIER_KEYS: &[&str] = &[
    "id",
    "companyId",
    "policyId",
    "name",
    "minTenureMonths",
    "maxTenureMonths",
    "annualLeaveDays",
    "createdAt",
    "updatedAt",
];
const BALANCE_KEYS: &[&str] = &[
    "id",
    "userId",
    "companyId",
    "year",
    "annualLeaveTotal",
    "annualLeaveUsed",
    "sickLeaveTotal",
    "sickLeaveUsed",
    "carryOverDays",
    "carryOverUsed",
    "accruedThroughMonth",
    "createdAt",
    "updatedAt",
];

async fn put_policy(app: &TestApp, fx: &Fx, body: Value) -> TestResponse {
    app.put("/leave-policies")
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await
}

fn this_year() -> i32 {
    chrono::Utc::now().year()
}

async fn set_joined(app: &TestApp, company_id: i32, date: &str) {
    app.exec(&format!(
        r#"UPDATE "userCompany" SET "createdAt" = '{date}' WHERE "companyId" = {company_id}"#
    ))
    .await;
}

#[tokio::test]
async fn policy_get_default_upsert_validation_and_roles() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;

    // Belum ada baris: bentuk default Node (tanpa id/createdAt).
    let r = app
        .get("/leave-policies")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(
        &r.json,
        &[
            "companyId",
            "annualLeaveDays",
            "sickLeaveDays",
            "sickLeaveMode",
            "carryOverEnabled",
            "maxCarryOverDays",
            "carryOverExpiryMonth",
            "resetType",
            "entitlementType",
            "accrualMode",
            "monthlyAccrualDays",
            "leaveTiers",
            "approvers",
        ],
    );
    assert_eq!(r.json["annualLeaveDays"], 12);
    assert_eq!(r.json["sickLeaveMode"], "DEDUCT_ANNUAL");
    assert_eq!(r.json["accrualMode"], "UPFRONT");

    // Pembuatan pertama; company baru tanpa input cuti sakit = UNLIMITED.
    let r = put_policy(
        &app,
        &fx,
        json!({"annualLeaveDays": 14, "carryOverEnabled": true, "maxCarryOverDays": "5", "carryOverExpiryMonth": 3}),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(&r.json, POLICY_KEYS);
    assert_eq!(r.json["annualLeaveDays"], 14);
    assert_eq!(r.json["sickLeaveMode"], "UNLIMITED");
    assert!(r.json["sickLeaveDays"].is_null());
    assert_eq!(r.json["carryOverEnabled"], true);
    assert_eq!(r.json["maxCarryOverDays"], 5);
    assert_eq!(r.json["carryOverExpiryMonth"], 3);
    assert_eq!(r.json["resetType"], "ANNUAL");
    assert_eq!(r.json["entitlementType"], "UNIFORM");
    assert_eq!(r.json["accrualMode"], "UPFRONT");
    assert!(r.json["monthlyAccrualDays"].is_null());
    assert_eq!(r.json["leaveTiers"], json!([]));
    assert_eq!(r.json["approvers"], json!([]));
    let id = r.json["id"].clone();

    // Update parsial: field lain tetap; `null` pada kolom nullable menghapus nilai.
    let r = put_policy(
        &app,
        &fx,
        json!({"annualLeaveDays": 20, "maxCarryOverDays": null}),
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.json["id"], id);
    assert_eq!(r.json["annualLeaveDays"], 20);
    assert!(r.json["maxCarryOverDays"].is_null());
    assert_eq!(r.json["carryOverExpiryMonth"], 3);
    assert_eq!(r.json["carryOverEnabled"], true);
    assert!(r.json["updatedAt"].is_string());
    let got = app
        .get("/leave-policies")
        .token(&fx.manager.token)
        .send()
        .await;
    assert_eq!(got.json, r.json);

    // Gabungan DB + DTO: MONTHLY wajib jumlah per bulan; tidak boleh melebihi jatah tahunan.
    let r = put_policy(&app, &fx, json!({"accrualMode": "MONTHLY"})).await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({"code": "LEAVE_POLICY_MONTHLY_ACCRUAL_REQUIRED",
               "message": "Jumlah cuti per bulan wajib diisi (kelipatan 0,5) untuk mode bulanan"})
    );
    let r = put_policy(
        &app,
        &fx,
        json!({"accrualMode": "MONTHLY", "monthlyAccrualDays": 21}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "LEAVE_POLICY_MONTHLY_EXCEEDS_MAX");
    assert_eq!(
        r.json["message"],
        "Jumlah per bulan tidak boleh melebihi maksimal per tahun"
    );
    // Tidak ada yang tersimpan setelah 400.
    let got = app
        .get("/leave-policies")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(got.json["accrualMode"], "UPFRONT");
    let r = put_policy(&app, &fx, json!({"sickLeaveMode": "LIMITED"})).await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "LEAVE_POLICY_SICK_DAYS_REQUIRED");

    // Validasi DTO (urutan pesan class-validator).
    let r = put_policy(
        &app,
        &fx,
        json!({"annualLeaveDays": -1, "carryOverExpiryMonth": 13, "resetType": "X",
               "monthlyAccrualDays": 0.25, "approverIds": [0, "a"]}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "annualLeaveDays must not be less than 0",
            "carryOverExpiryMonth must not be greater than 12",
            "resetType must be one of the following values: ANNUAL, ANNIVERSARY",
            "monthlyAccrualDays harus kelipatan 0,5",
            "monthlyAccrualDays must not be less than 0.5",
            "monthlyAccrualDays must be a number conforming to the specified constraints",
            "each value in approverIds must be a positive number",
            "each value in approverIds must be an integer number",
        ]
    );
    // `annualLeaveDays: null` lolos validasi lalu Prisma menolak (kolom wajib) → 500.
    let r = put_policy(&app, &fx, json!({"annualLeaveDays": null})).await;
    assert_eq!(r.status.as_u16(), 500);

    // Role & auth.
    app.get("/leave-policies").send().await.assert_status(401);
    app.get("/leave-policies")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.put("/leave-policies")
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn sick_and_accrual_side_effects_and_policy_switch_job() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let year = this_year();
    let (e1, e2) = (
        employee(&app, fx.id, &format!("S1 {}", fx.id)).await,
        employee(&app, fx.id, &format!("S2 {}", fx.id)).await,
    );
    let mut balances = Vec::new();
    for e in [e1, e2] {
        let b: i32 = app
            .scalar(&format!(
                r#"INSERT INTO "leaveBalance" ("userId", "companyId", year) VALUES ({e}, {}, {year}) RETURNING id"#,
                fx.id
            ))
            .await;
        balances.push(b);
    }
    let sick = |b: i32| {
        let app = &app;
        async move {
            app.scalar::<f64>(&format!(
                r#"SELECT "sickLeaveTotal" FROM "leaveBalance" WHERE id = {b}"#
            ))
            .await
        }
    };
    // Saldo kedua pernah di-adjust manual.
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason, source)
           VALUES ({}, {e2}, {}, 'sickLeaveTotal', 2, 'manual', 'ADJUSTMENT');
           UPDATE "leaveBalance" SET "sickLeaveTotal" = 2 WHERE id = {}"#,
        balances[1], fx.id, balances[1]
    ))
    .await;

    // sickLeaveDays saja → LIMITED + propagasi (baris ter-adjust dilewati).
    let r = put_policy(&app, &fx, json!({"sickLeaveDays": 7})).await;
    r.assert_status(200);
    assert_eq!(r.json["sickLeaveMode"], "LIMITED");
    assert_eq!(r.json["sickLeaveDays"], 7);
    assert_eq!(
        (sick(balances[0]).await, sick(balances[1]).await),
        (7.0, 2.0)
    );

    // Nilai sama + mode sama → tidak propagasi ulang.
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "sickLeaveTotal" = 1 WHERE id = {}"#,
        balances[0]
    ))
    .await;
    put_policy(&app, &fx, json!({"sickLeaveDays": 7}))
        .await
        .assert_status(200);
    assert_eq!(sick(balances[0]).await, 1.0);
    put_policy(&app, &fx, json!({"sickLeaveDays": 9}))
        .await
        .assert_status(200);
    assert_eq!(
        (sick(balances[0]).await, sick(balances[1]).await),
        (9.0, 2.0)
    );
    // c7e3aaf: kuota diturunkan di bawah pemakaian → di-clamp ke sickLeaveUsed per baris.
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "sickLeaveUsed" = 8 WHERE id = {}"#,
        balances[0]
    ))
    .await;
    put_policy(&app, &fx, json!({"sickLeaveDays": 5}))
        .await
        .assert_status(200);
    assert_eq!(sick(balances[0]).await, 8.0);
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "sickLeaveUsed" = 0 WHERE id = {}"#,
        balances[0]
    ))
    .await;
    put_policy(&app, &fx, json!({"sickLeaveDays": 9}))
        .await
        .assert_status(200);
    assert_eq!(sick(balances[0]).await, 9.0);

    // UNLIMITED mengosongkan jatah; saldo tidak disentuh.
    let r = put_policy(&app, &fx, json!({"sickLeaveMode": "UNLIMITED"})).await;
    assert_eq!(r.json["sickLeaveMode"], "UNLIMITED");
    assert!(r.json["sickLeaveDays"].is_null());
    assert_eq!(sick(balances[0]).await, 9.0);
    // Klien lama: sickLeaveDays null saat sudah UNLIMITED tetap UNLIMITED; saat LIMITED → DEDUCT_ANNUAL.
    let r = put_policy(&app, &fx, json!({"sickLeaveDays": null})).await;
    assert_eq!(r.json["sickLeaveMode"], "UNLIMITED");
    put_policy(&app, &fx, json!({"sickLeaveDays": 4}))
        .await
        .assert_status(200);
    let r = put_policy(&app, &fx, json!({"sickLeaveDays": null})).await;
    assert_eq!(r.json["sickLeaveMode"], "DEDUCT_ANNUAL");
    assert!(r.json["sickLeaveDays"].is_null());

    // c7e3aaf: saldo tahun lalu (UPFRONT, sudah tutup) ditandai lengkap (12) saat beralih.
    let past: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "accruedThroughMonth")
               VALUES ({e1}, {}, {}, 0) RETURNING id"#,
            fx.id,
            year - 1
        ))
        .await;

    // UPFRONT → MONTHLY: bulan yang sudah lewat dianggap dikreditkan (zona company = UTC).
    let month0: i32 = app
        .scalar("SELECT EXTRACT(MONTH FROM NOW() AT TIME ZONE 'UTC')::int - 1")
        .await;
    let r = put_policy(
        &app,
        &fx,
        json!({"accrualMode": "MONTHLY", "monthlyAccrualDays": 1.5}),
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.json["accrualMode"], "MONTHLY");
    assert_eq!(r.json["monthlyAccrualDays"], 1.5);
    for b in &balances {
        let m: i32 = app
            .scalar(&format!(
                r#"SELECT "accruedThroughMonth" FROM "leaveBalance" WHERE id = {b}"#
            ))
            .await;
        assert_eq!(m, month0);
    }
    let m: i32 = app
        .scalar(&format!(
            r#"SELECT "accruedThroughMonth" FROM "leaveBalance" WHERE id = {past}"#
        ))
        .await;
    assert_eq!(m, 12);
    // Tidak ada job peralihan untuk UPFRONT → MONTHLY.
    let updated_ms = |r: &TestResponse| {
        mz_core::time::parse_js_date(r.json["updatedAt"].as_str().unwrap())
            .unwrap()
            .and_utc()
            .timestamp_millis()
    };
    let none = mz_core::queue::get(
        app.state(),
        LEAVE_SCHEDULE_QUEUE,
        &mz_leave::jobs::policy_switch_job_id(fx.id, updated_ms(&r)),
    )
    .await
    .unwrap();
    assert!(none.is_none());

    // MONTHLY → UPFRONT: job `leave.policy-switch` di-enqueue SESUDAH commit.
    let r = put_policy(&app, &fx, json!({"accrualMode": "UPFRONT"})).await;
    r.assert_status(200);
    assert_eq!(r.json["accrualMode"], "UPFRONT");
    let job_id = mz_leave::jobs::policy_switch_job_id(fx.id, updated_ms(&r));
    let job = mz_core::queue::get(app.state(), LEAVE_SCHEDULE_QUEUE, &job_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("job {job_id} tidak ada"));
    assert_eq!(
        job.data,
        json!({"name": "leave.policy-switch", "data": {"companyId": fx.id}})
    );
    assert_eq!((job.max_attempts, job.backoff_ms), (3, 5_000));
    mz_core::queue::ack(app.state(), LEAVE_SCHEDULE_QUEUE, &job_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn approvers_replace_validate_and_admins_list() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let other = app.other_company_admin().await;

    // Daftar admin/manager (staff tidak termasuk), peran efektif per orang.
    let r = app
        .get("/leave-policies/admins")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let rows = r.json.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in rows {
        assert_keys(row, &["id", "name", "username", "role"]);
    }
    let role_of = |id: i32| {
        rows.iter()
            .find(|x| x["id"] == id)
            .map(|x| x["role"].clone())
    };
    assert_eq!(role_of(fx.admin.id), Some(json!("ADMIN")));
    assert_eq!(role_of(fx.manager.id), Some(json!("MANAGER")));
    let expected: Vec<i32> = sqlx::query_scalar(&format!(
        "SELECT id FROM users WHERE id IN ({}, {}) ORDER BY name ASC",
        fx.admin.id, fx.manager.id
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    let got: Vec<i32> = rows
        .iter()
        .map(|x| x["id"].as_i64().unwrap() as i32)
        .collect();
    assert_eq!(got, expected);
    app.get("/leave-policies/admins")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // Approver: ganti seluruh daftar (dedupe), urut nama.
    let r = put_policy(
        &app,
        &fx,
        json!({"approverIds": [fx.manager.id, fx.admin.id, fx.manager.id]}),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    let approvers = r.json["approvers"].as_array().unwrap();
    assert_eq!(approvers.len(), 2);
    assert_keys(
        &approvers[0],
        &["id", "policyId", "userId", "companyId", "createdAt", "user"],
    );
    assert_keys(&approvers[0]["user"], &["id", "name", "username"]);
    let ids: Vec<i64> = approvers
        .iter()
        .map(|a| a["userId"].as_i64().unwrap())
        .collect();
    let by_name: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id::bigint FROM users WHERE id IN ({}, {}) ORDER BY name ASC",
        fx.admin.id, fx.manager.id
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(ids, by_name);
    let listed = mz_leave::policy::get_policy_approvers(app.db(), fx.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);

    // Tidak valid: staff, admin company lain, id tak ada → pesan memuat id sesuai urutan kirim.
    let r = put_policy(
        &app,
        &fx,
        json!({"approverIds": [fx.staff.id, fx.admin.id, other.admin.id, 987_654_321]}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        format!(
            "User(s) [{}, {}, 987654321] are not a valid SYSTEM_ADMIN/SUPER_ADMIN/ADMIN/MANAGER in this company",
            fx.staff.id, other.admin.id
        )
    );
    // Gagal validasi → daftar lama utuh (transaksi dibatalkan).
    let got = app
        .get("/leave-policies")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(got.json["approvers"].as_array().unwrap().len(), 2);
    // User nonaktif tidak valid.
    app.exec(&format!(
        "UPDATE users SET status = 3 WHERE id = {}",
        fx.manager.id
    ))
    .await;
    let r = put_policy(&app, &fx, json!({"approverIds": [fx.manager.id]})).await;
    r.assert_status(400);
    app.exec(&format!(
        "UPDATE users SET status = 1 WHERE id = {}",
        fx.manager.id
    ))
    .await;

    // Mengganti jadi satu; tanpa field → tidak berubah; [] dan null mengosongkan.
    let r = put_policy(&app, &fx, json!({"approverIds": [fx.manager.id]})).await;
    assert_eq!(r.json["approvers"].as_array().unwrap().len(), 1);
    let r = put_policy(&app, &fx, json!({"annualLeaveDays": 13})).await;
    assert_eq!(r.json["approvers"].as_array().unwrap().len(), 1);
    let r = put_policy(&app, &fx, json!({"approverIds": []})).await;
    assert_eq!(r.json["approvers"], json!([]));
    put_policy(&app, &fx, json!({"approverIds": [fx.admin.id]}))
        .await
        .assert_status(200);
    let r = put_policy(&app, &fx, json!({"approverIds": null})).await;
    r.assert_status(200);
    assert_eq!(r.json["approvers"], json!([]));
    // String numerik diterima (`@Type(() => Number)`).
    let r = put_policy(&app, &fx, json!({"approverIds": [fx.admin.id.to_string()]})).await;
    assert_eq!(r.json["approvers"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn employee_overrides() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let fx2 = fixture(&app).await;
    let e1 = employee(&app, fx.id, &format!("Zed {}", fx.id)).await;
    let e2 = employee(&app, fx.id, &format!("Abe {}", fx.id)).await;
    let outsider = employee(&app, fx2.id, "Outsider").await;
    let url = |u: i32| format!("/leave-policies/employees/{u}");

    // Belum ada override → bentuk default (tanpa policy: 12).
    let r = app.get(&url(e1)).token(&fx.admin.token).send().await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({"userId": e1, "companyId": fx.id, "annualLeaveDays": 12, "isOverride": false})
    );
    put_policy(&app, &fx, json!({"annualLeaveDays": 16}))
        .await
        .assert_status(200);
    let r = app.get(&url(e1)).token(&fx.admin.token).send().await;
    assert_eq!(r.json["annualLeaveDays"], 16);

    // Validasi + 404 anggota.
    let r = app
        .put(&url(e1))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "annualLeaveDays must not be less than 0",
            "annualLeaveDays must be an integer number"
        ]
    );
    app.put(&url(e1))
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": 1.5}))
        .send()
        .await
        .assert_status(400);
    let r = app
        .put(&url(outsider))
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": 15}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "User not found in this company");

    // Upsert (buat lalu ubah), bentuk baris tanpa `user`.
    let r = app
        .put(&url(e1))
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": "15"}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(
        &r.json,
        &[
            "id",
            "userId",
            "companyId",
            "annualLeaveDays",
            "createdAt",
            "updatedAt",
        ],
    );
    assert_eq!(r.json["annualLeaveDays"], 15);
    let id = r.json["id"].clone();
    let r = app
        .put(&url(e1))
        .token(&fx.manager.token)
        .json(&json!({"annualLeaveDays": 18}))
        .send()
        .await;
    assert_eq!(
        (r.json["id"].clone(), r.json["annualLeaveDays"].clone()),
        (id, json!(18))
    );
    app.put(&url(e2))
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": 0}))
        .send()
        .await
        .assert_status(200);

    let r = app.get(&url(e1)).token(&fx.admin.token).send().await;
    assert_keys(
        &r.json,
        &[
            "id",
            "userId",
            "companyId",
            "annualLeaveDays",
            "createdAt",
            "updatedAt",
            "user",
            "isOverride",
        ],
    );
    assert_eq!(r.json["isOverride"], true);
    assert_eq!(r.json["user"]["id"], e1);

    // Daftar: urut nama pengguna ('Abe' dulu).
    let r = app
        .get("/leave-policies/employees")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let rows = r.json.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["userId"], e2);
    assert_keys(
        &rows[0],
        &[
            "id",
            "userId",
            "companyId",
            "annualLeaveDays",
            "createdAt",
            "updatedAt",
            "user",
        ],
    );
    // Company lain tidak melihatnya.
    let r = app
        .get("/leave-policies/employees")
        .token(&fx2.admin.token)
        .send()
        .await;
    assert_eq!(r.json, json!([]));

    // Hapus.
    let r = app.delete(&url(e1)).token(&fx.admin.token).send().await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"success": true}));
    let r = app.delete(&url(e1)).token(&fx.admin.token).send().await;
    r.assert_status(404);
    assert_eq!(
        r.json["message"],
        "Employee leave policy override not found"
    );
    assert_eq!(
        app.get(&url(e1)).token(&fx.admin.token).send().await.json["isOverride"],
        false
    );
    // Id bukan angka → NaN → 500.
    assert_eq!(
        app.get("/leave-policies/employees/abc")
            .token(&fx.admin.token)
            .send()
            .await
            .status
            .as_u16(),
        500
    );
    // Role.
    app.put(&url(e1))
        .token(&fx.staff.token)
        .json(&json!({"annualLeaveDays": 1}))
        .send()
        .await
        .assert_status(401);
    app.delete(&url(e2))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn tiers_crud_and_assignment() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let fx2 = fixture(&app).await;
    let e1 = employee(&app, fx.id, &format!("T1 {}", fx.id)).await;
    let outsider = employee(&app, fx2.id, "Outsider T").await;

    // Validasi gagal TETAP membuat leavePolicy dulu (upsert sebelum validasi tenure).
    let r = app
        .post("/leave-policies/tiers")
        .token(&fx.admin.token)
        .json(&json!({"name": "Bad", "minTenureMonths": 12, "maxTenureMonths": 12, "annualLeaveDays": 15}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "maxTenureMonths must be greater than minTenureMonths"
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leavePolicy" WHERE "companyId" = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(n, 1);
    // Validasi DTO tidak menyentuh DB.
    let r = app
        .post("/leave-policies/tiers")
        .token(&fx.admin.token)
        .json(&json!({"minTenureMonths": -1}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "name must be shorter than or equal to 100 characters",
            "name must be a string",
            "minTenureMonths must not be less than 0",
            "annualLeaveDays must not be less than 0",
            "annualLeaveDays must be an integer number"
        ]
    );

    let mk = |body: Value| {
        let app = &app;
        let token = fx.admin.token.clone();
        async move {
            app.post("/leave-policies/tiers")
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };
    let senior = mk(json!({"name": "Senior", "minTenureMonths": 12, "annualLeaveDays": 15})).await;
    senior.assert_status(201).assert_no_sql_error();
    assert_keys(&senior.json, TIER_KEYS);
    assert!(senior.json["maxTenureMonths"].is_null());
    let junior = mk(json!({"name": "Junior", "maxTenureMonths": 12, "annualLeaveDays": 12})).await;
    junior.assert_status(201);
    assert_eq!(junior.json["minTenureMonths"], 0);
    assert_eq!(junior.json["maxTenureMonths"], 12);
    assert_eq!(junior.json["policyId"], senior.json["policyId"]);
    let (sid, jid) = (
        senior.json["id"].as_i64().unwrap(),
        junior.json["id"].as_i64().unwrap(),
    );

    // Daftar: urut minTenureMonths naik, dengan `_count`.
    let r = app
        .get("/leave-policies/tiers")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let rows = r.json.as_array().unwrap();
    assert_eq!(rows[0]["id"], jid);
    assert_eq!(rows[1]["id"], sid);
    let mut keys_expected: Vec<&str> = TIER_KEYS.to_vec();
    keys_expected.push("_count");
    assert_keys(&rows[0], &keys_expected);
    assert_eq!(rows[0]["_count"], json!({"employeeTiers": 0}));

    // Detail: ParseIntPipe 400, 404 berkode, milik company lain 404.
    let r = app
        .get(&format!("/leave-policies/tiers/{sid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_keys(&r.json, &keys_expected);
    for bad in ["abc", "employees", "1x", ""] {
        if bad.is_empty() {
            continue;
        }
        let r = app
            .get(&format!("/leave-policies/tiers/{bad}"))
            .token(&fx.admin.token)
            .send()
            .await;
        r.assert_status(400);
        assert_eq!(
            r.json["message"], "Validation failed (numeric string is expected)",
            "{bad}"
        );
    }
    let r = app
        .get("/leave-policies/tiers/99999999")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code": "LEAVE_TIER_NOT_FOUND", "message": "Leave tier not found"})
    );
    app.get(&format!("/leave-policies/tiers/{sid}"))
        .token(&fx2.admin.token)
        .send()
        .await
        .assert_status(404);

    // Update (PUT memakai CreateLeaveTierDto: name & annualLeaveDays wajib).
    let r = app
        .put(&format!("/leave-policies/tiers/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"name": "Senior+", "annualLeaveDays": 18}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(&r.json, TIER_KEYS);
    assert_eq!(r.json["name"], "Senior+");
    assert_eq!(
        r.json["minTenureMonths"], 12,
        "min tidak dikirim → nilai lama"
    );
    assert_eq!(r.json["annualLeaveDays"], 18);
    assert!(r.json["updatedAt"].is_string());
    // max <= min (nilai lama min = 12) → 400; null menghapus batas atas.
    let r = app
        .put(&format!("/leave-policies/tiers/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"name": "x", "maxTenureMonths": 12, "annualLeaveDays": 1}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "maxTenureMonths must be greater than minTenureMonths"
    );
    let r = app
        .put(&format!("/leave-policies/tiers/{jid}"))
        .token(&fx.admin.token)
        .json(&json!({"name": "Junior", "maxTenureMonths": null, "annualLeaveDays": 12}))
        .send()
        .await;
    assert!(r.json["maxTenureMonths"].is_null());
    let r = app
        .put("/leave-policies/tiers/99999999")
        .token(&fx.admin.token)
        .json(&json!({"name": "x", "annualLeaveDays": 1}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Leave tier not found");
    app.put(&format!("/leave-policies/tiers/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);

    // Penugasan.
    let assign = |tier: i64, user: i32| {
        let app = &app;
        let token = fx.admin.token.clone();
        async move {
            app.put(&format!("/leave-policies/tiers/{tier}/assign/{user}"))
                .token(&token)
                .send()
                .await
        }
    };
    let r = assign(sid, outsider).await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "User not found in this company");
    let r = assign(99_999_999, e1).await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Leave tier not found");
    let r = assign(sid, e1).await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(
        &r.json,
        &[
            "id",
            "userId",
            "companyId",
            "tierId",
            "assignedAt",
            "tier",
            "user",
        ],
    );
    assert_keys(&r.json["tier"], TIER_KEYS);
    assert_keys(&r.json["user"], &["id", "name", "username"]);
    assert_eq!(r.json["tierId"], sid);
    let aid = r.json["id"].clone();
    // Pindah tier = baris sama.
    let r = assign(jid, e1).await;
    assert_eq!(
        (r.json["id"].clone(), r.json["tierId"].clone()),
        (aid, json!(jid))
    );
    // `assign/abc` → NaN → 500.
    let r = app
        .put(&format!("/leave-policies/tiers/{sid}/assign/abc"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 500);

    let r = app
        .get(&format!("/leave-policies/tiers/employees/{e1}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["tier"]["name"], "Junior");
    let r = app
        .get("/leave-policies/tiers")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json[0]["_count"], json!({"employeeTiers": 1}));

    // Hapus tier yang dipakai → 400; setelah dilepas → sukses.
    let r = app
        .delete(&format!("/leave-policies/tiers/{jid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Cannot delete tier: 1 employee(s) are assigned to it"
    );
    let r = app
        .delete(&format!("/leave-policies/tiers/employees/{e1}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"success": true}));
    let r = app
        .delete(&format!("/leave-policies/tiers/employees/{e1}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Employee has no tier assignment");
    // Tanpa penugasan: `null` → 200 tanpa body.
    let r = app
        .get(&format!("/leave-policies/tiers/employees/{e1}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.is_empty(), "{}", r.text());
    let r = app
        .delete(&format!("/leave-policies/tiers/{jid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"success": true}));
    app.delete(&format!("/leave-policies/tiers/{jid}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);

    // Role.
    app.get("/leave-policies/tiers")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.post("/leave-policies/tiers")
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
}

/// Rabu pertama bulan Maret tahun `y` (hari kerja pada pola kerja bawaan).
fn march_wednesday(y: i32) -> NaiveDate {
    (1..=7)
        .filter_map(|d| NaiveDate::from_ymd_opt(y, 3, d))
        .find(|d| d.weekday() == Weekday::Wed)
        .unwrap()
}

#[tokio::test]
async fn balances_initialize_list_and_view_fields() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let fx2 = fixture(&app).await;
    set_joined(&app, fx.id, "2020-01-01").await;
    let year = this_year();
    let next = year + 1;
    put_policy(&app, &fx, json!({"annualLeaveDays": 12, "carryOverEnabled": true, "maxCarryOverDays": 5, "sickLeaveDays": 6}))
        .await
        .assert_status(200);

    // Bulk: semua anggota aktif (admin, manager, staff) dibuat; ulang → semua dilewati.
    let r = app
        .post("/leave-policies/balances/initialize")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json, json!({"initialized": 3, "skipped": 0}));
    let r = app
        .post("/leave-policies/balances/initialize")
        .token(&fx.manager.token)
        .send()
        .await;
    assert_eq!(r.json, json!({"initialized": 0, "skipped": 3}));
    let b: (f64, f64) = sqlx::query_as(&format!(
        r#"SELECT "annualLeaveTotal", "sickLeaveTotal" FROM "leaveBalance"
           WHERE "userId" = {} AND "companyId" = {} AND year = {year}"#,
        fx.staff.id, fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(b, (12.0, 6.0));
    app.post("/leave-policies/balances/initialize")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // Validasi query.
    let r = app
        .post("/leave-policies/balances/initialize?year=abc")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "year must not be greater than 2100",
            "year must not be less than 2020",
            "year must be an integer number"
        ]
    );
    let r = app
        .post("/leave-policies/balances/initialize?year=2019")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), ["year must not be less than 2020"]);

    // Satu karyawan: carry-over dari tahun lalu + riwayat CARRY_OVER/FORFEIT.
    let e1 = employee(&app, fx.id, &format!("Carry {}", fx.id)).await;
    set_joined(&app, fx.id, "2020-01-01").await;
    let prev: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed")
               VALUES ({e1}, {}, {next}, 12, 4) RETURNING id"#,
            fx.id
        ))
        .await;
    let r = app
        .post(&format!(
            "/leave-policies/balances/initialize/{e1}?year={}",
            next + 1
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_keys(&r.json, BALANCE_KEYS);
    assert_eq!(r.json["year"], next + 1);
    assert_eq!(r.json["annualLeaveTotal"], 12);
    assert_eq!(r.json["carryOverDays"], 5);
    assert_eq!(r.json["sickLeaveTotal"], 6);
    let hist: Vec<(String, f64, i32)> = sqlx::query_as(&format!(
        r#"SELECT source::text, delta, "balanceId" FROM "leaveBalanceAdjustment" WHERE "userId" = {e1} ORDER BY id"#
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        hist,
        [
            (
                "CARRY_OVER".to_owned(),
                5.0,
                r.json["id"].as_i64().unwrap() as i32
            ),
            ("FORFEIT".to_owned(), -3.0, prev)
        ]
    );
    let r = app
        .post(&format!(
            "/leave-policies/balances/initialize/{e1}?year={}",
            next + 1
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Balance already initialized for this year"
    );
    // Bukan anggota aktif company peminta (tidak ada / company lain) → 404, tanpa baris saldo.
    let outsider = employee(&app, fx2.id, &format!("Outsider {}", fx2.id)).await;
    for target in [987_654_321, outsider] {
        let r = app
            .post(&format!("/leave-policies/balances/initialize/{target}"))
            .token(&fx.admin.token)
            .send()
            .await;
        r.assert_status(404).assert_no_sql_error();
        assert_eq!(r.json["message"], "User not found in this company");
    }
    let leaked: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leaveBalance" WHERE "userId" = {outsider} AND "companyId" = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(leaked, 0);

    // Daftar tahun depan: override, tier, libur terjadwal.
    let (e2, e3) = (
        employee(&app, fx.id, &format!("Bravo {}", fx.id)).await,
        employee(&app, fx.id, &format!("Alpha {}", fx.id)).await,
    );
    set_joined(&app, fx.id, "2020-01-01").await;
    for e in [e2, e3] {
        app.post(&format!(
            "/leave-policies/balances/initialize/{e}?year={next}"
        ))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(201);
    }
    app.put(&format!("/leave-policies/employees/{e2}"))
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": 20}))
        .send()
        .await
        .assert_status(200);
    let hol = march_wednesday(next);
    let hid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
               VALUES ({}, 'Cuti Bersama', '{hol}', 'JOINT_LEAVE', true) RETURNING id"#,
            fx.id
        ))
        .await;
    // e3 sudah dipotong untuk libur itu → tidak dihitung lagi.
    let e3_bal: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "leaveBalance" WHERE "userId" = {e3} AND year = {next}"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "holidayLeaveDeduction" ("companyId", "holidayId", "userId", "balanceId", "holidayName",
             "holidayDate", status, "intendedDays", "annualDeducted", "appliedAt")
           VALUES ({}, {hid}, {e3}, {e3_bal}, 'Cuti Bersama', '{hol}', 'APPLIED', 1, 1, NOW())"#,
        fx.id
    ))
    .await;

    let r = app
        .get(&format!("/leave-policies/balances?year={next}&limit=2"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["meta"],
        json!({"total": 3, "page": 1, "limit": 2, "totalPages": 2})
    );
    let data = r.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    let mut view_keys: Vec<&str> = BALANCE_KEYS.to_vec();
    view_keys.extend([
        "user",
        "accrualMode",
        "entitlement",
        "scheduledHolidayDays",
        "carryOverAvailable",
        "requestable",
    ]);
    assert_keys(&data[0], &view_keys);
    assert_keys(&data[0]["user"], &["id", "name", "username"]);
    // Urut nama pengguna: 'Alpha' (e3) lalu 'Bravo' (e2).
    assert_eq!(data[0]["userId"], e3);
    assert_eq!(data[1]["userId"], e2);
    assert_eq!(
        data[0]["scheduledHolidayDays"], 0,
        "e3 sudah punya potongan APPLIED"
    );
    assert_eq!(data[1]["scheduledHolidayDays"], 1);
    assert_eq!(data[0]["entitlement"], 12);
    assert_eq!(data[1]["entitlement"], 20, "override karyawan");
    assert_eq!(data[0]["accrualMode"], "UPFRONT");
    assert_eq!(
        data[1]["requestable"],
        data[1]["annualLeaveTotal"].as_f64().unwrap() - 1.0
    );
    // Halaman 2 + pencarian (tanpa memandang huruf besar-kecil, juga pada username).
    let r = app
        .get(&format!(
            "/leave-policies/balances?year={next}&limit=2&page=2"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    let r = app
        .get(&format!(
            "/leave-policies/balances?year={next}&search=BRAVO"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["userId"], e2);
    let r = app
        .get(&format!(
            "/leave-policies/balances?year={next}&search=@test.local"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 3);
    // Anggota yang sudah keluar: baris saldonya tetap ada tetapi tidak tampil (count & data).
    app.exec(&format!(
        r#"UPDATE "userCompany" SET status = 0 WHERE "userId" = {e2} AND "companyId" = {}"#,
        fx.id
    ))
    .await;
    let r = app
        .get(&format!("/leave-policies/balances?year={next}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 2);
    assert!(
        r.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["userId"] != e2)
    );
    let r = app
        .get(&format!(
            "/leave-policies/balances?year={next}&search=BRAVO"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 0);
    app.exec(&format!(
        r#"UPDATE "userCompany" SET status = 1 WHERE "userId" = {e2} AND "companyId" = {}"#,
        fx.id
    ))
    .await;
    // Tier mempengaruhi entitlement bila TIER_BASED.
    put_policy(&app, &fx, json!({"entitlementType": "TIER_BASED"}))
        .await
        .assert_status(200);
    let tier = app
        .post("/leave-policies/tiers")
        .token(&fx.admin.token)
        .json(&json!({"name": "T", "annualLeaveDays": 17}))
        .send()
        .await;
    app.put(&format!(
        "/leave-policies/tiers/{}/assign/{e3}",
        tier.json["id"]
    ))
    .token(&fx.admin.token)
    .send()
    .await
    .assert_status(200);
    let r = app
        .get(&format!(
            "/leave-policies/balances?year={next}&search=Alpha"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["data"][0]["entitlement"], 17);
    // Tahun tanpa saldo → kosong; validasi.
    let r = app
        .get("/leave-policies/balances?year=2021")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"data": [], "meta": {"total": 0, "page": 1, "limit": 20, "totalPages": 0}})
    );
    let r = app
        .get("/leave-policies/balances?page=0&limit=x&year=y&search[]=a")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "page must not be less than 1",
            "limit must not be less than 1",
            "limit must be an integer number",
            "search must be a string",
            "year must not be greater than 2100",
            "year must not be less than 2020",
            "year must be an integer number"
        ]
    );
    // Company lain tidak melihat saldo.
    let r = app
        .get(&format!("/leave-policies/balances?year={next}"))
        .token(&fx2.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 0);
    app.get("/leave-policies/balances")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn balance_adjust_and_history() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let fx2 = fixture(&app).await;
    let year = this_year();
    let e = employee(&app, fx.id, &format!("Adj {}", fx.id)).await;
    let bal: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed",
                 "sickLeaveTotal", "sickLeaveUsed", "carryOverDays", "carryOverUsed")
               VALUES ({e}, {}, {year}, 12, 3, 5, 5, 2, 0) RETURNING id"#,
            fx.id
        ))
        .await;
    let adjust = |token: String, user: i32, body: Value| {
        let app = &app;
        async move {
            app.patch(&format!("/leave-policies/balances/{user}/adjust"))
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };

    // Validasi.
    let r = adjust(fx.admin.token.clone(), e, json!({})).await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "field must be one of the following values: annualLeaveTotal, sickLeaveTotal, carryOverDays",
            "delta must be a number conforming to the specified constraints",
            "reason must be shorter than or equal to 500 characters",
            "reason wajib diisi",
            "reason must be a string",
        ]
    );
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"field": "annualLeaveTotal", "delta": 0, "reason": "x"}),
    )
    .await;
    assert_eq!(messages(&r), ["delta tidak boleh 0"]);
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"year": 2019, "field": "carryOverDays", "delta": 1, "reason": ""}),
    )
    .await;
    assert_eq!(
        messages(&r),
        ["year must not be less than 2020", "reason wajib diisi"]
    );

    // 404 belum diinisialisasi (tahun lain) dan karyawan company lain.
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"year": year - 1, "field": "annualLeaveTotal", "delta": 1, "reason": "x"}),
    )
    .await;
    r.assert_status(404);
    assert_eq!(
        r.json["message"],
        "Saldo cuti karyawan untuk tahun ini belum diinisialisasi"
    );
    adjust(
        fx2.admin.token.clone(),
        e,
        json!({"field": "annualLeaveTotal", "delta": 1, "reason": "x"}),
    )
    .await
    .assert_status(404);

    // Aturan nilai: tidak negatif, tidak di bawah yang terpakai.
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"field": "carryOverDays", "delta": -3, "reason": "x"}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Nilai carryOverDays tidak boleh negatif setelah adjustment"
    );
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"field": "annualLeaveTotal", "delta": -10, "reason": "x"}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Tidak dapat mengurangi annualLeaveTotal di bawah jumlah yang sudah terpakai (3 hari)"
    );
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"field": "sickLeaveTotal", "delta": -0.5, "reason": "x"}),
    )
    .await;
    assert_eq!(
        r.json["message"],
        "Tidak dapat mengurangi sickLeaveTotal di bawah jumlah yang sudah terpakai (5 hari)"
    );
    // Tidak ada yang berubah setelah penolakan.
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leaveBalanceAdjustment" WHERE "balanceId" = {bal}"#
        ))
        .await;
    assert_eq!(n, 0);

    // Sukses: nilai absolut baru + riwayat ADJUSTMENT atas nama pelaku.
    let r = adjust(
        fx.admin.token.clone(),
        e,
        json!({"field": "annualLeaveTotal", "delta": "2.5", "reason": "Bonus lembur"}),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    let mut keys_expected: Vec<&str> = BALANCE_KEYS.to_vec();
    keys_expected.push("user");
    assert_keys(&r.json, &keys_expected);
    assert_eq!(r.json["annualLeaveTotal"], 14.5);
    assert_eq!(r.json["user"]["id"], e);
    assert!(r.json["updatedAt"].is_string());
    let r = adjust(
        fx.manager.token.clone(),
        e,
        json!({"year": year, "field": "carryOverDays", "delta": -2, "reason": "Hangus"}),
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.json["carryOverDays"], 0);
    adjust(
        fx.staff.token.clone(),
        e,
        json!({"field": "carryOverDays", "delta": 1, "reason": "x"}),
    )
    .await
    .assert_status(401);

    // Riwayat: terbaru dulu, bentuk lengkap, penyaring tahun.
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason, source, "createdAt")
           VALUES ({bal}, {e}, {}, 'annualLeaveUsed', 1, 'Potong otomatis', 'AUTO_DEDUCTION', NOW() - interval '1 day')"#,
        fx.id
    ))
    .await;
    let r = app
        .get(&format!("/leave-policies/balances/{e}/adjustments"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let rows = r.json.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_keys(
        &rows[0],
        &[
            "id",
            "balanceId",
            "userId",
            "companyId",
            "adjustedById",
            "field",
            "delta",
            "reason",
            "source",
            "leaveRequestId",
            "dedupeKey",
            "holidayId",
            "intendedDelta",
            "createdAt",
            "adjustedBy",
            "holiday",
            "leaveRequest",
        ],
    );
    assert_eq!(rows[0]["field"], "carryOverDays");
    assert_eq!(rows[0]["delta"], -2);
    assert_eq!(rows[0]["source"], "ADJUSTMENT");
    assert_eq!(rows[0]["adjustedById"], fx.manager.id);
    assert_eq!(rows[0]["adjustedBy"]["id"], fx.manager.id);
    assert_eq!(rows[1]["delta"], 2.5);
    assert_eq!(rows[1]["reason"], "Bonus lembur");
    assert_eq!(rows[2]["source"], "AUTO_DEDUCTION");
    assert!(
        rows[2]["adjustedBy"].is_null()
            && rows[2]["holiday"].is_null()
            && rows[2]["leaveRequest"].is_null()
    );

    // Relasi: libur + pengajuan cuti ikut di-include.
    let hid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "companyHoliday" ("companyId", name, date, type) VALUES ({}, 'Libur X', '{year}-08-17', 'NATIONAL_HOLIDAY') RETURNING id"#,
            fx.id
        ))
        .await;
    let ltype: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "companyLeaveType" ("companyId", name, code) VALUES ({}, 'Tahunan', 'TH_{}') RETURNING id"#,
            fx.id, fx.id
        ))
        .await;
    let (lid, luuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "leaveTypeId", "startDate", "endDate", "totalDays", reason, status, "leaveType")
           VALUES ({e}, {}, {ltype}, '{year}-07-10', '{year}-07-12', 3, 'x', 'APPROVED', 'ANNUAL_LEAVE') RETURNING id, uuid::text"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason, source, "holidayId", "intendedDelta", "leaveRequestId", "createdAt")
           VALUES ({bal}, {e}, {}, 'annualLeaveUsed', 1, 'rel', 'HOLIDAY_DEDUCTION', {hid}, 1.5, {lid}, NOW() + interval '1 day')"#,
        fx.id
    ))
    .await;
    let r = app
        .get(&format!(
            "/leave-policies/balances/{e}/adjustments?year={year}"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let first = &r.json[0];
    assert_eq!(first["source"], "HOLIDAY_DEDUCTION");
    assert_eq!(first["intendedDelta"], 1.5);
    assert_keys(&first["holiday"], &["id", "name", "date"]);
    assert_eq!(
        first["holiday"]["date"],
        format!("{year}-08-17T00:00:00.000Z")
    );
    assert_keys(
        &first["leaveRequest"],
        &[
            "id",
            "uuid",
            "startDate",
            "endDate",
            "totalDays",
            "leaveType",
            "leaveTypeConfig",
        ],
    );
    assert_eq!(first["leaveRequest"]["uuid"], luuid);
    assert_eq!(
        first["leaveRequest"]["startDate"],
        format!("{year}-07-10T00:00:00.000Z")
    );
    assert_eq!(first["leaveRequest"]["totalDays"], 3);
    assert_eq!(first["leaveRequest"]["leaveType"], "ANNUAL_LEAVE");
    assert_eq!(
        first["leaveRequest"]["leaveTypeConfig"],
        json!({"name": "Tahunan", "code": format!("TH_{}", fx.id)})
    );
    // Tahun lain → kosong; company lain → kosong.
    let r = app
        .get(&format!(
            "/leave-policies/balances/{e}/adjustments?year={}",
            year - 1
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json, json!([]));
    let r = app
        .get(&format!("/leave-policies/balances/{e}/adjustments"))
        .token(&fx2.admin.token)
        .send()
        .await;
    assert_eq!(r.json, json!([]));
    let r = app
        .get(&format!(
            "/leave-policies/balances/{e}/adjustments?year=abc"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        app.get("/leave-policies/balances/abc/adjustments")
            .token(&fx.admin.token)
            .send()
            .await
            .status
            .as_u16(),
        500
    );
}

// c7e3aaf: adjust total/bawaan tahun Y menghitung ulang bawaan Y+1 dan catatan hangus Y.
#[tokio::test]
async fn adjust_recomputes_next_year_carry_over() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    put_policy(
        &app,
        &fx,
        json!({"carryOverEnabled": true, "maxCarryOverDays": 5}),
    )
    .await
    .assert_status(200);
    let year = this_year();
    let e = employee(&app, fx.id, &format!("Carry adj {}", fx.id)).await;
    let cur: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed")
               VALUES ({e}, {}, {year}, 12, 10) RETURNING id"#,
            fx.id
        ))
        .await;
    let next: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "carryOverDays")
               VALUES ({e}, {}, {}, 12, 2) RETURNING id"#,
            fx.id,
            year + 1
        ))
        .await;
    let r = app
        .patch(&format!("/leave-policies/balances/{e}/adjust"))
        .token(&fx.admin.token)
        .json(&json!({"field": "annualLeaveTotal", "delta": 4, "reason": "Koreksi"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["annualLeaveTotal"], 16);
    // Sisa Y = 6 → bawaan Y+1 = min(6, 5) = 5; 1 hari hangus.
    let carry: f64 = app
        .scalar(&format!(
            r#"SELECT "carryOverDays" FROM "leaveBalance" WHERE id = {next}"#
        ))
        .await;
    assert_eq!(carry, 5.0);
    let rows: Vec<(i32, String, f64, String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "balanceId", field, delta, source::text, reason, "dedupeKey"
             FROM "leaveBalanceAdjustment" WHERE "userId" = $1 AND source <> 'ADJUSTMENT' ORDER BY id"#,
    )
    .bind(e)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                next,
                "carryOverDays".to_owned(),
                3.0,
                "CARRY_OVER".to_owned(),
                format!("Koreksi sisa cuti dibawa: saldo {year} berubah"),
                None
            ),
            (
                cur,
                "annualLeaveTotal".to_owned(),
                -1.0,
                "FORFEIT".to_owned(),
                "Sisa cuti hangus (melebihi batas bawa 5 hari)".to_owned(),
                Some(format!("forfeit:{cur}"))
            ),
        ]
    );

    // sickLeaveTotal tidak menyentuh bawaan.
    app.patch(&format!("/leave-policies/balances/{e}/adjust"))
        .token(&fx.admin.token)
        .json(&json!({"field": "sickLeaveTotal", "delta": 1, "reason": "x"}))
        .send()
        .await
        .assert_status(200);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leaveBalanceAdjustment" WHERE "userId" = {e} AND source <> 'ADJUSTMENT'"#
        ))
        .await;
    assert_eq!(n, 2);
}
