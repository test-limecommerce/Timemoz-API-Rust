//! Uji integrasi `mz-timeconfig` (32 route) terhadap Postgres NYATA.
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_timeconfig`
//!
//! Tidak tercakup (butuh GCS / jaringan luar): sukses `GET /manual-time-approvals/attachments/{uuid}/file`
//! dan `…/file/url` untuk pemilik/approver (menandatangani URL / streaming dari GCS), sukses
//! `GET /app-releases/download/...` untuk baris yang ada (stream GCS), `DELETE /admin/app-releases/{uuid}`
//! untuk baris yang ada (hapus objek GCS dulu), dan upload berkas sungguhan `POST /admin/app-releases/upload`
//! (unggah GCS). Bagian DB dan validasi tiap route tetap diuji.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};

const MISSING: &str = "00000000-0000-4000-8000-00000000dead";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn next() -> i64 {
    i64::from(SEQ.fetch_add(1, Ordering::Relaxed))
}

fn uniq(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn uuids(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x["uuid"].as_str().map(str::to_owned))
        .collect()
}

fn location(r: &TestResponse) -> String {
    r.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Proyek baru yang dimiliki company fixture (status aktif).
struct Proj {
    id: i32,
    uuid: String,
    name: String,
}

async fn project(app: &TestApp, fx: &Company) -> Proj {
    let company = fx.id;
    let code = uniq("PRJ");
    let type_code = uniq("PTY");
    let type_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "projectType" (code, name, status) VALUES ('{type_code}', '{type_code}', 1) RETURNING id"#
        ))
        .await;
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
               VALUES ('{code}', '{code}', 5, {type_id}, '2100-01-01', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({id}, {company}, 1)"#
    ))
    .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM project WHERE id = {id}"))
        .await;
    Proj {
        id,
        uuid,
        name: code,
    }
}

/// Penyetuju aktif di proyek (`projectManualTimeApprover`).
async fn set_approver(app: &TestApp, project_id: i32, user_id: i32) {
    app.exec(&format!(
        r#"INSERT INTO "projectManualTimeApprover" ("projectId", "userId", status) VALUES ({project_id}, {user_id}, 1)
           ON CONFLICT ("projectId", "userId") DO UPDATE SET status = 1"#
    ))
    .await;
}

struct Tl {
    id: i32,
    uuid: String,
    tracking_id: i32,
}

/// Timeline pada slot waktu unik (2100 + `slot` x 3 hari), durasi `dur_min` menit mulai
/// `offset_min` menit dari slot. Slot berbeda tidak pernah tumpang tindih (margin 1 hari).
/// `manual = Some("PENDING")` membuat baris manual time (`isOffline`), `None` = timeline biasa.
async fn timeline(
    app: &TestApp,
    owner: i32,
    project_id: i32,
    slot: i64,
    offset_min: i64,
    dur_min: i64,
    manual: Option<&str>,
) -> Tl {
    let start = format!(
        "(TIMESTAMP '2100-01-01 09:00:00' + {slot} * INTERVAL '3 days' + {offset_min} * INTERVAL '1 minute')"
    );
    let end = format!("({start} + {dur_min} * INTERVAL '1 minute')");
    let tracking_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", "totalTakenInSecond", status)
               VALUES (({start})::date, {owner}, 0, 1)
               ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1
               RETURNING id"#
        ))
        .await;
    let is_offline = manual.is_some();
    let manual_sql = manual.map_or("NULL".to_owned(), |m| {
        format!("'{m}'::\"ManualTimeStatus\"")
    });
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
                   timezone, "trackingId", "projectId", status, "manualTimeStatus", reason, "requestedAt")
               VALUES ('it note', {is_offline}, {start}, {end}, {start}, {end}, 'Asia/Jakarta',
                   {tracking_id}, {project_id}, 1, {manual_sql}, 'it reason', NOW())
               RETURNING id"#
        ))
        .await;
    let uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "trackingTimeline" WHERE id = {id}"#
        ))
        .await;
    Tl {
        id,
        uuid,
        tracking_id,
    }
}

async fn status_of(app: &TestApp, timeline_id: i32) -> String {
    app.scalar(&format!(
        r#"SELECT "manualTimeStatus"::text FROM "trackingTimeline" WHERE id = {timeline_id}"#
    ))
    .await
}

/// Token sekali pakai untuk tautan email (`manualTimeApprovalToken`).
async fn email_token(app: &TestApp, tl: &Tl, approver: i32, expires: &str) -> String {
    app.scalar(&format!(
        r#"INSERT INTO "manualTimeApprovalToken" ("trackingTimelineId", "approverId", "expiredAt")
           VALUES ({}, {approver}, {expires}) RETURNING uuid::text"#,
        tl.id
    ))
    .await
}

/// Operator aktif/nonaktif + token JWT operator (`sub` = id).
async fn operator(app: &TestApp, status: i32) -> (i32, String) {
    let username = uniq("op");
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status)
               VALUES ('{username}', 'x', 'IT Operator', {status}) RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

/// Token SYSTEM_ADMIN (rute `project-types` tulis hanya untuk role ini). Template harus
/// punya user aktif dengan access level `SYSTEM_ADMIN` di company fixture.
async fn system_admin_token(app: &TestApp, fx: &Company) -> String {
    let company = fx.id;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT u.id FROM users u
           JOIN "userCompany" uc ON uc."userId" = u.id AND uc.status = 1 AND uc."companyId" = $1
           JOIN "userAccessLevel" ual ON ual."userId" = u.id AND ual.status = 1 AND ual."companyId" = $1
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE u.status = 1 AND al.code = 'SYSTEM_ADMIN'
           ORDER BY u.id LIMIT 1"#,
    )
    .bind(company)
    .fetch_optional(app.db())
    .await
    .unwrap_or_else(|e| panic!("fixture SYSTEM_ADMIN: {e}"));
    let id = match id {
        Some(id) => id,
        // Template tidak punya SYSTEM_ADMIN: buat satu per pemanggilan (unik,
        // supaya test paralel tidak bentrok).
        None => {
            let tag = uuid::Uuid::new_v4().simple().to_string();
            let id: i32 = app
                .scalar(&format!(
                    r#"INSERT INTO users (username, password, name, status)
                       VALUES ('it.sysadmin.{tag}@test.local', 'x', 'IT System Admin', 1) RETURNING id"#
                ))
                .await;
            app.exec(&format!(
                r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isDefault", "idleTimeInSecond")
                     VALUES ({id}, {company}, 1, true, 300);
                   INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
                     SELECT {id}, al.id, {company}, 1 FROM "accessLevel" al
                     WHERE al.code = 'SYSTEM_ADMIN' AND al."isSystem";"#
            ))
            .await;
            id
        }
    };
    app.token(id)
}

/// Tunggu baris audit muncul (flush asinkron).
async fn wait_count(app: &TestApp, sql: &str, want: i64) -> i64 {
    let mut n = 0;
    for _ in 0..40 {
        n = app.scalar::<i64>(sql).await;
        if n >= want {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    n
}

fn multipart(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    const B: &str = "----mzitboundary7d1f";
    let mut body = String::new();
    for (k, v) in fields {
        body.push_str(&format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
        ));
    }
    body.push_str(&format!("--{B}--\r\n"));
    (
        format!("multipart/form-data; boundary={B}"),
        body.into_bytes(),
    )
}

// ───────────────────────────── manual-time-approvals ─────────────────────────────

#[tokio::test]
async fn approve_and_reject_from_dashboard() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    set_approver(&app, p.id, fx.staff.id).await;

    // Pending milik staff2 muncul di my-pending dengan kolom lengkap.
    let t = timeline(&app, fx.staff2.id, p.id, next(), 0, 120, Some("PENDING")).await;
    let res = app
        .get("/manual-time-approvals/my-pending")
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert!(uuids(res.data()).contains(&t.uuid), "{}", res.text());

    // Approve oleh bukan approver ditolak (200 + success:false, seperti Node).
    let url = format!("/manual-time-approvals/{}/approve", t.uuid);
    let res = app.patch(&url).token(&fx.staff2.token).send().await;
    res.assert_status(200);
    assert_eq!(
        res.json["message"],
        "You are not authorized to approve this entry"
    );

    // Approver (staff) menyetujui: transaksi + update tracking + mail.
    let res = app.patch(&url).token(&fx.staff.token).send().await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["message"], "Manual time approved");
    assert_eq!(res.data()["manualTimeStatus"], "APPROVED");
    assert_eq!(
        res.data()["approvedBy"]["uuid"].as_str(),
        Some(fx.staff.uuid.as_str())
    );
    assert_eq!(status_of(&app, t.id).await, "APPROVED");
    let taken: i32 = app
        .scalar(&format!(
            r#"SELECT "totalTakenInSecond" FROM tracking WHERE id = {}"#,
            t.tracking_id
        ))
        .await;
    assert_eq!(taken, 7200);

    // Sudah tidak PENDING.
    let res = app.patch(&url).token(&fx.staff.token).send().await;
    res.assert_status(200);
    assert_eq!(res.json["message"], "Only PENDING entries can be approved");

    // Reject: validasi body, otorisasi, lalu sukses.
    let t2 = timeline(&app, fx.staff2.id, p.id, next(), 0, 60, Some("PENDING")).await;
    let rurl = format!("/manual-time-approvals/{}/reject", t2.uuid);
    app.patch(&rurl)
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    let res = app
        .patch(&rurl)
        .token(&fx.staff2.token)
        .json(&json!({ "rejectedReason": "bukan saya" }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.json["message"],
        "You are not authorized to reject this entry"
    );
    let res = app
        .patch(&rurl)
        .token(&fx.staff.token)
        .json(&json!({ "rejectedReason": "Bukti kurang" }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["message"], "Manual time rejected");
    assert_eq!(res.data()["manualTimeStatus"], "REJECTED");
    assert_eq!(res.data()["rejectedReason"], "Bukti kurang");
    assert_eq!(status_of(&app, t2.id).await, "REJECTED");
    let res = app
        .patch(&rurl)
        .token(&fx.staff.token)
        .json(&json!({ "rejectedReason": "lagi" }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Only PENDING entries can be rejected");

    // Id tidak ada dan uuid rusak.
    let res = app
        .patch(&format!("/manual-time-approvals/{MISSING}/approve"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Manual time entry not found");
    app.patch("/manual-time-approvals/not-a-uuid/approve")
        .token(&fx.super_admin.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn approve_covered_and_adjusted_ranges() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    set_approver(&app, p.id, fx.staff.id).await;

    // Seluruh rentang sudah tercatat timeline lain → 409 dan tetap PENDING (rollback).
    let s = next();
    let c = timeline(&app, fx.staff2.id, p.id, s, 0, 120, Some("PENDING")).await;
    timeline(&app, fx.staff2.id, p.id, s, 0, 120, None).await;
    let res = app
        .patch(&format!("/manual-time-approvals/{}/approve", c.uuid))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_status(409);
    assert!(
        res.text().contains("already fully recorded"),
        "{}",
        res.text()
    );
    assert_eq!(status_of(&app, c.id).await, "PENDING");

    // Sebagian tercatat: potongan sisa disetujui; baris tambahan dibuat.
    let s2 = next();
    let a = timeline(&app, fx.staff2.id, p.id, s2, 0, 120, Some("PENDING")).await;
    timeline(&app, fx.staff2.id, p.id, s2, 30, 30, None).await;
    let res = app
        .patch(&format!("/manual-time-approvals/{}/approve", a.uuid))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(
        res.json["message"],
        "Manual time approved; the part already recorded was removed"
    );
    let approved_rows: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "trackingTimeline" WHERE "projectId" = {} AND "manualTimeStatus" = 'APPROVED'"#,
            p.id
        ))
        .await;
    assert_eq!(approved_rows, 2);
    // 30 menit + 60 menit = 5400 detik.
    let taken: i32 = app
        .scalar(&format!(
            r#"SELECT "totalTakenInSecond" FROM tracking WHERE id = {}"#,
            a.tracking_id
        ))
        .await;
    assert_eq!(taken, 5400);
}

#[tokio::test]
async fn email_action_links() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    set_approver(&app, p.id, fx.staff.id).await;

    // Approve lewat tautan email, lalu tautan yang sama ditolak (sudah dipakai).
    let t1 = timeline(&app, fx.staff2.id, p.id, next(), 0, 120, Some("PENDING")).await;
    let tok1 = email_token(&app, &t1, fx.staff.id, "NOW() + INTERVAL '1 day'").await;
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={tok1}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(location(&r).contains("status=approved"), "{}", location(&r));
    assert!(location(&r).contains("tab=approvals"), "{}", location(&r));
    assert_eq!(status_of(&app, t1.id).await, "APPROVED");
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={tok1}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(location(&r).contains("status=error"), "{}", location(&r));
    assert!(
        location(&r).contains("already+been+used"),
        "{}",
        location(&r)
    );

    // Reject lewat tautan email.
    let t2 = timeline(&app, fx.staff2.id, p.id, next(), 0, 60, Some("PENDING")).await;
    let tok2 = email_token(&app, &t2, fx.staff.id, "NOW() + INTERVAL '1 day'").await;
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={tok2}&action=reject"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(location(&r).contains("status=rejected"), "{}", location(&r));
    assert_eq!(status_of(&app, t2.id).await, "REJECTED");

    // Tautan kedaluwarsa.
    let t3 = timeline(&app, fx.staff2.id, p.id, next(), 0, 60, Some("PENDING")).await;
    let tok3 = email_token(&app, &t3, fx.staff.id, "NOW() - INTERVAL '1 day'").await;
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={tok3}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(location(&r).contains("has+expired"), "{}", location(&r));
    assert_eq!(status_of(&app, t3.id).await, "PENDING");

    // Token tidak dikenal, aksi tidak valid, dan token bukan uuid (500 seperti Node).
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={MISSING}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(
        location(&r).contains("Invalid+or+expired+token"),
        "{}",
        location(&r)
    );
    let r = app
        .get(&format!(
            "/manual-time-approvals/email-action?token={tok3}&action=nope"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(location(&r).contains("Invalid+action"), "{}", location(&r));
    app.get("/manual-time-approvals/email-action?token=bukan-uuid&action=approve")
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn approval_listings_and_attachments() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    let p_other = project(&app, &fx).await; // tanpa approver
    set_approver(&app, p.id, fx.staff.id).await;

    let t = timeline(&app, fx.staff2.id, p.id, next(), 0, 120, Some("PENDING")).await;
    let t_other = timeline(
        &app,
        fx.staff2.id,
        p_other.id,
        next(),
        0,
        60,
        Some("PENDING"),
    )
    .await;

    // Dashboard: approver melihat pending proyeknya; filter tanggal & proyek.
    let res = app
        .get(&format!(
            "/manual-time-approvals?projectUuid={}&status=PENDING&startDate=2099-01-01&endDate=2101-12-31",
            p.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert!(
        res.json["total"].as_i64().unwrap_or(0) >= 1,
        "{}",
        res.text()
    );
    assert!(uuids(res.data()).contains(&t.uuid), "{}", res.text());

    // Proyek tanpa approver untuk staff: kosong.
    let res = app
        .get(&format!(
            "/manual-time-approvals?projectUuid={}",
            p_other.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["total"], 0);
    assert!(uuids(res.data()).is_empty());

    // Validasi query.
    app.get("/manual-time-approvals?status=NOPE")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get("/manual-time-approvals?projectUuid=abc")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    // endDate yang sudah memuat jam → Invalid Date (perilaku Node).
    let res = app
        .get("/manual-time-approvals?endDate=2101-01-01T10:00:00Z")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);

    // Proyek yang ditugaskan.
    let res = app
        .get("/manual-time-approvals/assigned-projects")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert!(
        res.data()
            .as_array()
            .into_iter()
            .flatten()
            .any(|x| x["uuid"].as_str() == Some(p.uuid.as_str())),
        "{}",
        res.text()
    );

    // Lampiran pada pending milik staff2 (proyek tanpa approver).
    let att: String = app
        .scalar(&format!(
            r#"INSERT INTO "manualTimeAttachment" ("trackingTimelineId", "fileName", "fileFullpath", "fileSize", "mimeType")
               VALUES ({}, 'bukti.pdf', 'manual-time/{}/bukti.pdf', 10, 'application/pdf') RETURNING uuid::text"#,
            t_other.id, t_other.uuid
        ))
        .await;
    let res = app
        .get("/manual-time-approvals/my-pending")
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let item = res
        .data()
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["uuid"].as_str() == Some(t_other.uuid.as_str()))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        item["manualTimeAttachments"][0]["fileUrl"],
        format!("/manual-time-approvals/attachments/{att}/file")
    );
    assert_eq!(item["manualTimeAttachments"][0]["fileName"], "bukti.pdf");

    // Lampiran: requester bukan pemilik, bukan approver, bukan peran perusahaan → 404.
    let res = app
        .get(&format!("/manual-time-approvals/attachments/{att}/file"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(404);
    app.get(&format!(
        "/manual-time-approvals/attachments/{att}/file/url"
    ))
    .token(&fx.staff.token)
    .send()
    .await
    .assert_status(404);
    app.get(&format!(
        "/manual-time-approvals/attachments/{MISSING}/file"
    ))
    .token(&fx.staff.token)
    .send()
    .await
    .assert_status(404);
    app.get("/manual-time-approvals/attachments/bukan-uuid/file")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────────── manual-time-settings ─────────────────────────────

#[tokio::test]
async fn settings_get_put_and_audit() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;

    let res = app
        .get("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let cur_mode = res.data()["mode"].as_str().unwrap_or_default().to_owned();
    let version = res.data()["exceptionsVersion"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        ["NONE", "ALL", "LIMIT"].contains(&cur_mode.as_str()),
        "{}",
        res.text()
    );
    assert!(res.data()["counts"].is_object(), "{}", res.text());

    // Peran STAFF tidak boleh membaca pengaturan.
    app.get("/v1/manual-time-settings")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // Validasi body.
    app.put("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .json(&json!({ "mode": "X" }))
        .send()
        .await
        .assert_status(400);

    // Versi daftar pengecualian usang → 409.
    let stale = json!({
        "mode": "ALL", "limitPeriod": "DAY", "autoApproveNewEmployees": true,
        "exceptionUserUuids": [], "exceptionsVersion": "0000000000000000"
    });
    let res = app
        .put("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .json(&stale)
        .send()
        .await;
    res.assert_status(409);
    assert!(
        res.text().contains("MANUAL_TIME_EXCEPTIONS_STALE"),
        "{}",
        res.text()
    );

    // Limit di luar rentang → 422.
    let res = app
        .put("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .json(&json!({
            "mode": "LIMIT", "limitPeriod": "DAY", "limitMinutes": 20,
            "autoApproveNewEmployees": true, "exceptionUserUuids": [],
            "exceptionsVersion": version
        }))
        .send()
        .await;
    res.assert_status(422);
    assert!(
        res.text().contains("MANUAL_TIME_LIMIT_OUT_OF_RANGE"),
        "{}",
        res.text()
    );

    // Simpan: mode berganti, staff masuk daftar pengecualian (selalu butuh persetujuan).
    let new_mode = if cur_mode == "LIMIT" { "ALL" } else { "LIMIT" };
    let res = app
        .put("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .json(&json!({
            "mode": new_mode, "limitPeriod": "DAY", "limitMinutes": 240,
            "autoApproveNewEmployees": true,
            "exceptionUserUuids": [fx.staff.uuid], "exceptionsVersion": version
        }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.data()["mode"], new_mode);
    assert!(
        res.data()["exceptions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|x| x["uuid"].as_str() == Some(fx.staff.uuid.as_str())
                && x["limitMinutes"].is_null()),
        "{}",
        res.text()
    );

    // Perubahan company wajib tercatat di auditLog (entity company, aksi update).
    let company = fx.id;
    let n = wait_count(
        &app,
        &format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {company}
               AND "entityType"::text = 'company' AND "entityId" = {company} AND action::text = 'update'"#
        ),
        1,
    )
    .await;
    assert!(n >= 1, "auditLog company update tidak tercatat");

    // Versi baru setelah simpan: versi lama sekarang usang.
    let new_version = app
        .get("/v1/manual-time-settings")
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(new_version.data()["mode"], new_mode);
}

#[tokio::test]
async fn settings_employee_lists() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;

    let res = app
        .get("/v1/manual-time-settings/employees?page=1&limit=5")
        .token(&fx.manager.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["meta"]["page"], 1);
    assert_eq!(res.json["meta"]["limit"], 5);
    assert!(res.json["meta"]["total"].is_number(), "{}", res.text());
    assert!(res.data().as_array().is_some_and(|a| a.len() <= 5));

    // Pencarian dengan karakter wildcard tidak error.
    let res = app
        .get("/v1/manual-time-settings/employees?search=%25_x")
        .token(&fx.manager.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();

    app.get("/v1/manual-time-settings/employees?limit=500")
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(400);
    app.get("/v1/manual-time-settings/employees?page=0")
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(400);
    app.get("/v1/manual-time-settings/employees")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    let res = app
        .get("/v1/manual-time-settings/employees/uuids")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert!(res.data()["uuids"].is_array(), "{}", res.text());
    let total = res.data()["total"].as_i64().unwrap_or(-1);
    assert!(total >= 0);
    let listed = res.data()["uuids"].as_array().map_or(0, Vec::len);
    assert!(i64::try_from(listed).unwrap_or(i64::MAX) <= total);
}

// ───────────────────────────── project-types ─────────────────────────────

#[tokio::test]
async fn project_types_list_pagination_and_order() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let admin = system_admin_token(&app, &fx).await;

    // Tiga tipe dengan huruf awal berbeda supaya urutan nama tidak bergantung collation.
    let names = [uniq("ZA"), uniq("ZB"), uniq("ZC")];
    for name in &names {
        app.post("/project-types")
            .token(&admin)
            .json(&json!({ "code": name, "name": name }))
            .send()
            .await
            .assert_ok();
    }

    // Tanpa parameter: semua baris aktif, default desc createdAt.
    let res = app
        .get("/project-types")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let total = res.data()["total"].as_i64().unwrap_or(0);
    assert!(total >= 3, "{}", res.text());
    assert_eq!(res.data()["pageNumber"], 1);
    assert_eq!(res.data()["itemsPerPage"], total);

    // Halaman kedua dengan 2 baris per halaman.
    let res = app
        .get("/project-types?pageNumber=2&itemsPerPage=2")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["pageNumber"], 2);
    assert_eq!(res.data()["itemsPerPage"], 2);
    assert!(
        res.data()["result"]
            .as_array()
            .is_some_and(|a| a.len() <= 2)
    );

    // Urutan nama asc dan desc: tiga tipe kita harus berurutan sesuai arah.
    for (dir, expect_asc) in [("asc", true), ("desc", false)] {
        let res = app
            .get(&format!(
                "/project-types?orderColumn=name&orderBy={dir}&pageNumber=1&itemsPerPage=100000"
            ))
            .token(&fx.staff.token)
            .send()
            .await;
        res.assert_ok().assert_no_sql_error();
        let listed: Vec<String> = res.data()["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["name"].as_str().map(str::to_owned))
            .collect();
        let mut expected = names.to_vec();
        expected.sort();
        if !expect_asc {
            expected.reverse();
        }
        let pos: Vec<usize> = expected
            .iter()
            .map(|n| listed.iter().position(|x| x == n).unwrap_or(usize::MAX))
            .collect();
        assert!(
            pos.iter().all(|p| *p != usize::MAX),
            "{dir}: {}",
            res.text()
        );
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{dir}: {pos:?}");
    }

    // Parameter tak valid: pageNumber bukan angka → 400; orderBy/orderColumn salah → 500 (Prisma di Node).
    app.get("/project-types?pageNumber=abc&itemsPerPage=2")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get("/project-types?pageNumber=1&itemsPerPage=2&orderBy=sideways")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(500);
    app.get("/project-types?pageNumber=1&itemsPerPage=2&orderColumn=bogus")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(500);
    // Hanya orderBy tanpa paging: itemsPerPage NaN → 500 (sama dengan Node).
    app.get("/project-types?orderBy=asc")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn project_types_crud() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let admin = system_admin_token(&app, &fx).await;

    // Buat.
    let code = uniq("CRUD");
    let res = app
        .post("/project-types")
        .token(&admin)
        .json(&json!({ "code": code, "name": "Nama awal" }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let uuid = res.data()["uuid"].as_str().unwrap_or_default().to_owned();
    assert_eq!(res.data()["code"], code.as_str());
    assert_eq!(res.data()["status"], 1);
    app.post("/project-types")
        .token(&admin)
        .json(&json!({ "code": code }))
        .send()
        .await
        .assert_status(400);
    app.post("/project-types")
        .token(&fx.staff.token)
        .json(&json!({ "code": code, "name": "x" }))
        .send()
        .await
        .assert_status(401);

    // Baca.
    let res = app
        .get(&format!("/project-types/{uuid}"))
        .token(&admin)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["name"], "Nama awal");
    app.get("/project-types/bukan-uuid")
        .token(&admin)
        .send()
        .await
        .assert_status(400);
    let res = app
        .get(&format!("/project-types/{MISSING}"))
        .token(&admin)
        .send()
        .await;
    assert_eq!(res.json["success"], false);

    // Ubah: hanya code dan name yang ditulis (status diabaikan), updatedAt terisi.
    let res = app
        .patch(&format!("/project-types/{uuid}"))
        .token(&admin)
        .json(&json!({ "name": "Nama baru", "status": 0 }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["name"], "Nama baru");
    assert_eq!(res.data()["code"], code.as_str());
    assert_eq!(res.data()["status"], 1);
    assert!(res.data()["updatedAt"].is_string(), "{}", res.text());
    app.patch(&format!("/project-types/{uuid}"))
        .token(&admin)
        .json(&json!({ "name": 3 }))
        .send()
        .await
        .assert_status(400);
    app.patch(&format!("/project-types/{MISSING}"))
        .token(&admin)
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(500);

    // Hapus = soft delete (status 0): tidak lagi terbaca.
    let res = app
        .delete(&format!("/project-types/{uuid}"))
        .token(&admin)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["isDeleted"], true);
    let res = app
        .get(&format!("/project-types/{uuid}"))
        .token(&admin)
        .send()
        .await;
    assert_eq!(res.json["success"], false);
    let status: i32 = app
        .scalar(&format!(
            r#"SELECT status FROM "projectType" WHERE uuid = '{uuid}'::uuid"#
        ))
        .await;
    assert_eq!(status, 0);
    let res = app
        .delete(&format!("/project-types/{MISSING}"))
        .token(&admin)
        .send()
        .await;
    assert!(!res.status.is_success(), "{}", res.text());
}

// ───────────────────────────── project-budget ─────────────────────────────

#[tokio::test]
async fn project_budget_upsert_get_report_and_delete() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    let url = format!("/project-budget/{}", p.uuid);

    // Upsert HOURS (manager).
    let hours = json!({
        "isEnabled": true, "budgetType": "HOURS", "budgetHours": 10,
        "resetCycle": "NEVER", "alertThreshold": 80, "enforcement": "ALERT_ONLY",
        "countNonBillable": true
    });
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&hours)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["budgetType"], "HOURS");
    assert_eq!(res.data()["budgetHours"].as_f64(), Some(10.0));

    // Baca.
    let res = app.get(&url).token(&fx.staff.token).send().await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["budgetType"], "HOURS");

    // Laporan: proyek muncul dengan status ON_TRACK, tidak di filter OVER_BUDGET.
    let res = app
        .get("/budget-report")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let item = res
        .data()
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["projectUuid"].as_str() == Some(p.uuid.as_str()))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(item["budgetType"], "HOURS");
    assert_eq!(item["status"], "ON_TRACK");
    assert_eq!(item["budgetHours"].as_f64(), Some(10.0));
    let res = app
        .get("/budget-report?status=OVER_BUDGET")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert!(!uuids(res.data()).contains(&p.uuid), "{}", res.text());

    // Ganti ke COST; biaya wajib lengkap.
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({
            "isEnabled": true, "budgetType": "COST", "billableRate": 50,
            "budgetAmount": 1000, "resetCycle": "MONTHLY", "countNonBillable": false
        }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["budgetType"], "COST");
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "isEnabled": true, "budgetType": "COST", "budgetAmount": 100 }))
        .send()
        .await;
    res.assert_status(200);
    assert!(
        res.json["message"]
            .as_str()
            .is_some_and(|m| m.contains("Billable rate")),
        "{}",
        res.text()
    );

    // Ekspor CSV.
    let res = app
        .get("/budget-report?export=csv")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let ct = res
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(ct.starts_with("text/csv"), "{ct}");
    let text = res.text();
    assert!(text.starts_with("Project,Type,Budget,Burned"), "{text}");
    assert!(text.contains(&format!("\"{}\",COST,$", p.name)), "{text}");
    assert!(!text.contains("undefined"), "{text}");

    // Validasi tipe budget → 400; proyek tidak ada → success:false.
    app.post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "isEnabled": true, "budgetType": "FOO" }))
        .send()
        .await
        .assert_status(400);
    let res = app
        .post(&format!("/project-budget/{MISSING}"))
        .token(&fx.manager.token)
        .json(&hours)
        .send()
        .await;
    assert_eq!(res.json["message"], "Project not found");

    // Nonaktifkan: laporan tidak lagi memuat proyek, GET tanpa budget aktif.
    let res = app.delete(&url).token(&fx.super_admin.token).send().await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["isDisabled"], true);
    let res = app.get(&url).token(&fx.staff.token).send().await;
    res.assert_ok();
    assert!(res.data().is_null(), "{}", res.text());
    let res = app
        .get("/budget-report")
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert!(!uuids(res.data()).contains(&p.uuid));

    // Hapus budget: hanya SUPER_ADMIN/ADMIN/SYSTEM_ADMIN.
    app.delete(&url)
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
}

// ───────────────────────────── project-manual-time-approvers ─────────────────────────────

#[tokio::test]
async fn project_approvers_add_and_remove() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    let member = fx.staff.id;
    app.exec(&format!(
        r#"INSERT INTO "projectUser" ("projectId", "userId", status) VALUES ({}, {member}, 1)"#,
        p.id
    ))
    .await;
    let url = format!("/projects/{}/manual-time-approvers", p.uuid);

    app.post(&url)
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);

    // Tambah approver anggota proyek.
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": fx.staff.uuid }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["message"], "Approver added");
    let approver_uuid = res.data()["uuid"].as_str().unwrap_or_default().to_owned();
    assert!(!approver_uuid.is_empty(), "{}", res.text());

    // Duplikat, bukan anggota, user/proyek tidak ada.
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": fx.staff.uuid }))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "User is already an approver for this project"
    );
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": fx.staff2.uuid }))
        .send()
        .await;
    assert_eq!(res.json["message"], "User is not a member of this project");
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": MISSING }))
        .send()
        .await;
    assert_eq!(res.json["message"], "User not found");
    let res = app
        .post(&format!("/projects/{MISSING}/manual-time-approvers"))
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": fx.staff.uuid }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Project not found");

    // Hapus (status 0), hapus lagi → tidak ditemukan, tambah lagi → aktif kembali (uuid sama).
    let del = format!("{url}/{approver_uuid}");
    let res = app.delete(&del).token(&fx.manager.token).send().await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["message"], "Approver removed");
    let status: i32 = app
        .scalar(&format!(
            r#"SELECT status FROM "projectManualTimeApprover" WHERE uuid = '{approver_uuid}'::uuid"#
        ))
        .await;
    assert_eq!(status, 0);
    let res = app.delete(&del).token(&fx.manager.token).send().await;
    assert_eq!(res.json["message"], "Approver not found");
    let res = app
        .post(&url)
        .token(&fx.manager.token)
        .json(&json!({ "userUuid": fx.staff.uuid }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["uuid"].as_str(), Some(approver_uuid.as_str()));

    // Peran STAFF tidak boleh menambah approver.
    app.post(&url)
        .token(&fx.staff.token)
        .json(&json!({ "userUuid": fx.staff.uuid }))
        .send()
        .await
        .assert_status(401);
}

// ───────────────────────────── app-releases ─────────────────────────────

#[tokio::test]
async fn app_releases_public_and_admin_flow() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };

    // Dua rilis WINDOWS/ALPHA (tanggal 2100 supaya terbaru di antara data template).
    let base = uniq("it");
    let v_old = format!("{base}a");
    let v_new = format!("{base}b");
    for (v, date) in [
        (&v_old, "TIMESTAMP '2100-01-01 00:00:00'"),
        (&v_new, "TIMESTAMP '2100-02-01 00:00:00'"),
    ] {
        app.exec(&format!(
            r#"INSERT INTO "appRelease" (version, platform, channel, "fileName", "fileSize", sha512,
                   "downloadUrl", "gcsPath", "releaseNotes", "releaseDate", "isActive")
               VALUES ('{v}', 'WINDOWS', 'ALPHA', 'Setup-{v}.exe', 1234, 'c2hh',
                   'https://example.test/x', 'app-releases/win/alpha/Setup-{v}.exe', 'catatan {v}', {date}, true)"#
        ))
        .await;
    }
    let new_uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "appRelease" WHERE version = '{v_new}'"#
        ))
        .await;

    // ── publik ──
    let res = app.get("/app-releases/changelog").send().await;
    res.assert_ok().assert_no_sql_error();
    assert!(res.data().is_array(), "{}", res.text());

    let res = app
        .get("/app-releases/release-notes/win?channel=alpha")
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["version"], v_new.as_str());
    assert_eq!(res.data()["platform"], "WINDOWS");
    assert_eq!(res.data()["channel"], "ALPHA");
    let res = app.get("/app-releases/release-notes/bsd").send().await;
    assert_eq!(res.json["message"], "Invalid platform");
    app.get("/app-releases/release-notes/win?channel=weird")
        .send()
        .await
        .assert_status(500);

    let res = app.get("/app-releases/win/alpha.yml").send().await;
    res.assert_status(200).assert_no_sql_error();
    assert!(
        res.text().contains(&format!("version: {v_new}")),
        "{}",
        res.text()
    );
    assert!(res.text().contains("sha512: c2hh"), "{}", res.text());
    app.get("/app-releases/bsd/alpha.yml")
        .send()
        .await
        .assert_status(404);
    app.get("/app-releases/win/foo.yml")
        .send()
        .await
        .assert_status(404);

    // Unduhan: rilis tidak ada (jalur GCS untuk baris yang ada tidak diuji di sini).
    app.get(&format!("/app-releases/download/win/{base}zz/Setup.exe"))
        .send()
        .await
        .assert_status(404);
    app.get("/app-releases/download/bsd/1.0.0/Setup.exe")
        .send()
        .await
        .assert_status(404);
    app.get(&format!(
        "/app-releases/download/win/{}/Setup.exe",
        "9".repeat(51)
    ))
    .send()
    .await
    .assert_status(404);

    // ── admin (operator) ──
    let (op_id, op_token) = operator(&app, 1).await;

    app.get("/admin/app-releases")
        .send()
        .await
        .assert_status(401);
    let res = app
        .get(&format!(
            "/admin/app-releases?platform=WINDOWS&channel=ALPHA&search={v_new}"
        ))
        .token(&op_token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["total"], 1, "{}", res.text());
    assert_eq!(
        res.data()["result"][0]["uuid"].as_str(),
        Some(new_uuid.as_str())
    );
    assert_eq!(res.data()["result"][0]["fileSize"], "1234");
    assert!(
        res.data()["result"][0]["downloadUrl"]
            .as_str()
            .is_some_and(|u| u.contains("/app-releases/download/win/")),
        "{}",
        res.text()
    );
    app.get("/admin/app-releases?platform=BSD")
        .token(&op_token)
        .send()
        .await
        .assert_status(400);
    let res = app
        .get("/admin/app-releases?pageNumber=1&itemsPerPage=2&orderColumn=version&orderBy=asc")
        .token(&op_token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["itemsPerPage"], 2);

    let res = app
        .get(&format!("/admin/app-releases/{new_uuid}"))
        .token(&op_token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.data()["version"], v_new.as_str());
    let res = app
        .get(&format!("/admin/app-releases/{MISSING}"))
        .token(&op_token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Release not found");

    // Nonaktifkan rilis terbaru → publik kembali ke rilis lama.
    let toggle = format!("/admin/app-releases/{new_uuid}/toggle");
    let res = app
        .patch(&toggle)
        .token(&op_token)
        .json(&json!({ "isActive": false }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["message"], "Release deactivated successfully");
    assert_eq!(res.data()["isActive"], false);
    let res = app
        .get("/app-releases/release-notes/win?channel=alpha")
        .send()
        .await;
    assert_eq!(res.data()["version"], v_old.as_str());

    // Tipe salah → galat; tanpa body → hanya ubah status tanpa flag; aktifkan lagi.
    let res = app
        .patch(&toggle)
        .token(&op_token)
        .json(&json!({ "isActive": "yes" }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Failed to toggle release status");
    let res = app.patch(&toggle).token(&op_token).send().await;
    res.assert_ok();
    assert_eq!(res.json["message"], "Release deactivated successfully");
    let res = app
        .patch(&toggle)
        .token(&op_token)
        .json(&json!({ "isActive": true }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["message"], "Release activated successfully");
    let res = app
        .get("/app-releases/release-notes/win?channel=alpha")
        .send()
        .await;
    assert_eq!(res.data()["version"], v_new.as_str());
    let res = app
        .patch(&format!("/admin/app-releases/{MISSING}/toggle"))
        .token(&op_token)
        .json(&json!({ "isActive": true }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Release not found");

    // Audit operator: tiga toggle sukses tercatat.
    let audit_n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "operatorAuditLog" WHERE "operatorId" = {op_id}
               AND action = 'app_release.toggle' AND "targetUuid" = '{new_uuid}'::uuid"#
        ))
        .await;
    assert!(audit_n >= 3, "operatorAuditLog toggle = {audit_n}");

    // Hapus rilis tidak ada (jalur hapus objek GCS untuk baris ada tidak diuji).
    let res = app
        .delete(&format!("/admin/app-releases/{MISSING}"))
        .token(&op_token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Release not found");

    // Token operator: nonaktif, sub bukan integer, kedaluwarsa → 401.
    let (_, inactive_token) = operator(&app, 0).await;
    app.get("/admin/app-releases")
        .token(&inactive_token)
        .send()
        .await
        .assert_status(401);
    app.get("/admin/app-releases")
        .token(&app.token_with(json!({ "sub": "abc" })))
        .send()
        .await
        .assert_status(401);
    app.get("/admin/app-releases")
        .token(&app.token_with(json!({ "sub": op_id, "exp": 1_000_000 })))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn app_release_upload_validation_and_failure_path() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let (_, op_token) = operator(&app, 1).await;

    // Tanpa token operator.
    let (ct, body) = multipart(&[("platform", "WINDOWS"), ("version", "1.0.0")]);
    app.post("/admin/app-releases/upload")
        .raw(&ct, body)
        .send()
        .await
        .assert_status(401);

    // Validasi DTO → 400 (sebelum GCS).
    let (ct, body) = multipart(&[("platform", "BSD"), ("channel", "NOPE")]);
    app.post("/admin/app-releases/upload")
        .token(&op_token)
        .raw(&ct, body)
        .send()
        .await
        .assert_status(400);

    // Tanpa berkas: gagal sebelum menyentuh GCS → 201 success:false (seperti Node).
    let (ct, body) = multipart(&[("platform", "WINDOWS"), ("version", "1.0.0")]);
    let res: TestResponse = app
        .post("/admin/app-releases/upload")
        .token(&op_token)
        .raw(&ct, body)
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.json["success"], false);
    assert!(
        res.json["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Failed to upload release")),
        "{}",
        res.text()
    );
}

// ───────────────────────────── urutan kunci = Prisma ─────────────────────────────

fn key_list(v: &Value) -> Vec<&str> {
    v.as_object()
        .unwrap_or_else(|| panic!("bukan objek: {v}"))
        .keys()
        .map(String::as_str)
        .collect()
}

/// Objek yang dibangun di SQL (`json_build_object`) berurutan `select` Node
/// (`manual-time-approvals.service.ts`), bukan urutan jsonb (panjang lalu abjad).
#[tokio::test]
async fn manual_time_json_keys_follow_node_order() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let p = project(&app, &fx).await;
    set_approver(&app, p.id, fx.staff.id).await;
    let t = timeline(&app, fx.staff2.id, p.id, next(), 0, 120, Some("PENDING")).await;
    app.exec(&format!(
        r#"INSERT INTO "manualTimeAttachment" ("trackingTimelineId", "fileName", "fileFullpath", "fileSize", "mimeType")
           VALUES ({}, 'bukti.pdf', 'manual-time/{}/bukti.pdf', 10, 'application/pdf')"#,
        t.id, t.uuid
    ))
    .await;

    // my-pending (findMyPending).
    let res = app
        .get("/manual-time-approvals/my-pending")
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let row = res
        .data()
        .as_array()
        .and_then(|a| a.iter().find(|r| r["uuid"] == t.uuid.as_str()))
        .unwrap_or_else(|| panic!("pending tidak ada: {}", res.text()));
    assert_eq!(
        key_list(row),
        [
            "uuid",
            "note",
            "reason",
            "startAt",
            "endAt",
            "timezone",
            "offsetMinutes",
            "manualTimeStatus",
            "requestedAt",
            "project",
            "manualTimeAttachments",
        ]
    );
    assert_eq!(key_list(&row["project"]), ["uuid", "name"]);
    assert_eq!(
        key_list(&row["manualTimeAttachments"][0]),
        ["uuid", "fileName", "fileUrl"]
    );
    // Format tanggal tetap `Date.toJSON()` (ms + Z).
    let start = row["startAt"].as_str().unwrap();
    assert!(
        start.len() == 24 && start.ends_with('Z'),
        "startAt: {start}"
    );

    // findAll (dashboard approver).
    let res = app
        .get(&format!("/manual-time-approvals?projectUuid={}", p.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(key_list(&res.json), ["success", "data", "total"]);
    let row = res
        .data()
        .as_array()
        .and_then(|a| a.iter().find(|r| r["uuid"] == t.uuid.as_str()))
        .unwrap_or_else(|| panic!("pending tidak ada: {}", res.text()));
    assert_eq!(
        key_list(row),
        [
            "uuid",
            "note",
            "reason",
            "startAt",
            "endAt",
            "timezone",
            "offsetMinutes",
            "manualTimeStatus",
            "manualTimeApprovalSource",
            "requestedAt",
            "rejectedReason",
            "approvedAt",
            "approvedBy",
            "tracking",
            "project",
            "manualTimeAttachments",
        ]
    );
    assert_eq!(key_list(&row["tracking"]), ["user"]);
    assert_eq!(
        key_list(&row["tracking"]["user"]),
        ["uuid", "name", "username"]
    );

    // assigned-projects.
    let res = app
        .get("/manual-time-approvals/assigned-projects")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let item = res
        .data()
        .as_array()
        .and_then(|a| a.iter().find(|x| x["uuid"] == p.uuid.as_str()))
        .unwrap_or_else(|| panic!("proyek tidak ada: {}", res.text()));
    assert_eq!(key_list(item), ["uuid", "name", "code", "status"]);

    // approve: uuid, manualTimeStatus, manualTimeApprovalSource, approvedAt, approvedBy{uuid,name}.
    let res = app
        .patch(&format!("/manual-time-approvals/{}/approve", t.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(
        key_list(res.data()),
        [
            "uuid",
            "manualTimeStatus",
            "manualTimeApprovalSource",
            "approvedAt",
            "approvedBy",
        ]
    );
    assert_eq!(key_list(&res.data()["approvedBy"]), ["uuid", "name"]);

    // reject: rejectedReason sebelum approvedAt (urutan `select` Node).
    let t2 = timeline(&app, fx.staff2.id, p.id, next(), 0, 60, Some("PENDING")).await;
    let res = app
        .patch(&format!("/manual-time-approvals/{}/reject", t2.uuid))
        .token(&fx.staff.token)
        .json(&json!({ "rejectedReason": "Bukti kurang" }))
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(
        key_list(res.data()),
        [
            "uuid",
            "manualTimeStatus",
            "manualTimeApprovalSource",
            "rejectedReason",
            "approvedAt",
            "approvedBy",
        ]
    );
}
