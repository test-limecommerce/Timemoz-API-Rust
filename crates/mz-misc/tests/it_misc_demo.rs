//! Uji integrasi `POST /demo-requests` lewat HTTP terhadap Postgres & Redis nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-misc --test it_misc_demo`.
//!
//! Tidak ada email yang terkirim: `TestApp` menolak berjalan bila `MAILTRAP_TOKEN` diset, dan
//! tanpa token `mz_core::mail` gagal sebelum menyentuh jaringan. Jadi jalur sukses (`success: true`)
//! tidak bisa diuji di sini; yang diuji adalah jalur gagal kirim dan semua jalur tolak.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::json;

fn build(state: &AppState) -> Router {
    mz_misc::router(state).with_state(state.clone())
}

/// IP acak per binary/test: limiter Redis memakai DB yang dibagi antar test.
fn fresh_ip() -> String {
    let b = uuid::Uuid::new_v4().as_bytes().to_owned();
    format!("10.{}.{}.{}", b[1], b[2], b[3])
}

fn valid_body() -> serde_json::Value {
    json!({
        "name": "  Yudha Pratama ",
        "email": "Yudha@Distributor.co.id",
        "phone": "08123456789",
        "companyName": "PT Distributor Maju",
        "message": "tim 25 orang",
        "tidakDikenal": "diabaikan"
    })
}

#[tokio::test]
async fn publik_tanpa_token_dan_gagal_kirim_dilaporkan_200() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let res = app.post("/demo-requests").json(&valid_body()).send().await;
    res.assert_status(200).assert_no_sql_error();
    // Tanpa MAILTRAP_TOKEN email dianggap gagal: body persis bentuk Node.
    assert_eq!(
        res.json,
        json!({
            "success": false,
            "message": "demo request submitted but email failed to send",
            "data": null
        })
    );
}

#[tokio::test]
async fn validasi_menolak_body_kosong_dengan_pesan_class_validator() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let res = app.post("/demo-requests").json(&json!({})).send().await;
    res.assert_status(400);
    assert_eq!(res.json["error"], "Bad Request");
    assert_eq!(res.json["statusCode"], 400);
    assert_eq!(
        res.json["message"],
        json!([
            "name must be shorter than or equal to 120 characters",
            "name should not be empty",
            "name must be a string",
            "email must be shorter than or equal to 254 characters",
            "email should not be empty",
            "email must be an email"
        ])
    );
}

#[tokio::test]
async fn validasi_email_salah_dan_field_opsional_terlalu_panjang() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let body = json!({
        "name": "Budi",
        "email": "bukan-email",
        "phone": "1".repeat(31)
    });
    let res = app.post("/demo-requests").json(&body).send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "email must be an email",
            "phone must be shorter than or equal to 30 characters"
        ])
    );
}

#[tokio::test]
async fn limiter_tiga_per_menit_per_ip_lalu_429() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let ip = fresh_ip();
    for _ in 0..3 {
        let res = app
            .post("/demo-requests")
            .ip(&ip)
            .json(&valid_body())
            .send()
            .await;
        res.assert_status(200);
    }
    let res = app
        .post("/demo-requests")
        .ip(&ip)
        .json(&valid_body())
        .send()
        .await;
    res.assert_status(429);
    assert_eq!(
        res.json,
        json!({
            "success": false,
            "message": "Too many requests, please try again later."
        })
    );
}
