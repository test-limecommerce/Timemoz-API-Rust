//! Uji integrasi 3 route laporan lembur HR (`/v1/overtime/reports*`) terhadap Postgres NYATA.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_reports`.
//! Data uji ada di Maret 2019 (rentang yang tidak dipakai data template); semua test
//! memakai baris yang sama (di-reset sekali) dan dijalankan berurutan lewat `SERIAL`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::Value;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const BASE: &str = "/v1/overtime/reports";
const PERIOD: &str = "from=2019-03-01&to=2019-03-31";
const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

fn build(state: &AppState) -> Router {
    mz_overtime::router(state).with_state(state.clone())
}

struct Fx {
    app: TestApp,
    c: Company,
    staff_name: String,
    manager_name: String,
}

async fn setup() -> Option<Fx> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "timeZone" = 'Asia/Jakarta', "workweekStartDay" = 1 WHERE id = {}"#,
        c.id
    ))
    .await;
    // Template anonim belum memberi role bawaan izin fitur `overtime-report` (produksi: backfill
    // registry Node, STAFF `withheldFromBuiltIn`).
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'overtime-report', 'READ'::"PermissionAction" FROM "accessLevel" al
           WHERE al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER') AND al."isSystem"
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    // Reset data uji (idempotent): permintaan lembur + timeline Maret 2019 milik fixture.
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {} AND date BETWEEN '2019-02-01' AND '2019-04-30'"#,
        c.id
    ))
    .await;
    app.exec(&format!(
        r#"DELETE FROM "trackingTimeline" WHERE "trackingId" IN
             (SELECT id FROM tracking WHERE "userId" IN ({}, {})
              AND "takenDate" BETWEEN '2019-02-01' AND '2019-04-30')"#,
        c.staff.id, c.staff2.id
    ))
    .await;

    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let code = format!("OR{}", &uniq[..10]);
    app.exec(&format!(
        r#"INSERT INTO "projectType" (code, name, status) VALUES ('PT{code}', 'PT {code}', 1)"#
    ))
    .await;
    let project_id: i32 = app
        .scalar(&format!(
            r#"WITH ins AS (
                 INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status, billable)
                 VALUES ('{code}', 'Proj {code}', 0, 0,
                   (SELECT id FROM "projectType" WHERE code = 'PT{code}'), '2020-01-01', 1, true)
                 RETURNING id)
               SELECT id FROM ins"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({project_id}, {}, 1)"#,
        c.id
    ))
    .await;

    // Senin-Jumat 4-8 Maret 2019: 08:00-17:00 WIB (9 jam) tiap hari = 45 jam.
    for day in 4..=8 {
        let taken = format!("2019-03-{day:02}");
        let tracking: i32 = app
            .scalar(&format!(
                r#"WITH ins AS (
                     INSERT INTO tracking ("takenDate", "userId", status, "isAbsent") VALUES ('{taken}', {}, 1, false)
                     ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1
                     RETURNING id)
                   SELECT id FROM ins"#,
                c.staff.id
            ))
            .await;
        app.exec(&format!(
            r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
                 timezone, "trackingId", "projectId", status)
               VALUES ('ot', false, '{taken}T01:00:00Z', '{taken}T10:00:00Z', '{taken}T01:00:00Z', '{taken}T10:00:00Z',
                 'Asia/Jakarta', {tracking}, {project_id}, 1)"#
        ))
        .await;
    }

    let (comp, staff, staff2, manager) = (c.id, c.staff.id, c.staff2.id, c.manager.id);
    // staff: COMPLETED 5 Maret (final, snapshot hari kerja), REJECTED 6 Maret & CANCELLED 9 Maret
    // (tanpa snapshot -> jenis hari live: Rabu kerja, Sabtu libur).
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec", reason,
             status, "retroWindowDays", "responseDeadline", "finalAt", "dayType", "baseNormalSec", "dailyLimitSec",
             "patternWorkDays", "approvedAt", "workedSec", "normalSec", "overtimeSec", "computedAt", "finalizedAt",
             "createdAt")
           VALUES ({comp}, {staff}, {staff}, 'SELF', '2019-03-05', 14400, 'uji', 'COMPLETED', 7,
             '2019-03-12T16:59:59.999Z', '2019-03-12T16:59:59.999Z', 'WORKDAY', 28800, 14400, 5, now(),
             32400, 28800, 3600, now(), now(), '2019-03-05T00:00:00Z')"#
    ))
    .await;
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec", reason,
             status, "retroWindowDays", "responseDeadline", "finalAt", "rejectReason", "createdAt")
           VALUES ({comp}, {staff}, {staff}, 'SELF', '2019-03-06', 3600, 'uji', 'REJECTED', 7,
             '2019-03-13T16:59:59.999Z', '2019-03-13T16:59:59.999Z', 'Tidak perlu', '2019-03-06T00:00:00Z')"#
    ))
    .await;
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec", reason,
             status, "retroWindowDays", "responseDeadline", "finalAt", "cancelReason", "cancelledAt", "createdAt")
           VALUES ({comp}, {staff}, {staff}, 'SELF', '2019-03-09', 3600, 'uji', 'CANCELLED', 7,
             '2019-03-16T16:59:59.999Z', '2019-03-16T16:59:59.999Z', 'NO_RESPONSE', now(), '2019-03-09T00:00:00Z')"#
    ))
    .await;
    // staff2: perintah dari manager, disetujui tapi belum final & melebihi batas harian.
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec", reason,
             status, "retroWindowDays", "responseDeadline", "finalAt", "decidedById", "decidedAt", "dayType",
             "baseNormalSec", "dailyLimitSec", "patternWorkDays", "approvedAt", "exceedsDailyLimit", "exceedsLimit",
             "createdAt")
           VALUES ({comp}, {staff2}, {manager}, 'ORDER', '2019-03-05', 18000, 'uji', 'APPROVED', 7,
             '2019-03-05T16:59:59.999Z', '2019-03-12T16:59:59.999Z', {staff2}, now(), 'WORKDAY', 28800, 14400, 5,
             now(), true, true, '2019-03-05T00:00:00Z')"#
    ))
    .await;

    let name_of = |id: i32| {
        let app = &app;
        async move {
            app.scalar::<String>(&format!("SELECT name FROM users WHERE id = {id}"))
                .await
        }
    };
    let staff_name = name_of(staff).await;
    let manager_name = name_of(manager).await;
    Some(Fx {
        app,
        c,
        staff_name,
        manager_name,
    })
}

fn data(res: &TestResponse) -> &Value {
    &res.json["data"]
}

fn find_employee<'a>(report: &'a Value, uuid: &str) -> &'a Value {
    report["employees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["employee"]["id"] == uuid)
        .unwrap_or_else(|| panic!("karyawan {uuid} tidak ada: {report}"))
}

fn messages(res: &TestResponse) -> Vec<String> {
    res.json["message"]
        .as_array()
        .unwrap_or_else(|| panic!("message bukan array: {}", res.json))
        .iter()
        .map(|m| m.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn daily_report_shape_rows_and_live_day_types() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    let users = format!("userIds={}&userIds={}", c.staff.uuid, c.staff2.uuid);
    let res = app
        .get(&format!("{BASE}?{PERIOD}&{users}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let d = data(&res);
    assert_eq!(d["period"]["from"], "2019-03-01");
    assert_eq!(d["period"]["to"], "2019-03-31");
    assert!(
        d["legalNote"]
            .as_str()
            .unwrap()
            .starts_with("Batas lembur PP 35/2021")
    );
    assert_eq!(d["employees"].as_array().unwrap().len(), 2);

    let staff = find_employee(d, &c.staff.uuid);
    assert_eq!(staff["employee"]["name"], fx.staff_name.as_str());
    assert_eq!(staff["employee"]["email"], c.staff.username.as_str());
    assert_eq!(staff["totalOvertimeMinutes"], 60);
    assert_eq!(staff["totalOvertimeSeconds"], 3600);
    assert_eq!(staff["notFinal"], false);
    let days = staff["days"].as_array().unwrap();
    let dates: Vec<&str> = days.iter().map(|x| x["date"].as_str().unwrap()).collect();
    assert_eq!(dates, ["2019-03-05", "2019-03-06", "2019-03-09"]);
    let [done, rejected, cancelled] = [&days[0], &days[1], &days[2]];
    assert_eq!(done["status"], "COMPLETED");
    assert_eq!(done["kind"], "SELF");
    assert_eq!(done["dayType"], "WORKDAY");
    assert_eq!(done["approvedMinutes"], 240);
    assert_eq!(done["workedMinutes"], 540);
    assert_eq!(done["overtimeMinutes"], 60);
    assert_eq!(done["dailyLimitMinutes"], 240);
    assert_eq!(done["isFinal"], true);
    assert_eq!(done["note"], Value::Null);
    assert_eq!(done["requestedByName"], fx.staff_name.as_str());
    // Tanpa snapshot: jenis hari live (Rabu kerja, Sabtu libur); angka tercatat null.
    assert_eq!(rejected["dayType"], "WORKDAY");
    assert_eq!(rejected["note"], "Tidak perlu");
    assert_eq!(rejected["workedMinutes"], Value::Null);
    assert_eq!(rejected["overtimeMinutes"], Value::Null);
    assert_eq!(rejected["dailyLimitMinutes"], Value::Null);
    assert_eq!(rejected["isFinal"], true);
    assert_eq!(cancelled["dayType"], "HOLIDAY");
    assert_eq!(cancelled["note"], "Tidak direspons sampai batas waktu");
    assert_eq!(cancelled["isFinal"], true);

    let staff2 = find_employee(d, &c.staff2.uuid);
    assert_eq!(staff2["notFinal"], true);
    assert_eq!(staff2["totalOvertimeMinutes"], 0);
    let order = &staff2["days"][0];
    assert_eq!(order["kind"], "ORDER");
    assert_eq!(order["requestedByName"], fx.manager_name.as_str());
    assert_eq!(order["status"], "APPROVED");
    assert_eq!(order["exceedsDailyLimit"], true);
    assert_eq!(order["exceedsLimit"], true);
    assert_eq!(order["isFinal"], false);
    assert_eq!(order["approvedMinutes"], 300);

    // Urutan karyawan = nama lalu id.
    let names: Vec<&str> = d["employees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["employee"]["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_by_key(|n| n.to_lowercase());
    assert_eq!(names, sorted);
}

#[tokio::test]
async fn daily_report_filters() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    let get = |qs: String| {
        let url = format!("{BASE}?{PERIOD}&{qs}");
        let token = c.manager.token.clone();
        async move { app.get(&url).token(&token).send().await }
    };
    let both = format!("userIds={},{}", c.staff.uuid, c.staff2.uuid);

    let res = get(format!("{both}&status=COMPLETED")).await;
    res.assert_ok();
    let rows: usize = data(&res)["employees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["days"].as_array().unwrap().len())
        .sum();
    assert_eq!(rows, 1);

    // dayType memakai jenis hari live untuk baris tanpa snapshot.
    let res = get(format!("{both}&dayType=HOLIDAY")).await;
    res.assert_ok();
    let employees = data(&res)["employees"].as_array().unwrap();
    assert_eq!(employees.len(), 1);
    assert_eq!(employees[0]["days"][0]["date"], "2019-03-09");

    let res = get(format!("{both}&exceedsLimit=true")).await;
    res.assert_ok();
    let employees = data(&res)["employees"].as_array().unwrap();
    assert_eq!(employees.len(), 1);
    assert_eq!(employees[0]["employee"]["id"], c.staff2.uuid.as_str());

    let res = get(format!("userIds={}&status=PENDING_APPROVAL", c.staff.uuid)).await;
    res.assert_ok();
    assert_eq!(data(&res)["employees"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn weekly_excess_report() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    let res = app
        .get(&format!(
            "{BASE}/weekly-excess?{PERIOD}&userIds={}",
            c.staff.uuid
        ))
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_ok().assert_no_sql_error();
    let d = data(&res);
    assert_eq!(d["label"], "Melebihi jam kerja mingguan");
    let months = d["months"].as_array().unwrap();
    assert_eq!(months.len(), 1, "{d}");
    assert_eq!(months[0]["month"], "2019-03");
    let emp = &months[0]["employees"][0];
    assert_eq!(emp["employee"]["id"], c.staff.uuid.as_str());
    assert_eq!(emp["totalExcessMinutes"], 240);
    let weeks = emp["weeks"].as_array().unwrap();
    assert_eq!(weeks.len(), 1);
    let w = &weeks[0];
    assert_eq!(w["weekStart"], "2019-03-04");
    assert_eq!(w["weekEnd"], "2019-03-10");
    assert_eq!(w["workedMinutes"], 2700);
    assert_eq!(w["normalMinutes"], 2400);
    assert_eq!(w["overtimeMinutes"], 60);
    assert_eq!(w["excessMinutes"], 240);
    assert_eq!(w["notFinal"], false);

    // staff2 tidak punya jam kerja: tidak ada kelebihan.
    let res = app
        .get(&format!(
            "{BASE}/weekly-excess?{PERIOD}&userIds={}",
            c.staff2.uuid
        ))
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(data(&res)["months"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn export_xlsx_and_pdf_headers_and_bodies() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    let users = format!("userIds={}&userIds={}", c.staff.uuid, c.staff2.uuid);

    let res = app
        .get(&format!("{BASE}/export?{PERIOD}&{users}&format=xlsx"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.headers["content-type"], XLSX_MIME);
    assert_eq!(
        res.headers["content-disposition"],
        "attachment; filename=\"laporan-lembur-2019-03-01_2019-03-31.xlsx\""
    );
    assert_eq!(
        res.headers["content-length"].to_str().unwrap(),
        res.bytes.len().to_string()
    );
    assert!(res.bytes.starts_with(b"PK"));

    let res = app
        .get(&format!("{BASE}/export?{PERIOD}&{users}&format=pdf"))
        .token(&c.manager.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.headers["content-type"], "application/pdf");
    assert_eq!(
        res.headers["content-disposition"],
        "attachment; filename=\"laporan-lembur-2019-03-01_2019-03-31.pdf\""
    );
    assert_eq!(
        res.headers["content-length"].to_str().unwrap(),
        res.bytes.len().to_string()
    );
    assert!(res.bytes.starts_with(b"%PDF-"));
    assert!(res.bytes.ends_with(b"%%EOF\n"));

    // Tanpa data pun file valid.
    let res = app
        .get(&format!(
            "{BASE}/export?from=2019-05-01&to=2019-05-02&format=pdf&userIds={}",
            c.staff.uuid
        ))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert!(res.bytes.starts_with(b"%PDF-"));
}

#[tokio::test]
async fn validation_errors_are_400_with_class_validator_messages() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    for path in [BASE.to_owned(), format!("{BASE}/weekly-excess")] {
        let res = app.get(&path).token(&c.admin.token).send().await;
        res.assert_status(400);
        assert_eq!(res.json["error"], "Bad Request");
        assert_eq!(res.json["statusCode"], 400);
        assert_eq!(
            messages(&res),
            [
                "from harus tanggal YYYY-MM-DD yang valid",
                "to harus tanggal YYYY-MM-DD yang valid"
            ]
        );
        let res = app
            .get(&format!(
                "{path}?{PERIOD}&userIds=abc&status=DONE&dayType=WEEKEND&exceedsLimit=yes"
            ))
            .token(&c.admin.token)
            .send()
            .await;
        res.assert_status(400);
        assert_eq!(messages(&res).len(), 4);
        assert_eq!(messages(&res)[0], "each value in userIds must be a UUID");
    }
    let res = app
        .get(&format!("{BASE}/export?{PERIOD}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        messages(&res),
        ["format must be one of the following values: xlsx, pdf"]
    );
}

#[tokio::test]
async fn invalid_range_is_400_overtime_invalid_range() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    for path in [
        BASE.to_owned(),
        format!("{BASE}/weekly-excess"),
        format!("{BASE}/export"),
    ] {
        let extra = if path.ends_with("export") {
            "&format=pdf"
        } else {
            ""
        };
        let res = app
            .get(&format!("{path}?from=2019-03-31&to=2019-03-01{extra}"))
            .token(&c.admin.token)
            .send()
            .await;
        res.assert_status(400);
        assert_eq!(res.json["code"], "OVERTIME_INVALID_RANGE");
        assert_eq!(
            res.json["message"],
            "Tanggal awal tidak boleh setelah tanggal akhir."
        );
        assert_eq!(res.json["details"]["from"], "2019-03-31");

        let res = app
            .get(&format!("{path}?from=2019-01-01&to=2019-12-31{extra}"))
            .token(&c.admin.token)
            .send()
            .await;
        res.assert_status(400);
        assert_eq!(res.json["code"], "OVERTIME_INVALID_RANGE");
        assert_eq!(res.json["details"]["maxDays"], 92);
    }
}

#[tokio::test]
async fn roles_and_auth_are_enforced() {
    let _g = SERIAL.lock().await;
    let Some(fx) = setup().await else { return };
    let (app, c) = (&fx.app, &fx.c);
    for path in [
        format!("{BASE}?{PERIOD}"),
        format!("{BASE}/weekly-excess?{PERIOD}"),
        format!("{BASE}/export?{PERIOD}&format=xlsx"),
    ] {
        // Tanpa token.
        app.get(&path).send().await.assert_status(401);
        // STAFF ditolak (tanpa STAFF dan GUEST): 401 seperti RolesGuard Node.
        let res = app.get(&path).token(&c.staff.token).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json["message"],
            "Access denied: insufficient permissions"
        );
        // Role yang diizinkan.
        for token in [&c.super_admin.token, &c.admin.token, &c.manager.token] {
            app.get(&path).token(token).send().await.assert_status(200);
        }
    }
}
