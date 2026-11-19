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
