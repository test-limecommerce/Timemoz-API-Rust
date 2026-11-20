//! Uji integrasi alur keputusan cuti (dasbor `/leaves`): persetujuan + potong saldo (bawaan
//! dulu, lalu tahunan; sakit LIMITED; jenis lama), riwayat penyesuaian, koreksi bawaan tahun
//! berikutnya, rollback saat saldo kurang, klaim bersamaan, hook lembur, tolak, minta revisi,
//! visibilitas approver. Postgres nyata (`mz-testkit`), GCS = emulator lokal.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn approve_deducts_carry_then_annual_with_ledger() {
    let Some((app, _em)) = app("flow_a").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    let b = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 1.0, 0.0, 0.0).await;
    let mon = first_monday(y, 2);
    // Senin–Rabu, tapi Selasa libur aktif → 2 hari (totalDays tersimpan 3 dihitung ulang).
    app.exec(&format!(
        r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
           VALUES ({}, 'Libur', '{}', 'NATIONAL_HOLIDAY', false)"#,
        fx.id,
        key(plus(mon, 1))
    ))
    .await;
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 2),
        3.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    // Lembur yang belum selesai pada hari cuti penuh ikut dibatalkan (FR-012).
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
             reason, status, "retroWindowDays", "responseDeadline", "finalAt")
           VALUES ({c}, {u}, {u}, 'SELF', '{d}', 3600, 'x', 'PENDING_APPROVAL', 7, '{d}', '{d}')"#,
        c = fx.id,
        u = fx.staff.id,
        d = key(mon)
    ))
    .await;

    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .json(&json!({ "approvalNote": "ok" }))
        .send()
        .await;
    res.assert_status(200);
    let j = &res.json;
    assert_eq!(j["status"], "APPROVED");
    assert_eq!(j["totalDays"], 2);
    assert_eq!(j["balanceBefore"], 13);
    assert_eq!(j["balanceAfter"], 11);
    assert_eq!(j["approvalNote"], "ok");
    assert_eq!(j["reviewedById"], fx.admin.id);
    assert_eq!(j["user"]["id"], fx.staff.id);
    assert_eq!(j["leaveTypeConfig"]["id"], t);
    assert!(j.get("approver").is_none() && j.get("reviewedBy").is_none());
    let keys: Vec<&String> = j.as_object().unwrap().keys().collect();
    assert_eq!(keys.first().map(|k| k.as_str()), Some("id"));
    assert_eq!(keys.last().map(|k| k.as_str()), Some("leaveTypeConfig"));

    assert_eq!(bal_row(&app, b).await, (12.0, 1.0, 1.0, 1.0, 0.0));
    let adj = adjustments(&app, fx.staff.id).await;
    let reason = format!("Potong otomatis: pengajuan cuti #{} disetujui", r.id);
    assert_eq!(
        adj,
        vec![
            (
                "carryOverUsed".into(),
                1.0,
                "AUTO_DEDUCTION".into(),
                Some(fx.admin.id),
                Some(r.id),
                reason.clone()
            ),
            (
                "annualLeaveUsed".into(),
                1.0,
                "AUTO_DEDUCTION".into(),
                Some(fx.admin.id),
                Some(r.id),
                reason
            ),
        ]
    );
    let ot: (String, Option<String>) = sqlx::query_as(
        r#"SELECT status::text, "cancelReason"::text FROM "overtimeRequest" WHERE "userId" = $1"#,
    )
    .bind(fx.staff.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(ot, ("CANCELLED".into(), Some("EMPLOYEE_ON_LEAVE".into())));

    // Sudah disetujui → 400 (cek status sebelum transaksi).
    let again = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    again.assert_status(400);
    assert_eq!(
        again.json["message"],
        "Only pending or needs-revision requests can be approved"
    );
    // Tolak yang sudah disetujui → 400.
    let rej = app
        .put(&format!("/leaves/{}/reject", r.uuid))
        .token(&fx.admin.token)
        .json(&json!({ "rejectionReason": "x" }))
        .send()
        .await;
    rej.assert_status(400);
    assert_eq!(
        rej.json["message"],
        "Cannot reject an already approved request"
    );
}

#[tokio::test]
async fn approve_insufficient_rolls_back_claim() {
    let Some((app, _em)) = app("flow_b").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    let b = balance(&app, fx.id, fx.staff.id, y, 12.0, 11.0, 0.0, 0.0, 0.0).await;
    let mon = first_monday(y, 3);
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 1),
        2.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Insufficient annual leave balance. Available: 1 day(s)."
    );
    // Klaim (status APPROVED) ikut dibatalkan.
    assert_eq!(status_of(&app, r.id).await, "PENDING");
    assert_eq!(bal_row(&app, b).await.1, 11.0);
    assert!(adjustments(&app, fx.staff.id).await.is_empty());

    // Rentang tanpa hari kerja (Sabtu–Minggu) → 400 berkode.
    let sat = plus(mon, 5);
    let w = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        sat,
        plus(sat, 1),
        2.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", w.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({ "code": "LEAVE_NO_WORKING_DAYS", "message": "Rentang tanggal tidak berisi hari kerja" })
    );
    assert_eq!(status_of(&app, w.id).await, "PENDING");
}

#[tokio::test]
async fn approve_sick_limited_and_legacy_types() {
    let Some((app, _em)) = app("flow_c").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    app.exec(&format!(
        r#"UPDATE "leavePolicy" SET "sickLeaveMode" = 'LIMITED', "sickLeaveDays" = 5 WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    let y = this_year();
    let sick = leave_type(&app, fx.id, true, "SICK", false, None).await;
    let b = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 0.0, 5.0, 4.0).await;
    let mon = first_monday(y, 4);
    let two = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(sick),
        None,
        mon,
        plus(mon, 1),
        2.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", two.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Insufficient sick leave balance. Available: 1 day(s)."
    );

    let one = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(sick),
        None,
        plus(mon, 7),
        plus(mon, 7),
        1.0,
        "NEEDS_REVISION",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", one.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        (
            res.json["balanceBefore"].clone(),
            res.json["balanceAfter"].clone()
        ),
        (json!(1), json!(0))
    );
    assert_eq!(bal_row(&app, b).await.4, 5.0);
    assert_eq!(adjustments(&app, fx.staff.id).await[0].0, "sickLeaveUsed");

    // Jenis lama ANNUAL_LEAVE (tanpa companyLeaveType) memotong tahunan; UNPAID tidak memotong.
    let legacy = request(
        &app,
        fx.id,
        fx.staff.id,
        None,
        Some("ANNUAL_LEAVE"),
        plus(mon, 14),
        plus(mon, 14),
        1.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    app.put(&format!("/leaves/{}/approve", legacy.uuid))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let unpaid = request(
        &app,
        fx.id,
        fx.staff.id,
        None,
        Some("UNPAID_LEAVE"),
        plus(mon, 15),
        plus(mon, 15),
        1.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", unpaid.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["balanceBefore"], res.json["balanceAfter"]);
    assert_eq!(bal_row(&app, b).await.1, 1.0);
    assert_eq!(adjustments(&app, fx.staff.id).await.len(), 2);
}

#[tokio::test]
async fn late_prior_year_approval_corrects_next_year_carry_over() {
    let Some((app, _em)) = app("flow_d").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    app.exec(&format!(
        r#"UPDATE "leavePolicy" SET "carryOverEnabled" = true, "maxCarryOverDays" = 5 WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    let y = this_year();
    let prev = balance(&app, fx.id, fx.staff.id, y - 1, 12.0, 4.0, 0.0, 0.0, 0.0).await;
    let next = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 5.0, 0.0, 0.0).await;
    let mon = first_monday(y - 1, 2);
    // Senin s.d. Senin berikutnya = 6 hari kerja.
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        None,
        Some("ANNUAL_LEAVE"),
        mon,
        plus(mon, 7),
        6.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["totalDays"], 6);
    assert_eq!(bal_row(&app, prev).await.1, 10.0);
    // Sisa Y-1 tinggal 2 → bawaan Y dipangkas 5 → 2.
    assert_eq!(bal_row(&app, next).await.2, 2.0);
    let adj = adjustments(&app, fx.staff.id).await;
    let last = adj.last().unwrap();
    assert_eq!(
        (last.0.as_str(), last.1, last.2.as_str(), last.3, last.4),
        ("carryOverDays", -3.0, "CARRY_OVER", None, None)
    );
    assert_eq!(
        last.5,
        format!(
            "Koreksi sisa cuti dibawa: pengajuan #{} tahun {} disetujui",
            r.id,
            y - 1
        )
    );
}

#[tokio::test]
async fn concurrent_approvals_deduct_once() {
    let Some((app, _em)) = app("flow_e").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    let b = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 0.0, 0.0, 0.0).await;
    policy_approver(&app, fx.id, fx.admin.id).await;
    policy_approver(&app, fx.id, fx.super_admin.id).await;
    let mon = first_monday(y, 5);
    // Tanpa approver (data lama) → semua approver kebijakan melihatnya.
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 1),
        2.0,
        "PENDING",
        None,
    )
    .await;
    let uri = format!("/leaves/{}/approve", r.uuid);
    let (a, b2) = tokio::join!(
        app.put(&uri).token(&fx.admin.token).send(),
        app.put(&uri).token(&fx.super_admin.token).send()
    );
    let mut codes = [a.status.as_u16(), b2.status.as_u16()];
    codes.sort_unstable();
    assert_eq!(codes[0], 200, "{:?} {:?}", a.json, b2.json);
    assert!(codes[1] == 409 || codes[1] == 400, "{codes:?}");
    for res in [&a, &b2] {
        if res.status.as_u16() == 409 {
            assert_eq!(
                res.json,
                json!({ "code": "LEAVE_ALREADY_PROCESSED", "message": "Pengajuan cuti sudah diproses" })
            );
        }
    }
    assert_eq!(bal_row(&app, b).await.1, 2.0);
    assert_eq!(adjustments(&app, fx.staff.id).await.len(), 1);
}

#[tokio::test]
async fn reject_revision_visibility_and_validation() {
    let Some((app, _em)) = app("flow_f").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, false, "ANNUAL", false, None).await;
    let mon = first_monday(y, 6);
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        mon,
        1.0,
        "PENDING",
        Some(fx.manager.id),
    )
    .await;

    // Bukan approver yang ditugaskan → 404 (detail, approve, reject, dokumen).
    for (m, path) in [
        ("GET", format!("/leaves/{}", r.uuid)),
        ("PUT", format!("/leaves/{}/approve", r.uuid)),
        ("GET", format!("/leaves/{}/document", r.uuid)),
    ] {
        let req = if m == "GET" {
            app.get(&path)
        } else {
            app.put(&path)
        };
        let res = req.token(&fx.admin.token).send().await;
        res.assert_status(404);
    }
    // STAFF tidak boleh (RolesGuard Node: 401 "Access denied: insufficient permissions").
    let denied = app
        .get(&format!("/leaves/{}", r.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    denied.assert_status(401);
    assert_eq!(
        denied.json["message"],
        "Access denied: insufficient permissions"
    );
    app.get(&format!("/leaves/{}", r.uuid))
        .send()
        .await
        .assert_status(401);
    // UUID bukan UUID → 500 (Prisma P2023 di Node).
    let bad = app
        .get("/leaves/not-a-uuid")
        .token(&fx.manager.token)
        .send()
        .await;
    bad.assert_status(500);

    // Detail oleh approver.
    let res = app
        .get(&format!("/leaves/{}", r.uuid))
        .token(&fx.manager.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["approver"]["id"], fx.manager.id);
    assert_eq!(res.json["reviewedBy"], Value::Null);
    // Tanpa dokumen → 404 + header Cache-Control tetap dipasang (@Header).
    let doc = app
        .get(&format!("/leaves/{}/document", r.uuid))
        .token(&fx.manager.token)
        .send()
        .await;
    doc.assert_status(404);
    assert_eq!(doc.json["message"], "Document not found");

    // Validasi body reject.
    let v = app
        .put(&format!("/leaves/{}/reject", r.uuid))
        .token(&fx.manager.token)
        .json(&json!({ "rejectionReason": 5 }))
        .send()
        .await;
    v.assert_status(400);
    assert_eq!(
        v.json["message"],
        json!([
            "rejectionReason must be shorter than or equal to 1000 characters",
            "rejectionReason must be a string"
        ])
    );

    // Minta revisi → NEEDS_REVISION; ulang → 400.
    let res = app
        .put(&format!("/leaves/{}/needs-revision", r.uuid))
        .token(&fx.manager.token)
        .json(&json!({ "revisionNote": "perbaiki" }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["status"], "NEEDS_REVISION");
    assert_eq!(res.json["revisionNote"], "perbaiki");
    assert_eq!(res.json["reviewedById"], fx.manager.id);
    let again = app
        .put(&format!("/leaves/{}/needs-revision", r.uuid))
        .token(&fx.manager.token)
        .send()
        .await;
    again.assert_status(400);
    assert_eq!(
        again.json["message"],
        "Only pending requests can be marked as needs revision"
    );

    // Tolak (NEEDS_REVISION boleh ditolak).
    let res = app
        .put(&format!("/leaves/{}/reject", r.uuid))
        .token(&fx.manager.token)
        .json(&json!({ "rejectionReason": "tidak bisa" }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["status"], "REJECTED");
    assert_eq!(res.json["rejectionReason"], "tidak bisa");
    assert!(res.json["reviewedAt"].is_string());
}

/// Hari kerja Senin–Jumat di `[from, to]` (tanpa libur), untuk ekspektasi lintas tahun.
fn weekdays(from: chrono::NaiveDate, to: chrono::NaiveDate) -> f64 {
    use chrono::Datelike;
    #[allow(clippy::cast_precision_loss)]
    let n = from
        .iter_days()
        .take_while(|d| *d <= to)
        .filter(|d| d.weekday().number_from_monday() <= 5)
        .count() as f64;
    n
}

// 677a060: cuti 31 Des / 1 Jan dipotong per tahun; baris Y+1 dibuat di dalam transaksi
// persetujuan; catatan hangus Y dikoreksi oleh recomputeNextYearCarryOver.
#[tokio::test]
async fn approve_cross_year_charges_each_year() {
    let Some((app, _em)) = app("flow_x1").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    let prev = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 0.0, 0.0, 0.0).await;
    let start = chrono::NaiveDate::from_ymd_opt(y, 12, 29).unwrap();
    let end = chrono::NaiveDate::from_ymd_opt(y + 1, 1, 5).unwrap();
    let d1 = weekdays(start, chrono::NaiveDate::from_ymd_opt(y, 12, 31).unwrap());
    let d2 = weekdays(chrono::NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap(), end);
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        start,
        end,
        d1 + d2,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["totalDays"].as_f64(), Some(d1 + d2));
    // balanceBefore/After = saldo tahun mulai.
    assert_eq!(res.json["balanceBefore"], 12);
    assert_eq!(res.json["balanceAfter"].as_f64(), Some(12.0 - d1));
    assert_eq!(bal_row(&app, prev).await.1, d1);
    let next: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "leaveBalance" WHERE "userId" = {} AND "companyId" = {} AND year = {}"#,
            fx.staff.id,
            fx.id,
            y + 1
        ))
        .await;
    assert_eq!(bal_row(&app, next).await.1, d2);

    let adj = adjustments(&app, fx.staff.id).await;
    let auto: Vec<f64> = adj
        .iter()
        .filter(|a| a.2 == "AUTO_DEDUCTION")
        .map(|a| a.1)
        .collect();
    assert_eq!(auto, vec![d1, d2]);
    // Bawa sisa tidak aktif → seluruh sisa Y hangus, dikoreksi sesudah potongan Y.
    let forfeit: Vec<f64> = adj
        .iter()
        .filter(|a| a.2 == "FORFEIT")
        .map(|a| a.1)
        .collect();
    assert_eq!(forfeit, vec![-(12.0 - d1)]);
}

// 677a060: tumpang tindih dengan cuti APPROVED dicek ulang di bawah kunci saldo; klaim
// di-rollback (status tetap PENDING).
#[tokio::test]
async fn approve_rejects_overlap_with_approved() {
    let Some((app, _em)) = app("flow_x2").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    let b = balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 0.0, 0.0, 0.0).await;
    let mon = first_monday(y, 3);
    request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 1),
        2.0,
        "APPROVED",
        Some(fx.admin.id),
    )
    .await;
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        plus(mon, 1),
        plus(mon, 2),
        2.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({
            "code": "LEAVE_OVERLAP",
            "message": format!(
                "Tanggal cuti bertabrakan dengan pengajuan lain ({} s.d. {}, status APPROVED)",
                key(mon),
                key(plus(mon, 1))
            ),
        })
    );
    assert_eq!(status_of(&app, r.id).await, "PENDING");
    assert_eq!(bal_row(&app, b).await.1, 0.0);

    // Sore & pagi di tanggal yang sama tidak bentrok.
    let r2 = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        plus(mon, 7),
        plus(mon, 7),
        0.5,
        "APPROVED",
        Some(fx.admin.id),
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "startDayType" = 'HALF_DAY_MORNING' WHERE id = {}"#,
        r2.id
    ))
    .await;
    let r3 = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        plus(mon, 7),
        plus(mon, 7),
        0.5,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "startDayType" = 'HALF_DAY_AFTERNOON' WHERE id = {}"#,
        r3.id
    ))
    .await;
    app.put(&format!("/leaves/{}/approve", r3.uuid))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
}

// 677a060: sakit DEDUCT_ANNUAL mengikuti aturan tahunan (bawaan dulu) dan bawaan Y+1 dikoreksi
// walau Y masih tahun berjalan.
#[tokio::test]
async fn sick_deduct_annual_uses_carry_and_recomputes_next_year() {
    let Some((app, _em)) = app("flow_x3").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    app.exec(&format!(
        r#"UPDATE "leavePolicy" SET "carryOverEnabled" = true, "maxCarryOverDays" = 5 WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    let y = this_year();
    let t = leave_type(&app, fx.id, true, "SICK", false, None).await;
    let cur = balance(&app, fx.id, fx.staff.id, y, 12.0, 6.0, 2.0, 0.0, 0.0).await;
    let next = balance(&app, fx.id, fx.staff.id, y + 1, 12.0, 0.0, 5.0, 0.0, 0.0).await;
    let mon = first_monday(y, 4);
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 2),
        3.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/approve", r.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    // Bawaan 2 dulu, lalu 1 dari tahunan; sickLeaveUsed tak tersentuh.
    let (_, used, _, carry_used, sick_used) = bal_row(&app, cur).await;
    assert_eq!((used, carry_used, sick_used), (7.0, 2.0, 0.0));
    // Sisa tahunan Y = 12 - 7 = 5 → bawaan Y+1 tetap 5 (batas 5); tak ada koreksi.
    assert_eq!(bal_row(&app, next).await.2, 5.0);

    // Pengajuan kedua menurunkan sisa Y ke 3 → bawaan Y+1 dikoreksi ke 3 (tahun berjalan pun).
    let r2 = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        plus(mon, 7),
        plus(mon, 8),
        2.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    app.put(&format!("/leaves/{}/approve", r2.uuid))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(bal_row(&app, next).await.2, 3.0);
    let last = adjustments(&app, fx.staff.id).await.pop().unwrap();
    assert_eq!(
        (last.0.as_str(), last.1, last.2.as_str()),
        ("carryOverDays", -2.0, "CARRY_OVER")
    );
}

// 677a060: REJECTED (termasuk "Cancelled by employee") tak bisa ditolak ulang; pembatalan
// pengajuan yang sudah APPROVED ditolak.
#[tokio::test]
async fn reject_only_reviewable_and_cancel_claims() {
    let Some((app, _em)) = app("flow_x4").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, false, "ANNUAL", false, None).await;
    let mon = first_monday(y, 6);
    let r = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        mon,
        1.0,
        "REJECTED",
        Some(fx.admin.id),
    )
    .await;
    let res = app
        .put(&format!("/leaves/{}/reject", r.uuid))
        .token(&fx.admin.token)
        .json(&json!({ "rejectionReason": "lagi" }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Only pending or needs-revision requests can be rejected"
    );

    let p = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        plus(mon, 7),
        plus(mon, 7),
        1.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;
    app.delete(&format!("/mobile/leaves/{}", p.uuid))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(status_of(&app, p.id).await, "REJECTED");
}
