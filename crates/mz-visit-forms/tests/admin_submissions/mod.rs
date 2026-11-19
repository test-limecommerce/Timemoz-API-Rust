//! Route submission admin (`/v1/admin/store-visit-forms/submissions/**`): fixture template,
//! versi, toko, kunjungan, submission, dan file dibuat lewat SQL lalu dihapus lagi.

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

use super::{SERIAL, common, setup};

const BASE: &str = "/v1/admin/store-visit-forms/submissions";

struct Fx {
    template_id: i32,
    template2_id: i32,
    store_id: i32,
    visit_id: i32,
    visit_uuid: String,
    sub_uuid: String,
    sub_id: i32,
    draft_uuid: String,
    file_uuid: String,
    unreferenced_file_uuid: String,
}

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn one<T>(app: &TestApp, sql: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    sqlx::query_scalar(sql)
        .fetch_one(app.db())
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

async fn seed(app: &TestApp, em: &common::Emulator, c: &Company) -> Fx {
    let cid = c.id;
    let staff = c.staff.id;
    let schema = json!({
        "schemaVersion": 1,
        "fields": [
            {"key": "catatan", "type": "text", "label": "Catatan", "required": true},
            {"key": "qty", "type": "number", "label": "Qty", "validation": {"min": 0}},
            {"key": "ttd", "type": "signature", "label": "Tanda tangan"}
        ]
    });
    let template_id: i32 = one(
        app,
        &format!(
            r#"INSERT INTO "storeVisitFormTemplate" ("companyId", name, "createdById")
               VALUES ({cid}, 'IT Laporan Submission', {staff}) RETURNING id"#
        ),
    )
    .await;
    let version_id: i32 = one(
        app,
        &format!(
            r#"INSERT INTO "storeVisitFormTemplateVersion" ("templateId", version, schema, "publishedAt", "publishedById")
               VALUES ({template_id}, 1, '{schema}'::jsonb, NOW(), {staff}) RETURNING id"#
        ),
    )
    .await;
    let code = format!("ITS-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let store_id: i32 = one(
        app,
        &format!(
            r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById")
               VALUES ({cid}, '{code}', 'Toko IT', 'Pemilik', 'Jl. Uji', -6.2, 106.8, {staff}) RETURNING id"#
        ),
    )
    .await;
    let (visit_id, visit_uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisit" ("companyId", "storeId", "userId", "visitNumber", "clientRequestId",
               "checkInAt", "checkInLat", "checkInLng", "checkInAccuracy", "checkInDistance", "checkInGpsValidation")
           VALUES ({cid}, {store_id}, {staff}, 'V-IT-001', gen_random_uuid(), NOW() - interval '2 hour',
                   -6.2, 106.8, 5, 3, 'valid'::"GpsValidation") RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    let file_uuid = uuid::Uuid::new_v4().to_string();
    let unreferenced_file_uuid = uuid::Uuid::new_v4().to_string();
    let answers = json!({"catatan": "lama", "qty": 3, "ttd": {"fileUuid": file_uuid}});
    let (sub_id, sub_uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisitFormSubmission" ("companyId", "visitId", "visitUuid", "userId", "storeId",
               "storeName", "visitCheckInAt", "templateId", "templateVersionId", answers, "submittedAt", "clientRequestId")
           VALUES ({cid}, {visit_id}, '{visit_uuid}', {staff}, {store_id}, 'Toko IT (saat kunjungan)',
                   NOW() - interval '2 hour', {template_id}, {version_id}, '{answers}'::jsonb, NOW(), gen_random_uuid())
           RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    // Draft pada template lain? `@@unique([visitId, templateId])` → template kedua.
    let template2: i32 = one(
        app,
        &format!(
            r#"INSERT INTO "storeVisitFormTemplate" ("companyId", name, "createdById")
               VALUES ({cid}, 'IT Draft', {staff}) RETURNING id"#
        ),
    )
    .await;
    let draft_uuid: String = one(
        app,
        &format!(
            r#"INSERT INTO "storeVisitFormSubmission" ("companyId", "visitId", "visitUuid", "userId", "storeId",
                   "storeName", "visitCheckInAt", "templateId", "templateVersionId", answers)
               VALUES ({cid}, {visit_id}, '{visit_uuid}', {staff}, {store_id}, 'Toko IT (saat kunjungan)',
                       NOW() - interval '2 hour', {template2}, {version_id}, '{{"catatan":"draft"}}'::jsonb)
               RETURNING uuid::text"#
        ),
    )
    .await;
    for (uuid, confirmed) in [(&file_uuid, true), (&unreferenced_file_uuid, true)] {
        let key = format!("store-visit-forms/{cid}/{visit_uuid}/{uuid}.png");
        app.exec(&format!(
            r#"INSERT INTO "storeVisitFormFile" (uuid, "submissionId", "fieldPath", "objectKey", "contentType", "sizeBytes", "isConfirmed")
               VALUES ('{uuid}', {sub_id}, 'ttd', '{key}', 'image/png', 100, {confirmed})"#
        ))
        .await;
        em.put(&key, &png(), "image/png");
    }
    Fx {
        template_id,
        template2_id: template2,
        store_id,
        visit_id,
        visit_uuid,
        sub_uuid,
        sub_id,
        draft_uuid,
        file_uuid,
        unreferenced_file_uuid,
    }
}

async fn cleanup(app: &TestApp, fx: &Fx) {
    let (sub, visit, store, tpl, tpl2) = (
        fx.sub_id,
        fx.visit_id,
        fx.store_id,
        fx.template_id,
        fx.template2_id,
    );
    app.exec(&format!(
        r#"DELETE FROM "storeVisitFormSubmissionRevision" WHERE "submissionId" = {sub};
           DELETE FROM "storeVisitFormSubmission" WHERE "visitId" = {visit};
           DELETE FROM "storeVisit" WHERE id = {visit};
           DELETE FROM store WHERE id = {store};
           DELETE FROM "storeVisitFormTemplateVersion" WHERE "templateId" IN ({tpl}, {tpl2});
           DELETE FROM "storeVisitFormTemplate" WHERE id IN ({tpl}, {tpl2});"#
    ))
    .await;
}

fn by_visit(fx: &Fx) -> String {
    format!("{BASE}?visitUuid={}", fx.visit_uuid)
}

#[tokio::test]
async fn submissions_gates_and_list() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = seed(&app, &em, &c).await;
    let admin = &c.admin.token;

    app.get(BASE).send().await.assert_status(401);
    let r = app.get(BASE).token(&c.staff.token).send().await;
    r.assert_status(401);
    assert_eq!(r.json["message"], "Access denied: insufficient permissions");

    // Default: hanya yang sudah submit.
    let r = app.get(&by_visit(&fx)).token(admin).send().await;
    r.assert_status(200).assert_no_sql_error();
    let data = r.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    let row = &data[0];
    assert_eq!(row["id"], fx.sub_uuid.as_str());
    assert_eq!(row["status"], "submitted");
    assert_eq!(row["template"]["name"], "IT Laporan Submission");
    assert_eq!(row["version"]["version"], 1);
    assert_eq!(row["user"]["id"], c.staff.uuid.as_str());
    assert_eq!(row["store"]["name"], "Toko IT (saat kunjungan)");
    assert_eq!(row["visit"]["id"], fx.visit_uuid.as_str());
    assert!(row.get("answers").is_none());
    assert_eq!(
        r.json["meta"],
        json!({"total": 1, "page": 1, "limit": 20, "totalPages": 1})
    );

    let r = app
        .get(&format!("{}&status=draft", by_visit(&fx)))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"][0]["id"], fx.draft_uuid.as_str());
    let r = app
        .get(&format!("{}&status=all&limit=1&page=2", by_visit(&fx)))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["meta"],
        json!({"total": 2, "page": 2, "limit": 1, "totalPages": 2})
    );
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);

    // Filter tanggal (timezone company) & uuid asing → nol baris.
    let r = app
        .get(&format!(
            "{}&dateFrom=2000-01-01&dateTo=2000-01-02",
            by_visit(&fx)
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 0);
    let r = app
        .get(&format!(
            "{BASE}?templateUuid={}&userUuid={}&storeUuid={}",
            uuid::Uuid::new_v4(),
            c.staff.uuid,
            uuid::Uuid::new_v4()
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 0);
    let tpl_uuid: String = one(
        &app,
        &format!(
            r#"SELECT uuid::text FROM "storeVisitFormTemplate" WHERE id = {}"#,
            fx.template_id
        ),
    )
    .await;
    let r = app
        .get(&format!(
            "{BASE}?templateUuid={tpl_uuid}&userUuid={}",
            c.staff.uuid
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);

    // Validasi query.
    let r = app
        .get(&format!("{BASE}?status=x&limit=500&dateFrom=1"))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "dateFrom must be YYYY-MM-DD",
            "status must be one of the following values: submitted, draft, all",
            "limit must not be greater than 100"
        ])
    );

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn submission_detail_files_and_revisions() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = seed(&app, &em, &c).await;
    let admin = &c.admin.token;
    let url = format!("{BASE}/{}", fx.sub_uuid);

    let r = app.get(&url).token(admin).send().await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["canRevise"], true);
    assert_eq!(d["schema"]["fields"][0]["key"], "catatan");
    assert_eq!(d["answers"]["catatan"], "lama");
    assert_eq!(
        d["answers"]["ttd"],
        json!({
            "fileUuid": fx.file_uuid,
            "contentType": "image/png",
            "sizeBytes": 100,
            "url": format!("{BASE}/{}/files/{}", fx.sub_uuid, fx.file_uuid),
        })
    );
    assert!(d.get("answersDisplay").is_none());

    app.get(&format!("{BASE}/{}", uuid::Uuid::new_v4()))
        .token(admin)
        .send()
        .await
        .assert_status(404);
    let r = app
        .get(&format!("{BASE}/not-a-uuid"))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");

    // Company lain → 404 (bukan 403).
    let other = app.other_company_admin().await;
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = true WHERE id = {}"#,
        other.id
    ))
    .await;
    common::open_form_gates(&app, other.id).await;
    let r = app.get(&url).token(&other.admin.token).send().await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code": "FORM_SUBMISSION_NOT_FOUND", "message": "Form submission not found"})
    );
    app.get(&format!("{url}/files/{}", fx.file_uuid))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);

    // Proxy file & signed URL.
    let r = app
        .get(&format!("{url}/files/{}", fx.file_uuid))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.bytes, png());
    let r = app
        .get(&format!("{url}/files/{}/url", fx.file_uuid))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.json["url"].as_str().unwrap().contains(&fx.file_uuid));
    assert!(r.json["expiresAt"].is_string());
    assert_eq!(
        r.headers["cache-control"],
        mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL
    );
    // Tidak dirujuk jawaban → 404.
    app.get(&format!("{url}/files/{}", fx.unreferenced_file_uuid))
        .token(admin)
        .send()
        .await
        .assert_status(404);
    app.get(&format!("{url}/files/x/url"))
        .token(admin)
        .send()
        .await
        .assert_status(400);

    // Revisi: kosong lalu satu sesudah koreksi.
    let r = app
        .get(&format!("{url}/revisions"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json, json!({"data": [], "revisionCount": 0}));
    app.get(&format!("{BASE}/{}/revisions", uuid::Uuid::new_v4()))
        .token(admin)
        .send()
        .await
        .assert_status(404);

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn submission_correction() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = seed(&app, &em, &c).await;
    let admin = &c.admin.token;
    let url = format!("{BASE}/{}", fx.sub_uuid);
    let ttd = json!({"fileUuid": fx.file_uuid});
    let body = |answers: Value, rev: i64| json!({"answers": answers, "reason": "salah ketik", "expectedRevision": rev});

    // 401 tanpa token; validasi DTO; uuid rusak didahulukan.
    app.patch(&url)
        .json(&body(json!({}), 0))
        .send()
        .await
        .assert_status(401);
    let r = app.patch(&url).token(admin).json(&json!({})).send().await;
    r.assert_status(400);
    assert_eq!(r.json["message"][0], "answers must be an object");
    let r = app
        .patch(&format!("{BASE}/x"))
        .token(admin)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");

    // Jawaban tidak lolos validasi final → 422.
    let r = app
        .patch(&url)
        .token(admin)
        .json(&body(json!({"qty": -1, "ttd": ttd}), 0))
        .send()
        .await;
    r.assert_status(422).assert_no_sql_error();
    assert_eq!(r.json["code"], "FORM_VALIDATION_FAILED");

    // Media immutable.
    let r = app
        .patch(&url)
        .token(admin)
        .json(&body(json!({"catatan": "baru"}), 0))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "FORM_MEDIA_IMMUTABLE");
    assert_eq!(
        r.json["details"],
        json!({"expected": [fx.file_uuid], "received": []})
    );

    // Sukses.
    let r = app
        .patch(&url)
        .token(admin)
        .json(&body(json!({"catatan": "baru", "qty": 4, "ttd": ttd}), 0))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["revisionCount"], 1);
    assert_eq!(d["canRevise"], true);
    assert_eq!(d["answers"]["catatan"], "baru");
    assert_eq!(d["lastEditedBy"]["id"], c.admin.uuid.as_str());
    assert!(d["lastEditedAt"].is_i64());

    // expectedRevision basi → 409 tanpa baris revisi baru.
    let r = app
        .patch(&url)
        .token(admin)
        .json(&body(json!({"catatan": "lagi", "ttd": ttd}), 0))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({
            "code": "FORM_REVISION_CONFLICT",
            "message": "This submission was corrected by someone else; reload before correcting again",
            "details": {"expectedRevision": 0, "currentRevision": 1}
        })
    );

    // Riwayat revisi: before-image.
    let r = app
        .get(&format!("{url}/revisions"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["revisionCount"], 1);
    let rev = &r.json["data"][0];
    assert_eq!(rev["revision"], 1);
    assert_eq!(rev["reason"], "salah ketik");
    assert_eq!(rev["answersBefore"]["catatan"], "lama");
    assert_eq!(rev["editedBy"]["id"], c.admin.uuid.as_str());

    // Draft → 409 FORM_NOT_SUBMITTED.
    let r = app
        .patch(&format!("{BASE}/{}", fx.draft_uuid))
        .token(admin)
        .json(&body(json!({"catatan": "x"}), 0))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "FORM_NOT_SUBMITTED");

    // Mode `selected` tanpa terdaftar → MANAGER ditolak 403 (ADMIN tetap lolos lewat peran?).
    app.exec(&format!(
        r#"UPDATE company SET "storeVisitFormReviseMode" = 'selected' WHERE id = {}"#,
        c.id
    ))
    .await;
    let r = app
        .patch(&url)
        .token(&c.manager.token)
        .json(&body(json!({"catatan": "m", "ttd": ttd}), 1))
        .send()
        .await;
    app.exec(&format!(
        r#"UPDATE company SET "storeVisitFormReviseMode" = 'allPrivileged' WHERE id = {}"#,
        c.id
    ))
    .await;
    r.assert_status(403).assert_no_sql_error();
    assert_eq!(r.json["code"], "FORM_REVISE_FORBIDDEN");

    app.patch(&format!("{BASE}/{}", uuid::Uuid::new_v4()))
        .token(admin)
        .json(&body(json!({}), 0))
        .send()
        .await
        .assert_status(404);

    cleanup(&app, &fx).await;
}

#[tokio::test]
async fn submission_pdf() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = seed(&app, &em, &c).await;
    let admin = &c.admin.token;

    app.get(&format!("{BASE}/{}/pdf", fx.sub_uuid))
        .send()
        .await
        .assert_status(401);
    let r = app
        .get(&format!("{BASE}/{}/pdf", fx.sub_uuid))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.headers["content-type"], "application/pdf");
    assert_eq!(
        r.headers["content-disposition"],
        "attachment; filename=\"it-laporan-submission-v-it-001.pdf\""
    );
    assert_eq!(
        r.headers["content-length"],
        r.bytes.len().to_string().as_str()
    );
    assert!(r.bytes.starts_with(b"%PDF"));

    // Draft juga bisa dicetak.
    let r = app
        .get(&format!("{BASE}/{}/pdf", fx.draft_uuid))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.starts_with(b"%PDF"));

    let r = app
        .get(&format!("{BASE}/{}/pdf", uuid::Uuid::new_v4()))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "FORM_SUBMISSION_NOT_FOUND");

    cleanup(&app, &fx).await;
}
