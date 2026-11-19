//! Uji integrasi route mobile form kunjungan (`/v1/mobile/store-visits/:visitUuid/forms/**`)
//! dan proxy file bertanda tangan `/v1/mobile/store-visit-forms/submissions/:uuid/files/:fileUuid`
//! terhadap Postgres nyata; GCS diganti emulator. Fixture `store`/`storeVisit`/template dibuat
//! lewat SQL dan dibersihkan di akhir tiap test.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-visit-forms --test it_visit_forms_mobile`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// `common::FILE_SECRET` < 32 byte (minimum `signFilePath`) → secret sendiri.
const FILE_SECRET: &str = "it-visit-forms-mobile-file-url-secret-0123";

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

async fn setup() -> Option<(TestApp, common::Emulator, Company)> {
    let em = common::start_emulator().await;
    let key_path = format!(
        "{}/it_visit_forms_mobile_sa.json",
        env!("CARGO_TARGET_TMPDIR")
    );
    std::fs::write(
        &key_path,
        json!({
            "client_email": "sa@test.iam.gserviceaccount.com",
            "private_key": include_str!("../../mz-files/src/test_key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    let vars = [
        ("STORAGE_EMULATOR_HOST", em.host.as_str()),
        ("GCP_BUCKET_NAME", common::BUCKET),
        ("GCP_KEY_PATH", key_path.as_str()),
        ("FILE_URL_SECRET", FILE_SECRET),
    ];
    let app = TestApp::with_vars(common::build, &vars).await?;
    let c = app.company().await;
    common::open_form_gates(&app, c.id).await;
    Some((app, em, c))
}

fn schema() -> Value {
    json!({
        "schemaVersion": 1,
        "fields": [
            {"key": "nama", "type": "text", "label": "Nama", "required": true,
             "validation": {"maxLength": 20}},
            {"key": "foto", "type": "photo", "label": "Foto"},
            {"key": "catatan", "type": "text", "label": "Catatan",
             "visibleIf": {"all": [{"field": "nama", "op": "eq", "value": "x"}]}}
        ]
    })
}

struct Fx {
    store_id: i32,
    visit_id: i32,
    visit: String,
    template_id: i32,
    template: String,
}

/// Toko + kunjungan aktif milik `user_id` + template `scope=all` terpublish v1.
async fn fixture(app: &TestApp, company_id: i32, user_id: i32) -> Fx {
    let tag = uuid::Uuid::new_v4().simple().to_string();
    // Satu kunjungan aktif per user (partial unique): tutup yang lama.
    app.exec(&format!(
        r#"UPDATE "storeVisit" SET state = 'completed' WHERE "userId" = {user_id} AND state = 'active'"#
    ))
    .await;
    let store_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById")
               VALUES ({company_id}, 'IT{}', 'Toko IT {tag}', 'Owner', 'Jl. Uji', -6.2, 106.8, {user_id})
               RETURNING id"#,
            &tag[..8]
        ))
        .await;
    let (visit_id, visit): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisit" ("companyId", "storeId", "userId", "visitNumber", "clientRequestId",
             "checkInAt", "checkInLat", "checkInLng", "checkInAccuracy", "checkInDistance", "checkInGpsValidation")
           VALUES ({company_id}, {store_id}, {user_id}, 'IT-{}', gen_random_uuid(), NOW(), -6.2, 106.8, 5, 0, 'valid')
           RETURNING id, uuid::text"#,
        &tag[..12]
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    let (template_id, template): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisitFormTemplate" ("companyId", name, "createdById", "sortOrder")
           VALUES ({company_id}, 'Form IT {tag}', {user_id}, -1000) RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "storeVisitFormTemplateVersion" ("templateId", version, schema, "publishedAt")
           VALUES ($1, 1, $2, NOW())"#,
    )
    .bind(template_id)
    .bind(schema())
    .execute(app.db())
    .await
    .unwrap();
    Fx {
        store_id,
        visit_id,
        visit,
        template_id,
        template,
    }
}

async fn cleanup(app: &TestApp, fx: &Fx) {
    let (t, v, s) = (fx.template_id, fx.visit_id, fx.store_id);
    app.exec(&format!(
        r#"DELETE FROM "storeVisitFormFile" WHERE "submissionId" IN
             (SELECT id FROM "storeVisitFormSubmission" WHERE "templateId" = {t});
           DELETE FROM "storeVisitFormSubmission" WHERE "templateId" = {t};
           DELETE FROM "storeVisitFormTemplateVersion" WHERE "templateId" = {t};
           DELETE FROM "storeVisitFormTemplateUser" WHERE "templateId" = {t};
           DELETE FROM "storeVisitFormTemplate" WHERE id = {t};
           DELETE FROM "storeVisit" WHERE id = {v};
           DELETE FROM store WHERE id = {s};"#
    ))
    .await;
}

fn forms(fx: &Fx) -> String {
    format!("/v1/mobile/store-visits/{}/forms", fx.visit)
}

fn form(fx: &Fx) -> String {
    format!("{}/{}", forms(fx), fx.template)
}

fn code(v: &Value) -> &str {
    v["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn mobile_forms_gates() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;

    // 401 tanpa token.
    app.get(&forms(&fx)).send().await.assert_status(401);
    app.put(&format!("{}/draft", form(&fx)))
        .json(&json!({"answers": {}}))
        .send()
        .await
        .assert_status(401);

    // Paket tanpa FORM → 403; order-choices hanya menuntut ORDER (route-level menimpa kelas).
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET form = false, "order" = true WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let r = app.get(&forms(&fx)).token(&c.staff.token).send().await;
    r.assert_status(403);
    assert_eq!(code(&r.json), "PLAN_FEATURE_DISABLED");
    app.get(&format!("{}/order-choices", forms(&fx)))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(200);
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET form = true, "order" = false WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let r = app
        .get(&format!("{}/order-choices", forms(&fx)))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(code(&r.json), "PLAN_FEATURE_DISABLED");
    common::open_form_gates(&app, c.id).await;

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn mobile_list_and_get_form() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;

    let r = app.get(&forms(&fx)).token(&c.staff.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    let mine = r
        .data()
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == fx.template.as_str())
        .cloned()
        .expect("template uji ada di daftar");
    assert_eq!(mine["status"], "notStarted");
    assert_eq!(mine["version"], 1);
    assert_eq!(mine["submissionId"], Value::Null);
    let keys: Vec<&String> = mine.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "name",
            "description",
            "sortOrder",
            "status",
            "submissionId",
            "submittedAt",
            "versionId",
            "version"
        ]
    );

    // Kunjungan orang lain → 404 (userId ikut di where), apa pun rolenya.
    let r = app.get(&forms(&fx)).token(&c.manager.token).send().await;
    r.assert_status(404);
    assert_eq!(code(&r.json), "VISIT_NOT_FOUND");
    // visitUuid bukan UUID → Prisma P2023 → 500 (tanpa ParseUUIDPipe di Node).
    app.get("/v1/mobile/store-visits/not-a-uuid/forms")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(500);

    // getForm: belum dimulai.
    let r = app.get(&form(&fx)).token(&c.staff.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["status"], "notStarted");
    assert_eq!(r.data()["submission"], Value::Null);
    assert_eq!(r.data()["version"]["version"], 1);
    assert_eq!(r.data()["version"]["schema"]["fields"][0]["key"], "nama");
    assert_eq!(r.data()["template"]["id"], fx.template.as_str());

    // Template tak dikenal → 404; nonaktif tanpa submission → 409; tidak di-assign → 403.
    let r = app
        .get(&format!("{}/{}", forms(&fx), uuid::Uuid::new_v4()))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code(&r.json), "FORM_TEMPLATE_NOT_FOUND");
    app.exec(&format!(
        r#"UPDATE "storeVisitFormTemplate" SET "isActive" = false WHERE id = {}"#,
        fx.template_id
    ))
    .await;
    let r = app.get(&form(&fx)).token(&c.staff.token).send().await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_TEMPLATE_INACTIVE");
    app.exec(&format!(
        r#"UPDATE "storeVisitFormTemplate" SET "isActive" = true, scope = 'targeted' WHERE id = {}"#,
        fx.template_id
    ))
    .await;
    let r = app.get(&form(&fx)).token(&c.staff.token).send().await;
    r.assert_status(403);
    assert_eq!(code(&r.json), "FORM_NOT_ASSIGNED");
    let r = app.get(&forms(&fx)).token(&c.staff.token).send().await;
    assert!(
        !r.data()
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == fx.template.as_str())
    );
    app.exec(&format!(
        r#"INSERT INTO "storeVisitFormTemplateUser" ("templateId", "userId") VALUES ({}, {})"#,
        fx.template_id, c.staff.id
    ))
    .await;
    app.get(&form(&fx))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(200);

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn mobile_draft_and_submit() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;
    let draft = format!("{}/draft", form(&fx));
    let submit = format!("{}/submit", form(&fx));

    // DTO.
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["answers must be an object"]));
    let r = app
        .post(&submit)
        .token(&c.staff.token)
        .json(&json!({"answers": {}}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["clientRequestId must be a UUID"]));

    // Batas atas tetap ditegakkan pada draft.
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "x".repeat(21)}}))
        .send()
        .await;
    r.assert_status(422).assert_no_sql_error();
    assert_eq!(code(&r.json), "FORM_VALIDATION_FAILED");
    assert_eq!(r.json["details"]["errors"][0]["key"], "nama");

    // Draft: required dilewati, nilai tersembunyi disimpan apa adanya.
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({"answers": {"catatan": "tetap"}}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["status"], "draft");
    assert_eq!(
        r.data()["submission"]["answers"],
        json!({"catatan": "tetap"})
    );
    assert_eq!(r.data()["submission"]["submittedAt"], Value::Null);
    let sub_uuid = r.data()["submission"]["id"].as_str().unwrap().to_owned();
    // Update memakai baris yang sama.
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Budi"}}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["submission"]["id"], sub_uuid.as_str());
    let (store_name, rows): (String, i64) = sqlx::query_as(
        r#"SELECT max("storeName"), count(*) FROM "storeVisitFormSubmission" WHERE "templateId" = $1"#,
    )
    .bind(fx.template_id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(rows, 1);
    assert!(store_name.starts_with("Toko IT "));

    // Submit tanpa field wajib → 422.
    let rid = uuid::Uuid::new_v4().to_string();
    let r = app
        .post(&submit)
        .token(&c.staff.token)
        .json(&json!({"answers": {}, "clientRequestId": rid}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code(&r.json), "FORM_VALIDATION_FAILED");

    // Submit sah: nilai tersembunyi dibuang, 200 (bukan 201).
    let r = app
        .post(&submit)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Budi", "catatan": "buang"}, "clientRequestId": rid}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["status"], "submitted");
    assert_eq!(r.data()["submission"]["answers"], json!({"nama": "Budi"}));
    assert!(r.data()["submission"]["submittedAt"].is_i64());
    assert!(!r.text().contains("revision"));

    // Retry idempoten (clientRequestId sama) → 200; berbeda → 409.
    let r = app
        .post(&submit)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Budi"}, "clientRequestId": rid}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["submission"]["id"], sub_uuid.as_str());
    let r = app
        .post(&submit)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Budi"}, "clientRequestId": uuid::Uuid::new_v4()}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_ALREADY_SUBMITTED");
    // Draft sesudah submit → 409.
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Lain"}}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_ALREADY_SUBMITTED");

    // Daftar & detail mencerminkan submit; template nonaktif tetap tampil (cabang UNION).
    app.exec(&format!(
        r#"UPDATE "storeVisitFormTemplate" SET "isActive" = false WHERE id = {}"#,
        fx.template_id
    ))
    .await;
    let r = app.get(&forms(&fx)).token(&c.staff.token).send().await;
    let mine = r
        .data()
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == fx.template.as_str())
        .cloned()
        .unwrap();
    assert_eq!(mine["status"], "submitted");
    assert_eq!(mine["submissionId"], sub_uuid.as_str());
    let r = app.get(&form(&fx)).token(&c.staff.token).send().await;
    r.assert_status(200);
    assert_eq!(r.data()["submission"]["answers"], json!({"nama": "Budi"}));
    assert!(r.data()["submission"].get("answersDisplay").is_none());

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn mobile_write_guards() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;
    let draft = format!("{}/draft", form(&fx));

    // Kunjungan orang lain → 404 (findOwnVisit memfilter userId).
    let r = app
        .put(&draft)
        .token(&c.staff2.token)
        .json(&json!({"answers": {}}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code(&r.json), "VISIT_NOT_FOUND");

    // Kunjungan sudah selesai → 409 FORM_VISIT_NOT_ACTIVE (baca boleh, tulis tidak).
    app.exec(&format!(
        r#"UPDATE "storeVisit" SET state = 'completed' WHERE id = {}"#,
        fx.visit_id
    ))
    .await;
    let r = app
        .put(&draft)
        .token(&c.staff.token)
        .json(&json!({"answers": {}}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_VISIT_NOT_ACTIVE");
    let r = app
        .post(&format!("{}/files", form(&fx)))
        .token(&c.staff.token)
        .json(&json!({"fieldPath": "foto", "contentType": "image/jpeg", "sizeBytes": 10}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_VISIT_NOT_ACTIVE");
    app.get(&form(&fx))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(200);

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn mobile_files_and_signed_proxy() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;
    let files = format!("{}/files", form(&fx));

    // DTO.
    let r = app
        .post(&files)
        .token(&c.staff.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"].as_array().unwrap().len(), 8);
    // Field bukan media → 422 dari files service.
    let r = app
        .post(&files)
        .token(&c.staff.token)
        .json(&json!({"fieldPath": "nama", "contentType": "image/jpeg", "sizeBytes": 10}))
        .send()
        .await;
    assert_eq!(r.status.as_u16() / 100, 4, "{}", r.text());
    r.assert_no_sql_error();

    // Unggah pertama membuat baris draft implisit (201).
    let r = app
        .post(&files)
        .token(&c.staff.token)
        .json(&json!({"fieldPath": "foto", "contentType": "image/jpeg", "sizeBytes": 4}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let file_uuid = r.data()["fileUuid"].as_str().unwrap().to_owned();
    assert_eq!(r.data()["fieldPath"], "foto");
    assert_eq!(r.data()["expiresInSeconds"], 900);
    assert!(
        r.data()["uploadUrl"]
            .as_str()
            .unwrap()
            .contains("X-Goog-Signature")
    );
    let (sub_uuid, key): (String, String) = sqlx::query_as(
        r#"SELECT s.uuid::text, f."objectKey" FROM "storeVisitFormFile" f
             JOIN "storeVisitFormSubmission" s ON s.id = f."submissionId" WHERE f.uuid = $1::uuid"#,
    )
    .bind(&file_uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(key.starts_with(&format!("store-visit-forms/{}/{}/", c.id, fx.visit)));

    // Confirm: objek belum ada → gagal; sesudah PUT klien → 200 terkonfirmasi.
    let confirm = format!("{files}/{file_uuid}/confirm");
    let r = app.post(&confirm).token(&c.staff.token).send().await;
    assert!(r.status.is_client_error(), "{}", r.text());
    em.put(&key, b"\xff\xd8\xff\xd9", "image/jpeg");
    let r = app.post(&confirm).token(&c.staff.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["isConfirmed"], true);
    assert_eq!(r.data()["fileUuid"], file_uuid.as_str());
    assert!(r.data().get("objectKey").is_none());
    let r = app
        .post(&format!("{files}/{}/confirm", uuid::Uuid::new_v4()))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code(&r.json), "FORM_FILE_NOT_FOUND");

    // Draft merujuk file → getForm memberi URL proxy mobile bertanda tangan.
    app.put(&format!("{}/draft", form(&fx)))
        .token(&c.staff.token)
        .json(&json!({"answers": {"nama": "Budi", "foto": {"fileUuid": file_uuid}}}))
        .send()
        .await
        .assert_status(200);
    let r = app.get(&form(&fx)).token(&c.staff.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    let url = r.data()["submission"]["answers"]["foto"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = &url[url.find("/v1/mobile/").unwrap()..];
    assert!(path.starts_with(&format!(
        "/v1/mobile/store-visit-forms/submissions/{sub_uuid}/files/{file_uuid}?exp="
    )));

    // Proxy tanpa JWT: sig sah → 200; rusak / tanpa sig → 404 + header tanpa-autentikasi.
    let r = app.get(path).send().await;
    r.assert_status(200);
    assert_eq!(r.bytes, b"\xff\xd8\xff\xd9");
    let tampered = format!("{}x", path);
    let r = app.get(&tampered).send().await;
    r.assert_status(404);
    assert!(r.headers.contains_key("x-robots-tag"));
    let bare = path.split('?').next().unwrap();
    app.get(bare).send().await.assert_status(404);
    // Kedaluwarsa: exp lampau dengan sig apa pun → 404.
    app.get(&format!("{bare}?exp=1&sig=abc"))
        .send()
        .await
        .assert_status(404);
    // Submission lain pada path bertanda tangan → sig tidak cocok → 404.
    let other = path.replace(&sub_uuid, &uuid::Uuid::new_v4().to_string());
    app.get(&other).send().await.assert_status(404);
    // ParseUUIDPipe pada proxy.
    app.get("/v1/mobile/store-visit-forms/submissions/x/files/y")
        .send()
        .await
        .assert_status(400);

    // Submit merekonsiliasi: file yang tidak dirujuk dihapus sesudah commit.
    let r = app
        .post(&files)
        .token(&c.staff.token)
        .json(&json!({"fieldPath": "foto", "contentType": "image/png", "sizeBytes": 4}))
        .send()
        .await;
    r.assert_status(201);
    let orphan_uuid = r.data()["fileUuid"].as_str().unwrap().to_owned();
    let orphan_key: String = app
        .scalar(&format!(
            r#"SELECT "objectKey" FROM "storeVisitFormFile" WHERE uuid = '{orphan_uuid}'"#
        ))
        .await;
    em.put(&orphan_key, b"png!", "image/png");
    let r = app
        .post(&format!("{}/submit", form(&fx)))
        .token(&c.staff.token)
        .json(
            &json!({"answers": {"nama": "Budi", "foto": {"fileUuid": file_uuid}},
                      "clientRequestId": uuid::Uuid::new_v4()}),
        )
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let left: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "storeVisitFormFile" WHERE uuid = '{orphan_uuid}'"#
        ))
        .await;
    assert_eq!(left, 0);
    assert!(em.deleted().contains(&orphan_key));
    assert!(em.get(&key).is_some());
    // File yang sudah tidak dirujuk tidak bisa diakses lewat proxy (baris hilang).
    // Unggah / confirm sesudah submit → 409.
    let r = app
        .post(&files)
        .token(&c.staff.token)
        .json(&json!({"fieldPath": "foto", "contentType": "image/jpeg", "sizeBytes": 4}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_ALREADY_SUBMITTED");
    let r = app.post(&confirm).token(&c.staff.token).send().await;
    r.assert_status(409);
    assert_eq!(code(&r.json), "FORM_ALREADY_SUBMITTED");

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn mobile_order_choices() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, c.id, c.staff.id).await;
    let base = format!("{}/order-choices", forms(&fx));

    let r = app.get(&base).token(&c.staff.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data().is_array());
    assert_eq!(r.json["meta"], json!({"hasMore": false}));
    let r = app
        .get(&format!("{base}?search=zz-tidak-ada-zz&limit=5"))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json, json!({"data": [], "meta": {"hasMore": false}}));
    let r = app
        .get(&format!("{base}?limit=0"))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["limit must not be less than 1"]));
    // Kunjungan orang lain → 404.
    app.get(&base)
        .token(&c.staff2.token)
        .send()
        .await
        .assert_status(404);

    cleanup(&app, &fx).await;
}
