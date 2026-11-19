//! Uji integrasi template & import CSV kuota cuti (`/leave-policies/balances/import*`, Node
//! c3082b6) terhadap Postgres nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-leave-config --test it_leave_cfg_balance_import`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use chrono::Datelike;
use common::*;
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

/// `userId`, `balanceId`, `adjustedById`, `field`, `delta`, `reason`, `source`.
type AdjRow = (i32, i32, Option<i32>, String, f64, String, String);

fn this_year() -> i32 {
    chrono::Utc::now().year()
}

fn multipart(field: &str, filename: &str, ctype: &str, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----ITBOUNDARYkuotaCuti";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {ctype}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn upload_as(
    app: &TestApp,
    uri: &str,
    token: &str,
    filename: &str,
    ctype: &str,
    data: &[u8],
) -> TestResponse {
    let (ct, body) = multipart("file", filename, ctype, data);
    app.post(uri).token(token).raw(&ct, body).send().await
}

async fn upload(app: &TestApp, uri: &str, token: &str, csv: &str) -> TestResponse {
    upload_as(app, uri, token, "kuota.csv", "text/csv", csv.as_bytes()).await
}

async fn setup(app: &TestApp) -> Fx {
    let fx = fixture(app).await;
    app.put("/leave-policies")
        .token(&fx.admin.token)
        .json(&json!({"annualLeaveDays": 12}))
        .send()
        .await
        .assert_status(200);
    fx
}

async fn username(app: &TestApp, id: i32) -> String {
    app.scalar(&format!("SELECT username FROM users WHERE id = {id}"))
        .await
}

async fn joined_2020(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"UPDATE "userCompany" SET "createdAt" = '2020-01-01' WHERE "companyId" = {company_id}"#
    ))
    .await;
}

fn code(r: &TestResponse) -> &str {
    r.json["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn template_semua_anggota_aktif_urut_nama() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = setup(&app).await;
    let evil = employee(&app, fx.id, "=Evil, Name").await;
    let gone = employee(&app, fx.id, "Keluar").await;
    app.exec(&format!(
        r#"UPDATE "userCompany" SET status = 0 WHERE "userId" = {gone} AND "companyId" = {}"#,
        fx.id
    ))
    .await;

    let r = app
        .get("/leave-policies/balances/import-template")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers["content-type"].to_str().unwrap(),
        "text/csv; charset=utf-8"
    );
    assert_eq!(
        r.headers["content-disposition"].to_str().unwrap(),
        "attachment; filename=\"template-import-kuota-cuti.csv\""
    );
    assert_eq!(
        r.headers["content-length"].to_str().unwrap(),
        r.bytes.len().to_string()
    );

    let members: Vec<(String, String)> = sqlx::query_as(&format!(
        r#"SELECT u.name, u.username FROM users u JOIN "userCompany" uc ON uc."userId" = u.id
           WHERE uc."companyId" = {} AND uc.status = 1 ORDER BY u.name, u.id"#,
        fx.id
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(members.len(), 4);
    let evil_username = username(&app, evil).await;
    let mut expected = String::from("\u{FEFF}Nama,Email,Kuota Cuti\r\n");
    for (name, email) in &members {
        let name = if *name == "=Evil, Name" {
            "\"'=Evil, Name\"".to_owned()
        } else {
            name.clone()
        };
        expected.push_str(&format!("{name},{email},0\r\n"));
    }
    assert_eq!(r.text(), expected);
    assert!(
        r.text()
            .contains(&format!("\"'=Evil, Name\",{evil_username},0\r\n"))
    );
    assert!(!r.text().contains("Keluar"));

    app.get("/leave-policies/balances/import-template")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn preview_tidak_menulis_dan_import_menimpa_kuota() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = setup(&app).await;
    let other = fixture(&app).await;
    let year = this_year();
    let e1 = employee(&app, fx.id, "Satu").await;
    let e2 = employee(&app, fx.id, "Dua").await;
    let e3 = employee(&app, fx.id, "Tiga").await;
    joined_2020(&app, fx.id).await;
    let b1: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed")
               VALUES ({e1}, {}, {year}, 12, 4) RETURNING id"#,
            fx.id
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed")
           VALUES ({e3}, {}, {year}, 10, 6)"#,
        fx.id
    ))
    .await;
    let (u1, u2, u3) = (
        username(&app, e1).await,
        username(&app, e2).await,
        username(&app, e3).await,
    );
    let foreign = other.staff.username.clone();
    let csv = format!(
        "Nama,Email,Kuota Cuti\n\
         Satu,{},14.5\n\
         Dua,'{u2},\"6,5\"\n\
         Tiga,{u3},5\n\
         Asing,{foreign},3\n\
         Satu lagi,{u1},20\n\
         Admin,{},abc\n",
        u1.to_uppercase(),
        fx.admin.username
    );

    let r = upload(
        &app,
        "/leave-policies/balances/import/preview",
        &fx.admin.token,
        &csv,
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(&r.json, &["year", "toUpdate", "unchanged", "invalid"]);
    assert_eq!(r.json["year"], json!(year));
    assert_eq!(
        r.json["toUpdate"],
        json!([
            {"row": 2, "userId": e1, "name": "Satu", "email": u1, "quota": 14.5, "previous": 12},
            {"row": 3, "userId": e2, "name": "Dua", "email": u2, "quota": 6.5, "previous": null},
        ])
    );
    assert_eq!(r.json["unchanged"], json!([]));
    let invalid: Vec<(i64, Value)> = r.json["invalid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["row"].as_i64().unwrap(), i["errors"].clone()))
        .collect();
    assert_eq!(
        invalid,
        [
            (
                4,
                json!(["kuota cuti tidak boleh di bawah cuti yang sudah terpakai (6 hari)"])
            ),
            (
                5,
                json!(["email tidak ditemukan di karyawan aktif company"])
            ),
            (6, json!(["email duplikat dengan baris 2"])),
            (
                7,
                json!(["kuota cuti harus angka 0–365 (maksimal 2 desimal)"])
            ),
        ]
    );
    assert_eq!(
        r.json["invalid"][1]["raw"],
        json!({"nama": "Asing", "email": foreign, "kuota cuti": "3"})
    );
    let count: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leaveBalance" WHERE "companyId" = {} AND "userId" = {e2}"#,
            fx.id
        ))
        .await;
    assert_eq!(count, 0, "preview tidak boleh membuat saldo");

    let r = upload(
        &app,
        "/leave-policies/balances/import",
        &fx.manager.token,
        &csv,
    )
    .await;
    r.assert_status(201).assert_no_sql_error();
    assert_keys(
        &r.json,
        &["year", "updated", "unchanged", "initialized", "invalid"],
    );
    assert_eq!(r.json["updated"], json!(2));
    assert_eq!(r.json["unchanged"], json!(0));
    assert_eq!(r.json["initialized"], json!(1));
    assert_eq!(r.json["invalid"].as_array().unwrap().len(), 4);

    let totals: Vec<(i32, f64)> = sqlx::query_as(&format!(
        r#"SELECT "userId", "annualLeaveTotal" FROM "leaveBalance"
           WHERE "companyId" = {} AND year = {year} AND "userId" IN ({e1}, {e2}, {e3}) ORDER BY "userId""#,
        fx.id
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(totals, [(e1, 14.5), (e2, 6.5), (e3, 10.0)]);

    let adj: Vec<AdjRow> = sqlx::query_as(&format!(
        r#"SELECT "userId", "balanceId", "adjustedById", field, delta, reason, source::text
           FROM "leaveBalanceAdjustment"
           WHERE "companyId" = {} AND reason = 'Import kuota cuti dari CSV' ORDER BY id"#,
        fx.id
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    let b2: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "leaveBalance" WHERE "userId" = {e2} AND "companyId" = {} AND year = {year}"#,
            fx.id
        ))
        .await;
    let reason = "Import kuota cuti dari CSV".to_owned();
    assert_eq!(
        adj,
        [
            (
                e1,
                b1,
                Some(fx.manager.id),
                "annualLeaveTotal".to_owned(),
                2.5,
                reason.clone(),
                "ADJUSTMENT".to_owned()
            ),
            // Saldo baru dibuat dengan jatah kebijakan (12) lalu ditimpa 6.5.
            (
                e2,
                b2,
                Some(fx.manager.id),
                "annualLeaveTotal".to_owned(),
                -5.5,
                reason,
                "ADJUSTMENT".to_owned()
            ),
        ]
    );

    // Diulang: semua sudah sama, tidak ada tulisan baru.
    let r = upload(
        &app,
        "/leave-policies/balances/import",
        &fx.admin.token,
        &csv,
    )
    .await;
    r.assert_status(201);
    assert_eq!(r.json["updated"], json!(0));
    assert_eq!(r.json["unchanged"], json!(2));
    assert_eq!(r.json["initialized"], json!(0));
}

#[tokio::test]
async fn pemisah_titik_koma_dan_tahun_query() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = setup(&app).await;
    joined_2020(&app, fx.id).await;
    let csv = format!(
        "\u{FEFF}Nama;EMAIL;Kuota_Cuti\r\nStaff;{};12,25\r\n",
        fx.staff.username
    );
    let r = upload(
        &app,
        "/leave-policies/balances/import?year=2095",
        &fx.admin.token,
        &csv,
    )
    .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({"year": 2095, "updated": 1, "unchanged": 0, "initialized": 1, "invalid": []})
    );
    let total: f64 = app
        .scalar(&format!(
            r#"SELECT "annualLeaveTotal" FROM "leaveBalance"
               WHERE "userId" = {} AND "companyId" = {} AND year = 2095"#,
            fx.staff.id, fx.id
        ))
        .await;
    assert_eq!(total, 12.25);
}

#[tokio::test]
async fn validasi_berkas_dan_query() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = setup(&app).await;
    let tok = &fx.admin.token;
    let ok_csv = format!("Email,Kuota Cuti\n{},1\n", fx.staff.username);

    for uri in [
        "/leave-policies/balances/import",
        "/leave-policies/balances/import/preview",
    ] {
        // Tanpa berkas.
        let r = app.post(uri).token(tok).send().await;
        r.assert_status(422);
        assert_eq!(code(&r), "IMPORT_INVALID_FILE");
        assert_eq!(r.json["message"], json!("No file uploaded"));

        let r = upload_as(&app, uri, tok, "kuota.xlsx", "text/csv", ok_csv.as_bytes()).await;
        r.assert_status(422);
        assert_eq!(r.json["message"], json!("File must have a .csv extension"));

        let r = upload_as(&app, uri, tok, "kuota.csv", "image/png", ok_csv.as_bytes()).await;
        r.assert_status(422);
        assert_eq!(
            r.json["message"],
            json!("File type is not supported, upload a CSV file")
        );

        let r = upload(&app, uri, tok, "Nama,Email\nA,a@x.com\n").await;
        r.assert_status(422);
        assert_eq!(
            r.json["message"],
            json!("Kolom wajib tidak ada: kuota cuti")
        );

        let r = upload(&app, uri, tok, "Nama,Email,Kuota Cuti\nA,a@x.com,12,5\n").await;
        r.assert_status(422);
        assert_eq!(
            r.json["message"],
            json!("Invalid Record Length: columns length is 3, got 4 on line 2")
        );

        let r = upload(&app, &format!("{uri}?year=2019"), tok, &ok_csv).await;
        r.assert_status(400);
        assert_eq!(messages(&r), ["year must not be less than 2020"]);
        let r = upload(&app, &format!("{uri}?year=abc"), tok, &ok_csv).await;
        r.assert_status(400);
        assert_eq!(
            messages(&r),
            [
                "year must not be greater than 2100",
                "year must not be less than 2020",
                "year must be an integer number"
            ]
        );

        // > 1 MB → 413, sebelum validasi query (FileInterceptor jalan sebelum pipa).
        let big = format!("Email,Kuota Cuti\n{}", "x".repeat(1_048_577));
        let r = upload(&app, &format!("{uri}?year=abc"), tok, &big).await;
        r.assert_status(413);
        assert_eq!(code(&r), "IMPORT_FILE_TOO_LARGE");
        assert_eq!(r.json["message"], json!("File exceeds the 1 MB limit"));

        // Staff tidak boleh.
        upload(&app, uri, &fx.staff.token, &ok_csv)
            .await
            .assert_status(401);
    }
}
