//! Uji integrasi route operator PDF invoice (`GET /admin-invoices/{uuid}/pdf`,
//! `POST /admin-invoices/{uuid}/regenerate-pdf`) terhadap Postgres nyata + emulator GCS di proses
//! uji (tidak ada GCS sungguhan). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-billing --test it_billing_pdf`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use axum::Router;
use common::{BUCKET, Emulator, start_emulator};
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::json;

fn build(state: &AppState) -> Router {
    mz_billing::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn app_with_gcs() -> Option<(TestApp, Emulator)> {
    let em = start_emulator().await;
    let vars = [
        ("STORAGE_EMULATOR_HOST", em.host.as_str()),
        ("GCP_BUCKET_NAME", BUCKET),
    ];
    let app = TestApp::with_vars(build, &vars).await?;
    Some((app, em))
}

async fn operator(app: &TestApp, role: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Operator', 1, '{role}') RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

struct Inv {
    id: i32,
    uuid: String,
    number: String,
    company_id: i32,
}

/// Company (dengan owner default `userCompany`) + subscription + invoice + 1 item.
async fn invoice(app: &TestApp, status: &str, provider: &str, path_pdf: &str) -> Inv {
    let c = app.company().await;
    let tag = uniq()[..10].to_uppercase();
    let number = format!("INV/{tag}");
    let sub: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, quantity, "pricePerUser")
               VALUES ({}, (SELECT MIN(id) FROM "pricingTiers"), '2026-11-01', '2026-12-01', 'ACTIVE', 5, 1.5)
               RETURNING id"#,
            c.id
        ))
        .await;
    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "taxPrice", "totalPrice",
                                 status, "paymentProvider", currency, "expiredAt", "invoicePathPdf")
           VALUES ($1, $2, 'https://pay.example/x', 7.5, 0.83, 8.33, $3::"InvoiceStatus", $4, 'USD', '2026-12-01', $5)
           RETURNING id, uuid::text"#,
    )
    .bind(sub)
    .bind(&number)
    .bind(status)
    .bind(provider)
    .bind(path_pdf)
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ({id}, 'Monthly Subscription - 5 user(s)', 5, 1.5, 7.5)"#
    ))
    .await;
    Inv {
        id,
        uuid,
        number,
        company_id: c.id,
    }
}

async fn path_pdf(app: &TestApp, id: i32) -> String {
    sqlx::query_scalar(r#"SELECT "invoicePathPdf" FROM invoices WHERE id = $1"#)
        .bind(id)
        .fetch_one(app.db())
        .await
        .unwrap()
}

/// Isi teks yang dirender di PDF (potongan hex `TJ` digabung).
fn pdf_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for line in s.lines().filter(|l| l.ends_with("TJ")) {
        let mut rest = line;
        while let Some(i) = rest.find('<') {
            let Some(j) = rest[i..].find('>') else { break };
            let hex = &rest[i + 1..i + j];
            for k in (0..hex.len()).step_by(2) {
                out.push(u8::from_str_radix(&hex[k..k + 2], 16).unwrap() as char);
            }
            rest = &rest[i + j + 1..];
        }
        out.push('\n');
    }
    out
}

fn is_v2_key(k: &str, number: &str) -> bool {
    k.starts_with(&format!("invoices/{number}-")) && k.ends_with(".v2.pdf")
}

#[tokio::test]
async fn gerbang_operator_dan_validasi() {
    let Some((app, _em)) = app_with_gcs().await else {
        return;
    };
    let c = app.company().await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let (_, owner) = operator(&app, "OWNER").await;
    let u = uuid::Uuid::new_v4();
    for (m, uri) in [
        (http::Method::GET, format!("/admin-invoices/{u}/pdf")),
        (
            http::Method::POST,
            format!("/admin-invoices/{u}/regenerate-pdf"),
        ),
    ] {
        // Tanpa token / token user biasa → 401 polos.
        let r = app.request(m.clone(), &uri).send().await;
        r.assert_status(401).assert_no_sql_error();
        assert_eq!(
            r.json,
            json!({ "message": "Unauthorized", "statusCode": 401 })
        );
        app.request(m.clone(), &uri)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
    }

    // SUPPORT boleh GET pdf (404 karena invoice tidak ada), tapi 403 untuk regenerate.
    let r = app
        .get(&format!("/admin-invoices/{u}/pdf"))
        .token(&support)
        .send()
        .await;
    r.assert_status(404).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "code": "INVOICE_NOT_FOUND", "message": "Invoice not found" })
    );
    let r = app
        .post(&format!("/admin-invoices/{u}/regenerate-pdf"))
        .token(&support)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(403).assert_no_sql_error();

    // ParseUUIDPipe.
    for (m, uri) in [
        (http::Method::GET, "/admin-invoices/not-a-uuid/pdf"),
        (
            http::Method::POST,
            "/admin-invoices/not-a-uuid/regenerate-pdf",
        ),
    ] {
        let r = app
            .request(m, uri)
            .token(&owner)
            .json(&json!({}))
            .send()
            .await;
        r.assert_status(400).assert_no_sql_error();
        assert_eq!(r.json["message"], "Validation failed (uuid is expected)");
    }

    // Body regenerate: ADMIN_VALIDATION_PIPE (whitelist + Length 5..500).
    let url = format!("/admin-invoices/{u}/regenerate-pdf");
    let r = app
        .post(&url)
        .token(&owner)
        .json(&json!({ "reason": "abc" }))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!(["reason must be longer than or equal to 5 characters"])
    );
    let r = app
        .post(&url)
        .token(&owner)
        .json(&json!({ "foo": 1 }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["property foo should not exist"]));

    // Invoice tidak ada → 404 ber-code.
    let r = app
        .post(&url)
        .token(&owner)
        .json(&json!({ "reason": "regenerate please" }))
        .send()
        .await;
    r.assert_status(404).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "code": "INVOICE_NOT_FOUND", "message": "Invoice not found" })
    );
}

#[tokio::test]
async fn get_pdf_membuat_menyimpan_dan_memakai_ulang() {
    let Some((app, em)) = app_with_gcs().await else {
        return;
    };
    let (_, support) = operator(&app, "SUPPORT").await;
    let inv = invoice(&app, "UNPAID", "paypal", "").await;
    let url = format!("/admin-invoices/{}/pdf", inv.uuid);

    let r = app.get(&url).token(&support).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.headers["content-type"], "application/pdf");
    assert_eq!(r.headers["cache-control"], "private, no-cache");
    let disp = r.headers["content-disposition"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(disp.contains(".pdf"), "{disp}");
    assert!(r.bytes.starts_with(b"%PDF-"));
    let text = pdf_text(&r.bytes);
    assert!(text.contains("INVOICE"), "{text}");
    assert!(text.contains(&format!("#{}", inv.number)), "{text}");
    assert!(text.contains("UNPAID"));
    assert!(text.contains("Monthly Subscription - 5 user(s)"));
    assert!(text.contains("$8.33"));
    assert!(text.contains("https://pay.example/x"));

    // Key ber-versi Node, tersimpan di DB + bucket.
    let key = path_pdf(&app, inv.id).await;
    assert!(is_v2_key(&key, &inv.number), "{key}");
    assert_eq!(em.keys(), vec![key.clone()]);
    assert_eq!(em.get(&key).unwrap(), r.bytes);

    // Buka lagi: tidak membuat object baru.
    let r2 = app.get(&url).token(&support).send().await;
    r2.assert_status(200);
    assert_eq!(path_pdf(&app, inv.id).await, key);
    assert_eq!(em.keys(), vec![key.clone()]);
    assert!(em.deleted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn get_pdf_layout_lama_dibuat_ulang_dan_object_lama_dihapus() {
    let Some((app, em)) = app_with_gcs().await else {
        return;
    };
    let (_, owner) = operator(&app, "OWNER").await;
    let inv = invoice(&app, "PAID", "manual", "invoices/old-layout.v1.pdf").await;
    em.put("invoices/old-layout.v1.pdf", b"%PDF-old");

    let r = app
        .get(&format!("/admin-invoices/{}/pdf", inv.uuid))
        .token(&owner)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let key = path_pdf(&app, inv.id).await;
    assert!(is_v2_key(&key, &inv.number), "{key}");
    assert_eq!(em.keys(), vec![key]);
    assert_eq!(
        em.deleted.lock().unwrap().clone(),
        vec!["invoices/old-layout.v1.pdf".to_owned()]
    );
    // Invoice manual PAID: label provider penuh + cap PAID.
    let text = pdf_text(&r.bytes);
    assert!(text.contains("Bank transfer / manual"), "{text}");
    assert!(text.contains("PAID"));
}

#[tokio::test]
async fn regenerate_pdf_mengganti_key_dan_menulis_audit() {
    let Some((app, em)) = app_with_gcs().await else {
        return;
    };
    let (op, owner) = operator(&app, "FINANCE").await;
    let inv = invoice(&app, "UNPAID", "paypal", "").await;
    // PDF pertama.
    app.get(&format!("/admin-invoices/{}/pdf", inv.uuid))
        .token(&owner)
        .send()
        .await
        .assert_status(200);
    let old = path_pdf(&app, inv.id).await;
    assert!(is_v2_key(&old, &inv.number));
    // Detik unix pada key harus berbeda.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let r = app
        .post(&format!("/admin-invoices/{}/regenerate-pdf", inv.uuid))
        .token(&owner)
        .json(&json!({ "reason": "  logo diganti  " }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({
            "success": true,
            "message": "Invoice PDF regenerated",
            "data": { "url": format!("/invoices/{}/pdf", inv.uuid) },
        })
    );
    let new = path_pdf(&app, inv.id).await;
    assert!(is_v2_key(&new, &inv.number));
    assert_ne!(new, old);
    assert_eq!(em.keys(), vec![new]);
    assert_eq!(em.deleted.lock().unwrap().clone(), vec![old]);

    type AuditRow = (String, String, Option<String>, Option<i32>, Option<String>);
    let rows: Vec<AuditRow> = sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, "companyId", reason
             FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(
            "invoice.regenerate_pdf".to_owned(),
            "invoice".to_owned(),
            Some(inv.uuid.clone()),
            Some(inv.company_id),
            Some("logo diganti".to_owned()),
        )]
    );
    // Status/nominal tidak berubah.
    let (status, total): (String, f64) =
        sqlx::query_as(r#"SELECT status::text, "totalPrice" FROM invoices WHERE id = $1"#)
            .bind(inv.id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!((status.as_str(), total), ("UNPAID", 8.33));

    // Tanpa body juga sah (reason opsional) dan tanpa key lama.
    let inv2 = invoice(&app, "PAID", "manual", "").await;
    let r = app
        .post(&format!("/admin-invoices/{}/regenerate-pdf", inv2.uuid))
        .token(&owner)
        .send()
        .await;
    r.assert_status(200);
    assert!(is_v2_key(&path_pdf(&app, inv2.id).await, &inv2.number));
}

#[tokio::test]
async fn regenerate_gagal_unggah_mengembalikan_pointer_lama() {
    let Some((app, em)) = app_with_gcs().await else {
        return;
    };
    let (_, owner) = operator(&app, "OWNER").await;
    let old = "invoices/keep-me.v2.pdf";
    let inv = invoice(&app, "UNPAID", "paypal", old).await;
    em.put(old, b"%PDF-keep");
    em.set_fail_upload(true);

    let r = app
        .post(&format!("/admin-invoices/{}/regenerate-pdf", inv.uuid))
        .token(&owner)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(500);
    // Pointer lama dikembalikan, object lama tidak dihapus.
    assert_eq!(path_pdf(&app, inv.id).await, old);
    assert_eq!(em.keys(), vec![old.to_owned()]);
}
