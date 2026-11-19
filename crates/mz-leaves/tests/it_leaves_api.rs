//! Uji integrasi route `/mobile/leaves` (pengajuan multipart + lampiran GCS emulator, kuota,
//! saldo, approver, pratinjau hari, revisi, batal, dokumen) dan route baca dasbor `/leaves`
//! (daftar, kalender, laporan CSV/XLSX, ringkasan, overview).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Read;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn submit_with_document_quota_and_validation() {
    let Some((app, em)) = app("api_a").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    policy_approver(&app, fx.id, fx.manager.id).await;
    let y = this_year();
    let doc_type = leave_type(&app, fx.id, true, "ANNUAL", true, None).await;
    let quota_type = leave_type(&app, fx.id, false, "ANNUAL", false, Some(3)).await;
    balance(&app, fx.id, fx.staff.id, y, 12.0, 0.0, 0.0, 0.0, 0.0).await;
    let mon = first_monday(y, 7);

    // Validasi DTO (multipart: semua string).
    let (ct, body) = multipart(&[("leaveTypeId", "abc"), ("reason", "")], None);
    let res = app
        .post("/mobile/leaves")
        .token(&fx.staff.token)
        .raw(&ct, body)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "leaveTypeId must not be less than 1",
            "leaveTypeId must be an integer number",
            "approverId must not be less than 1",
            "approverId must be an integer number",
            "startDate must be a valid ISO 8601 date string",
            "endDate must be a valid ISO 8601 date string",
            "reason should not be empty"
        ])
    );

    let doc_type_s = doc_type.to_string();
    let manager_s = fx.manager.id.to_string();
    let (start, end) = (key(mon), key(plus(mon, 1)));
    let fields = |t: &str, a: &str| {
        vec![
            ("leaveTypeId", t.to_owned()),
            ("approverId", a.to_owned()),
            ("startDate", start.clone()),
            ("endDate", end.clone()),
            ("reason", "keluarga".to_owned()),
        ]
    };
    let as_refs = |f: &Vec<(&'static str, String)>| -> Vec<(&'static str, String)> { f.clone() };
    let send = |f: Vec<(&'static str, String)>,
                file: Option<(&'static str, &'static str, &'static [u8])>| {
        let pairs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (ct, body) = multipart(&pairs, file.map(|(n, m, b)| ("document", n, m, b)));
        app.post("/mobile/leaves")
            .token(&fx.staff.token)
            .raw(&ct, body)
            .send()
    };

    // Dokumen wajib.
    let res = send(as_refs(&fields(&doc_type_s, &manager_s)), None).await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Cuti ANNUAL wajib melampirkan dokumen");
    // Approver bukan approver kebijakan.
    let res = send(
        fields(&doc_type_s, &fx.admin.id.to_string()),
        Some(("surat.PDF", "application/pdf", b"%PDF-1")),
    )
    .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Approver tidak valid untuk perusahaan ini"
    );
    // Jenis tidak aktif / bukan milik company.
    let res = send(fields("999999", &manager_s), None).await;
    res.assert_status(404);
    assert_eq!(
        res.json["message"],
        "Jenis cuti tidak ditemukan atau tidak aktif"
    );
    // Berkas di field lain → 400 Unexpected field.
    let pairs: Vec<(&str, &str)> = vec![("leaveTypeId", "1")];
    let (ct, body) = multipart(&pairs, Some(("foto", "a.png", "image/png", b"x")));
    let res = app
        .post("/mobile/leaves")
        .token(&fx.staff.token)
        .raw(&ct, body)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Unexpected field");

    // Sukses: 201, lampiran di emulator, relasi user/approver/leaveTypeConfig.
    let res = send(
        fields(&doc_type_s, &manager_s),
        Some(("surat.dokter.PDF", "application/pdf", b"%PDF-1.4 uji")),
    )
    .await;
    res.assert_status(201);
    let j = res.json.clone();
    assert_eq!(j["status"], "PENDING");
    assert_eq!(j["totalDays"], 2);
    assert_eq!(j["startDate"], format!("{start}T00:00:00.000Z"));
    assert_eq!(j["approver"]["id"], fx.manager.id);
    assert_eq!(j["user"]["id"], fx.staff.id);
    assert_eq!(j["leaveTypeConfig"]["requiresDocument"], true);
    assert!(j.get("reviewedBy").is_none());
    let doc_key = j["documentKey"].as_str().unwrap().to_owned();
    assert!(doc_key.starts_with(&format!("leave-documents/{}/{}/", fx.id, fx.staff.id)));
    assert!(doc_key.ends_with(".PDF"));
    let stored = em.get(&doc_key).unwrap();
    assert_eq!(
        (stored.0.as_str(), stored.1.as_slice()),
        ("application/pdf", &b"%PDF-1.4 uji"[..])
    );
    let uuid = j["uuid"].as_str().unwrap().to_owned();

    // Signed URL dokumen: pemilik & approver.
    for (path, token) in [
        (format!("/mobile/leaves/{uuid}/document"), &fx.staff.token),
        (format!("/leaves/{uuid}/document"), &fx.manager.token),
    ] {
        let res = app.get(&path).token(token).send().await;
        res.assert_status(200);
        let url = res.json["url"].as_str().unwrap();
        assert!(url.contains(BUCKET) && url.contains("X-Goog-Signature"));
        assert!(
            url.contains("response-content-type=application%2Fpdf"),
            "{url}"
        );
        assert!(res.json["expiresAt"].is_string());
        assert_eq!(
            res.headers.get("cache-control").unwrap(),
            "private, max-age=300"
        );
    }
    // Dokumen orang lain → 404.
    app.get(&format!("/mobile/leaves/{uuid}/document"))
        .token(&fx.staff2.token)
        .send()
        .await
        .assert_status(404);

    // Kuota per jenis: 3 hari; 2 PENDING sudah ada → sisa 1.
    request(
        &app,
        fx.id,
        fx.staff.id,
        Some(quota_type),
        None,
        plus(mon, 7),
        plus(mon, 8),
        2.0,
        "PENDING",
        Some(fx.manager.id),
    )
    .await;
    let res = send(fields(&quota_type.to_string(), &manager_s), None).await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Kuota Cuti ANNUAL tidak cukup. Tersisa: 1 hari."
    );

    // Saldo tidak cukup (JSON body juga diterima tanpa multer).
    let big = app
        .post("/mobile/leaves")
        .token(&fx.staff.token)
        .json(&json!({
            "leaveTypeId": leave_type(&app, fx.id, true, "ANNUAL", false, None).await,
            "approverId": fx.manager.id,
            "startDate": key(mon),
            "endDate": key(plus(mon, 25)),
            "reason": "panjang",
        }))
        .send()
        .await;
    big.assert_status(400);
    assert_eq!(
        big.json["message"],
        "Saldo cuti tahunan tidak cukup. Tersisa: 12 hari."
    );
}

#[tokio::test]
async fn revise_cancel_list_get() {
    let Some((app, _em)) = app("api_b").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    policy_approver(&app, fx.id, fx.manager.id).await;
    let y = this_year();
    let t = leave_type(&app, fx.id, false, "ANNUAL", false, None).await;
    let mon = first_monday(y, 8);
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

    // Revisi hanya untuk NEEDS_REVISION.
    let res = app
        .put(&format!("/mobile/leaves/{}/revise", r.uuid))
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Hanya pengajuan berstatus NEEDS_REVISION yang bisa direvisi"
    );

    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET status = 'NEEDS_REVISION', "revisionNote" = 'x' WHERE id = {}"#,
        r.id
    ))
    .await;
    // approverId null → approver tidak valid.
    let res = app
        .put(&format!("/mobile/leaves/{}/revise", r.uuid))
        .token(&fx.staff.token)
        .json(&json!({ "approverId": null }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Approver tidak valid untuk perusahaan ini"
    );

    let res = app
        .put(&format!("/mobile/leaves/{}/revise", r.uuid))
        .token(&fx.staff.token)
        .json(&json!({ "endDate": key(plus(mon, 2)), "endDayType": "HALF_DAY_MORNING", "reason": "baru" }))
        .send()
        .await;
    res.assert_status(200);
    let j = &res.json;
    assert_eq!(j["status"], "PENDING");
    assert_eq!(j["totalDays"], 2.5);
    assert_eq!(j["revisionNote"], Value::Null);
    assert_eq!(j["reason"], "baru");
    assert_eq!(j["approverId"], fx.manager.id);
    assert_eq!(j["reviewedBy"], Value::Null);
    assert!(j.get("user").is_none());

    // Detail & daftar milik sendiri.
    let res = app
        .get(&format!("/mobile/leaves/{}", r.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["uuid"], r.uuid);
    app.get(&format!("/mobile/leaves/{}", r.uuid))
        .token(&fx.staff2.token)
        .send()
        .await
        .assert_status(404);
    let res = app
        .get(&format!("/mobile/leaves?status=PENDING&year={y}&limit=1"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.json["meta"],
        json!({ "total": 1, "page": 1, "limit": 1, "totalPages": 1 })
    );
    assert_eq!(res.json["data"][0]["uuid"], r.uuid);
    let bad = app
        .get("/mobile/leaves?page=0&status=X")
        .token(&fx.staff.token)
        .send()
        .await;
    bad.assert_status(400);
    assert_eq!(
        bad.json["message"],
        json!([
            "status must be one of the following values: PENDING, APPROVED, REJECTED, NEEDS_REVISION",
            "page must not be less than 1"
        ])
    );

    // Batal → REJECTED tanpa relasi; batal lagi → 400.
    let res = app
        .delete(&format!("/mobile/leaves/{}", r.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["status"], "REJECTED");
    assert_eq!(res.json["rejectionReason"], "Cancelled by employee");
    assert!(res.json.get("leaveTypeConfig").is_none());
    let res = app
        .delete(&format!("/mobile/leaves/{}", r.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Only pending or needs-revision requests can be cancelled"
    );
}

#[tokio::test]
async fn balance_approvers_day_count() {
    let Some((app, _em)) = app("api_c").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    policy_approver(&app, fx.id, fx.manager.id).await;
    let y = this_year();
    let q = leave_type(&app, fx.id, false, "ANNUAL", false, Some(4)).await;
    balance(&app, fx.id, fx.staff.id, y, 12.0, 2.5, 3.0, 0.0, 1.0).await;
    let mon = first_monday(y, 9);
    request(
        &app,
        fx.id,
        fx.staff.id,
        Some(q),
        None,
        mon,
        mon,
        1.0,
        "APPROVED",
        Some(fx.manager.id),
    )
    .await;

    let res = app
        .get("/mobile/leaves/balance")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    let j = &res.json;
    assert_eq!(j["year"], y);
    assert_eq!(
        j["annualLeave"],
        json!({ "total": 12, "used": 2.5, "remaining": 9.5, "accrualMode": "UPFRONT", "entitlement": 12, "requestable": 12.5 })
    );
    assert_eq!(
        j["sickLeave"],
        json!({ "mode": "DEDUCT_ANNUAL", "total": 0, "used": 1, "remaining": -1 })
    );
    assert_eq!(
        j["carryOver"],
        json!({ "total": 3, "used": 0, "remaining": 3 })
    );
    assert_eq!(
        j["leaveTypeQuotas"],
        json!([{ "leaveTypeId": q, "name": "Cuti ANNUAL", "quota": 4, "used": 1, "remaining": 3 }])
    );
    assert_eq!(j["scheduledHolidayDeductions"], json!([]));

    let res = app
        .get("/mobile/leaves/approvers")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json[0]["id"], fx.manager.id);
    assert_eq!(res.json.as_array().unwrap().len(), 1);

    // Senin–Senin: Sabtu/Minggu + libur Rabu dikecualikan.
    let wed = plus(mon, 2);
    app.exec(&format!(
        r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
           VALUES ({}, 'Hari Raya', '{}', 'NATIONAL_HOLIDAY', false)"#,
        fx.id,
        key(wed)
    ))
    .await;
    let res = app
        .get(&format!(
            "/mobile/leaves/day-count?startDate={}&endDate={}&endDayType=HALF_DAY_AFTERNOON",
            key(mon),
            key(plus(mon, 7))
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["totalDays"], 4.5);
    assert_eq!(
        res.json["excludedDates"],
        json!([
            { "date": key(wed), "reason": "HOLIDAY", "name": "Hari Raya", "deductsLeave": false },
            { "date": key(plus(mon, 5)), "reason": "WEEKEND" },
            { "date": key(plus(mon, 6)), "reason": "WEEKEND" },
        ])
    );
    assert_eq!(res.json["requestable"], 12.5);
    // Validasi + rentang terbalik + tanpa hari kerja.
    let res = app
        .get("/mobile/leaves/day-count?startDate=2026-02-30")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "startDate must be a valid ISO 8601 date string",
            "endDate must be a valid ISO 8601 date string"
        ])
    );
    let res = app
        .get(&format!(
            "/mobile/leaves/day-count?startDate={}&endDate={}",
            key(plus(mon, 1)),
            key(mon)
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Tanggal selesai tidak boleh sebelum tanggal mulai"
    );
    let sat = plus(mon, 5);
    let res = app
        .get(&format!(
            "/mobile/leaves/day-count?startDate={}&endDate={}",
            key(sat),
            key(plus(sat, 1))
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "LEAVE_NO_WORKING_DAYS");
    // Tanpa token → 401.
    app.get("/mobile/leaves/balance")
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn dashboard_reads_and_reports() {
    let Some((app, _em)) = app("api_d").await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = app.company().await;
    reset(&app, &fx).await;
    // Tahun lalu: analitik memakai setahun penuh, tidak bergantung tanggal hari ini.
    let y = this_year() - 1;
    let t = leave_type(&app, fx.id, true, "ANNUAL", false, None).await;
    balance(&app, fx.id, fx.staff.id, y, 12.0, 2.0, 0.0, 0.0, 0.0).await;
    let mon = first_monday(y, 10);
    let a = request(
        &app,
        fx.id,
        fx.staff.id,
        Some(t),
        None,
        mon,
        plus(mon, 1),
        2.0,
        "APPROVED",
        Some(fx.manager.id),
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "reviewedById" = {m}, "reviewedAt" = "createdAt" + interval '3 hours',
             "balanceBefore" = 12, "balanceAfter" = 10, "approvalNote" = 'ok, siap'
           WHERE id = {id}"#,
        m = fx.manager.id,
        id = a.id
    ))
    .await;
    let p = request(
        &app,
        fx.id,
        fx.staff2.id,
        None,
        Some("SICK_LEAVE"),
        plus(mon, 1),
        plus(mon, 2),
        2.0,
        "PENDING",
        Some(fx.manager.id),
    )
    .await;
    // Pengajuan approver lain tidak terlihat oleh manager.
    request(
        &app,
        fx.id,
        fx.staff2.id,
        Some(t),
        None,
        mon,
        mon,
        1.0,
        "PENDING",
        Some(fx.admin.id),
    )
    .await;

    let tok = &fx.manager.token;
    let res = app
        .get(&format!("/leaves?year={y}&month=10&limit=1"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.json["meta"],
        json!({ "total": 2, "page": 1, "limit": 1, "totalPages": 2 })
    );
    assert_eq!(res.json["data"].as_array().unwrap().len(), 1);
    let s = &res.json["summary"];
    assert_eq!(s["totalPending"], 1);
    assert_eq!(s["overlappingRequests"], 2);
    assert_eq!(s["avgProcessingHours"], 3);
    let keys: Vec<&String> = res.json["data"][0].as_object().unwrap().keys().collect();
    let tail: Vec<&str> = keys[keys.len() - 4..].iter().map(|k| k.as_str()).collect();
    assert_eq!(tail, ["user", "reviewedBy", "approver", "leaveTypeConfig"]);
    let res = app
        .get("/leaves?status=APPROVED&userId=0")
        .token(tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["meta"]["total"], 1);
    app.get("/leaves")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // Kalender bulan.
    let res = app
        .get(&format!("/leaves/calendar?year={y}&month=10"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["totalRequests"], 2);
    let cal = res.json["calendar"].as_array().unwrap();
    assert_eq!(cal[0]["date"], key(mon));
    assert_eq!(cal[0]["leaves"].as_array().unwrap().len(), 1);
    assert_eq!(cal[1]["leaves"].as_array().unwrap().len(), 2);
    assert_eq!(cal.len(), 3);
    let bad = app.get("/leaves/calendar?year=x").token(tok).send().await;
    bad.assert_status(400);

    // Laporan CSV.
    let res = app.get("/leaves/report?format=csv").token(tok).send().await;
    res.assert_status(200);
    assert_eq!(
        res.headers.get("content-type").unwrap(),
        "text/csv; charset=utf-8"
    );
    let cd = res
        .headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        cd.starts_with("attachment; filename=\"Laporan-Cuti-") && cd.ends_with(".csv\""),
        "{cd}"
    );
    let text = res.text();
    assert!(text.starts_with('\u{feff}'));
    let lines: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines[1].contains("\"ok, siap\"") && lines[1].contains(",12,10,"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains("SICK_LEAVE") && lines[2].ends_with(",-,-,-,-,-"),
        "{}",
        lines[2]
    );
    // XLSX: dua sheet + dua grafik.
    let res = app
        .get(&format!(
            "/leaves/report?startDate={}&endDate={}",
            key(mon),
            key(mon)
        ))
        .token(tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.headers.get("content-type").unwrap(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(res.bytes.clone())).unwrap();
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    assert!(
        names.iter().any(|n| n == "xl/charts/chart1.xml"),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n == "xl/charts/chart2.xml"),
        "{names:?}"
    );
    let mut wb = String::new();
    zip.by_name("xl/workbook.xml")
        .unwrap()
        .read_to_string(&mut wb)
        .unwrap();
    assert!(wb.contains("Laporan Cuti") && wb.contains("Grafik"));
    let bad = app
        .get("/leaves/report?format=pdf&startDate=x")
        .token(tok)
        .send()
        .await;
    bad.assert_status(400);
    assert_eq!(
        bad.json["message"],
        json!([
            "startDate must be a valid ISO 8601 date string",
            "format must be one of the following values: csv, excel"
        ])
    );

    // Ringkasan analitik.
    let res = app
        .get(&format!("/leaves/report/summary?year={y}&workdayHours=7.5"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let j = &res.json;
    assert_eq!(j["year"], y);
    assert_eq!(j["workdayHours"], 7.5);
    assert_eq!(
        j["statusBreakdown"],
        json!({ "pending": 1, "approved": 1, "rejected": 0, "needsRevision": 0, "cancelled": 0 })
    );
    assert_eq!(
        j["approvalTime"],
        json!({ "avgHours": 3, "minHours": 3, "maxHours": 3, "decidedCount": 1 })
    );
    assert_eq!(j["usageByLeaveType"].as_array().unwrap().len(), 2);
    let staff = j["employees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["userId"] == fx.staff.id)
        .unwrap();
    assert_eq!(
        staff["balance"]["annual"],
        json!({ "total": 12, "used": 2, "carryOverAvailable": 0, "remaining": 10 })
    );
    assert_eq!(staff["balance"]["sick"], Value::Null);
    assert_eq!(
        staff["leave"],
        json!({ "requestedDays": 2, "approvedDays": 2 })
    );
    assert_eq!(staff["comparison"]["leaveHours"], 15);
    app.get("/leaves/report/summary?year=1999")
        .token(tok)
        .send()
        .await
        .assert_status(400);

    // Overview.
    let res = app
        .get(&format!("/leaves/overview?year={y}&month=10"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let j = &res.json;
    assert_eq!(j["stats"]["pendingCount"], 1);
    assert_eq!(j["stats"]["approvalRatePercent"], 100);
    assert_eq!(j["stats"]["quotaYear"], y);
    let pa = &j["pendingApprovals"][0];
    assert_eq!(pa["uuid"], p.uuid);
    assert_eq!(pa["leaveTypeName"], "SICK_LEAVE");
    assert_eq!(pa["hasConflict"], true);
    assert_eq!(j["usageByType"]["totalDays"], 2);
    assert_eq!(j["usageByType"]["items"][0]["percentage"], 100);
    assert_eq!(j["workVsLeaveThisMonth"]["monthLabel"], "Oct");
    let staff = j["workVsLeaveThisMonth"]["employees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["userId"] == fx.staff.id)
        .unwrap();
    assert_eq!(staff["leaveDays"], 2);
    assert_eq!(staff["leaveHours"], 16);
    app.get("/leaves/overview?month=13")
        .token(tok)
        .send()
        .await
        .assert_status(400);
}
