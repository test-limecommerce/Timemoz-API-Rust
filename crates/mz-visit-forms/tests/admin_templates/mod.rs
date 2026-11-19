//! Uji integrasi route template admin (`/v1/admin/store-visit-forms/{catalog,templates/**}`).

use std::time::Duration;

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

use super::{SERIAL, common, setup};

const BASE: &str = "/v1/admin/store-visit-forms";
const MISSING: &str = "3c9d8e7f-6a5b-4c4d-9e3f-2a1b0c9d8e7f";

fn valid_schema() -> Value {
    json!({
        "schemaVersion": 1,
        "fields": [
            { "key": "catatan", "type": "textarea", "label": "Catatan kunjungan", "required": true },
            { "key": "qty", "type": "number", "label": "Qty", "validation": { "min": 1, "max": 99, "decimals": 0 } }
        ]
    })
}

/// Hapus semua template uji (beserta versi/aset/assignee) milik company fixture.
async fn cleanup(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"DELETE FROM "storeVisitFormTemplateUser" WHERE "templateId" IN
             (SELECT id FROM "storeVisitFormTemplate" WHERE "companyId" = {company_id} AND name LIKE 'IT F1 %');
           DELETE FROM "storeVisitFormTemplateAsset" WHERE "templateId" IN
             (SELECT id FROM "storeVisitFormTemplate" WHERE "companyId" = {company_id} AND name LIKE 'IT F1 %');
           DELETE FROM "storeVisitFormTemplateVersion" WHERE "templateId" IN
             (SELECT id FROM "storeVisitFormTemplate" WHERE "companyId" = {company_id} AND name LIKE 'IT F1 %');
           DELETE FROM "storeVisitFormTemplate" WHERE "companyId" = {company_id} AND name LIKE 'IT F1 %';
           UPDATE "companyPlanOverride" SET "maxFormTemplates" = NULL WHERE "companyId" = {company_id};"#
    ))
    .await;
}

async fn create(app: &TestApp, c: &Company, body: Value) -> Value {
    let r = app
        .post(&format!("{BASE}/templates"))
        .token(&c.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(201);
    r.json["data"].clone()
}

/// Baris audit `storeVisitForm` untuk satu template (ditulis sesudah respons).
async fn audit_actions(
    app: &TestApp,
    template_id: i32,
    at_least: usize,
) -> Vec<(String, Option<String>)> {
    for _ in 0..50 {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT action::text, "subEntity" FROM "auditLog"
                WHERE "entityType"::text = 'storeVisitForm' AND "entityId" = $1 ORDER BY id"#,
        )
        .bind(template_id)
        .fetch_all(app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit template < {at_least}");
}

async fn template_id(app: &TestApp, uuid: &str) -> i32 {
    app.scalar(&format!(
        r#"SELECT id FROM "storeVisitFormTemplate" WHERE uuid = '{uuid}'"#
    ))
    .await
}

#[tokio::test]
async fn templates_gates_and_catalog() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let url = format!("{BASE}/catalog");
    app.get(&url).send().await.assert_status(401);
    let r = app.get(&url).token(&c.staff.token).send().await;
    r.assert_status(401);
    assert_eq!(r.json["message"], "Access denied: insufficient permissions");
    let r = app.get(&url).token(&c.manager.token).send().await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["schemaVersion"], 1);
    assert_eq!(r.json["data"]["limits"]["maxSchemaBytes"], 262_144);

    // ParseUUIDPipe.
    let r = app
        .get(&format!("{BASE}/templates/not-a-uuid"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");
    // 404 template.
    let r = app
        .get(&format!("{BASE}/templates/{MISSING}"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(404);
    assert_eq!(
        r.json,
        json!({ "code": "FORM_TEMPLATE_NOT_FOUND", "message": "Form template not found" })
    );
    // Validasi query & body.
    let r = app
        .get(&format!("{BASE}/templates?page=0&limit=500"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "page must not be less than 1",
            "limit must not be greater than 100"
        ])
    );
    let r = app
        .post(&format!("{BASE}/templates"))
        .token(&c.admin.token)
        .json(&json!({ "name": "", "scope": "x" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "name must be longer than or equal to 1 characters",
            "scope must be one of the following values: all, targeted"
        ])
    );
}

#[tokio::test]
async fn template_crud_versions_publish() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    cleanup(&app, c.id).await;

    let t = create(&app, &c, json!({ "name": "IT F1 Laporan", "sortOrder": 5 })).await;
    let uuid = t["id"].as_str().unwrap().to_owned();
    assert_eq!(t["versionCount"], 1);
    assert_eq!(t["publishedVersion"], Value::Null);
    assert_eq!(t["hasDraftVersion"], true);
    assert_eq!(t["assigneeCount"], 0);
    assert_eq!(t["scope"], "all");
    assert_eq!(t["draftVersion"]["version"], 1);
    assert_eq!(
        t["draftVersion"]["schema"],
        json!({ "schemaVersion": 1, "fields": [] })
    );
    assert_eq!(t["publishedVersionDetail"], Value::Null);
    let tid = template_id(&app, &uuid).await;

    // List + cari.
    let r = app
        .get(&format!("{BASE}/templates?q=it%20f1%20lap&isActive=true"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["meta"]["totalPages"], 1);
    assert_eq!(r.json["data"][0]["id"], uuid.as_str());

    // Patch.
    let r = app
        .patch(&format!("{BASE}/templates/{uuid}"))
        .token(&c.admin.token)
        .json(&json!({ "description": "desk", "scope": "targeted" }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["description"], "desk");
    assert_eq!(r.json["data"]["scope"], "targeted");
    let updated_at = r.json["data"]["updatedAt"].as_i64().unwrap();

    // Assignee: uuid asing → 422, staff → 200.
    let r = app
        .put(&format!("{BASE}/templates/{uuid}/assignees"))
        .token(&c.admin.token)
        .json(&json!({ "userUuids": [MISSING] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(422);
    assert_eq!(r.json["code"], "FORM_ASSIGNEE_INVALID");
    assert_eq!(r.json["details"]["unknownUserIds"], json!([MISSING]));
    let r = app
        .put(&format!("{BASE}/templates/{uuid}/assignees"))
        .token(&c.admin.token)
        .json(&json!({ "userUuids": "x" }))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .put(&format!("{BASE}/templates/{uuid}/assignees"))
        .token(&c.admin.token)
        .json(&json!({ "userUuids": [c.staff.uuid, c.staff.uuid] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["scope"], "targeted");
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["data"][0]["userId"], c.staff.id);
    assert_eq!(r.json["data"][0]["id"], c.staff.uuid.as_str());
    let r = app
        .get(&format!("{BASE}/templates/{uuid}/assignees"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"][0]["userId"], c.staff.id);
    // Ganti ke kosong: baris lama dicabut (soft delete).
    let r = app
        .put(&format!("{BASE}/templates/{uuid}/assignees"))
        .token(&c.admin.token)
        .json(&json!({ "userUuids": [] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"], json!([]));
    let tomb: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "storeVisitFormTemplateUser" WHERE "templateId" = {tid} AND "deletedAt" IS NOT NULL"#
        ))
        .await;
    assert_eq!(tomb, 1);

    // Versi: draft sudah ada → 409.
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/versions"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(409);
    assert_eq!(r.json["code"], "FORM_DRAFT_VERSION_EXISTS");
    assert_eq!(r.json["details"]["version"], 1);
    let version_uuid = r.json["details"]["versionId"].as_str().unwrap().to_owned();
    let vurl = format!("{BASE}/templates/{uuid}/versions/{version_uuid}");

    // Publish schema kosong → 422.
    let r = app
        .post(&format!("{vurl}/publish"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(422);
    assert_eq!(r.json["code"], "FORM_SCHEMA_INVALID");
    assert!(!r.json["details"]["errors"].as_array().unwrap().is_empty());

    // PATCH schema: token basi → 409, validasi → 400, sukses → 200.
    let r = app
        .patch(&vurl)
        .token(&c.admin.token)
        .json(&json!({ "schema": valid_schema(), "expectedTemplateUpdatedAt": updated_at - 1 }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(409);
    assert_eq!(r.json["code"], "FORM_TEMPLATE_STALE");
    assert_eq!(r.json["details"]["currentUpdatedAt"], updated_at);
    let r = app
        .patch(&vurl)
        .token(&c.admin.token)
        .json(&json!({ "schema": [] }))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .patch(&vurl)
        .token(&c.admin.token)
        .json(&json!({ "schema": valid_schema(), "expectedTemplateUpdatedAt": updated_at.to_string() }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["schema"], valid_schema());
    assert!(r.json["data"]["templateUpdatedAt"].as_i64().unwrap() >= updated_at);

    // Validate (dry run): selalu 200.
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/validate"))
        .token(&c.manager.token)
        .json(&json!({ "schema": { "schemaVersion": 1, "fields": [] } }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert!(!r.json["data"]["errors"].as_array().unwrap().is_empty());
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/validate"))
        .token(&c.manager.token)
        .json(&json!({ "schema": valid_schema() }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["errors"], json!([]));
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/validate"))
        .token(&c.manager.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["schema must be an object"]));

    // Publish → 200, ulang → 409.
    let r = app
        .post(&format!("{vurl}/publish"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["isPublished"], true);
    assert_eq!(r.json["data"]["publishedById"], c.admin.id);
    let r = app
        .post(&format!("{vurl}/publish"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({ "code": "FORM_VERSION_LOCKED", "message": "This version has already been published" })
    );
    // Versi terpublish tidak bisa diubah.
    let r = app
        .patch(&vurl)
        .token(&c.admin.token)
        .json(&json!({ "schema": valid_schema(), "expectedTemplateUpdatedAt": 0 }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["message"], "A published version cannot be modified");

    // Versi baru menyalin schema terakhir.
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/versions"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(201);
    assert_eq!(r.json["data"]["version"], 2);
    assert_eq!(r.json["data"]["schema"], valid_schema());
    let r = app
        .get(&format!("{BASE}/templates/{uuid}/versions"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"][0]["version"], 2);
    assert_eq!(r.json["data"][1]["isPublished"], true);
    assert!(r.json["data"][0].get("schema").is_none());
    let r = app.get(&vurl).token(&c.manager.token).send().await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["version"], 1);
    let r = app
        .get(&format!("{BASE}/templates/{uuid}/versions/{MISSING}"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "FORM_VERSION_NOT_FOUND");

    // Detail.
    let r = app
        .get(&format!("{BASE}/templates/{uuid}"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["publishedVersion"], 1);
    assert_eq!(r.json["data"]["hasDraftVersion"], true);
    assert_eq!(r.json["data"]["publishedVersionDetail"]["version"], 1);
    assert_eq!(r.json["data"]["draftVersion"]["version"], 2);

    // Company lain → 404.
    let other = app.other_company_admin().await;
    common::open_form_gates(&app, other.id).await;
    let r = app
        .get(&format!("{BASE}/templates/{uuid}"))
        .token(&other.admin.token)
        .send()
        .await;
    r.assert_status(404);

    // Hapus (soft) → 404 sesudahnya.
    let r = app
        .delete(&format!("{BASE}/templates/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"], json!({ "id": uuid, "deleted": true }));
    let r = app
        .get(&format!("{BASE}/templates/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(404);

    // Audit (entitas `storeVisitForm`): create, update (patch), assignment add/remove,
    // soft delete. Pagar `updatedAt` saja tidak menghasilkan baris (tanpa perubahan bermakna).
    let rows = audit_actions(&app, tid, 5).await;
    assert_eq!(rows[0], ("create".to_owned(), None));
    assert!(
        rows.iter()
            .any(|(_, sub)| sub.as_deref() == Some("assignment"))
    );
    assert!(rows.iter().any(|(a, s)| a == "update" && s.is_none()));

    cleanup(&app, c.id).await;
}

#[tokio::test]
async fn template_plan_limit() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    cleanup(&app, c.id).await;
    let running: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM subscriptions s JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
                WHERE s."companyId" = {} AND s.status::text IN ('ACTIVE', 'TRIAL')"#,
            c.id
        ))
        .await;
    let boosted: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyPlanBoost" WHERE "companyId" = {} AND enabled AND product::text = 'SALES'"#,
            c.id
        ))
        .await;
    let used: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "storeVisitFormTemplate" WHERE "companyId" = {} AND "deletedAt" IS NULL"#,
            c.id
        ))
        .await;
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET "maxFormTemplates" = {} WHERE "companyId" = {}"#,
        used.max(1),
        c.id
    ))
    .await;
    if used == 0 {
        create(&app, &c, json!({ "name": "IT F1 Satu" })).await;
    }
    let r = app
        .post(&format!("{BASE}/templates"))
        .token(&c.admin.token)
        .json(&json!({ "name": "IT F1 Lebih" }))
        .send()
        .await;
    r.assert_no_sql_error();
    eprintln!("plan-limit: running={running} boosted={boosted}");
    if running > 0 && boosted == 0 {
        r.assert_status(402);
        assert_eq!(r.json["code"], "FORM_TEMPLATE_LIMIT_REACHED");
        assert_eq!(
            r.json["message"],
            format!(
                "Your plan allows {} visit forms. Upgrade the plan to add more.",
                used.max(1)
            )
        );
    }
    cleanup(&app, c.id).await;
}

#[tokio::test]
async fn template_pdf_preview_and_assets() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    cleanup(&app, c.id).await;
    let t = create(&app, &c, json!({ "name": "IT F1 \"Kop\" Toko" })).await;
    let uuid = t["id"].as_str().unwrap().to_owned();

    // Pratinjau: schema tidak valid → 422, valid → PDF inline.
    let url = format!("{BASE}/templates/{uuid}/pdf-preview");
    let r = app
        .post(&url)
        .token(&c.manager.token)
        .json(&json!({ "schema": { "schemaVersion": 1, "fields": [] } }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(422);
    assert_eq!(
        r.json["message"],
        "Fix the schema problems before previewing the PDF"
    );
    let r = app
        .post(&url)
        .token(&c.manager.token)
        .json(&json!({ "schema": valid_schema() }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.headers["content-type"], "application/pdf");
    assert_eq!(
        r.headers["content-disposition"],
        "inline; filename=\"it-f1-kop-toko-pratinjau.pdf\""
    );
    assert_eq!(r.headers["cache-control"], "no-store");
    assert!(r.bytes.starts_with(b"%PDF"));

    // Aset: validasi → 400, daftar → 201, confirm sebelum PUT → gagal, sesudah → 200.
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/assets"))
        .token(&c.admin.token)
        .json(&json!({ "contentType": "image/gif", "sizeBytes": 10 }))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/assets"))
        .token(&c.admin.token)
        .json(&json!({ "contentType": "image/png", "sizeBytes": 68 }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(201);
    assert_eq!(r.json["data"]["expiresInSeconds"], 900);
    assert_eq!(r.json["data"]["contentType"], "image/png");
    assert!(
        r.json["data"]["uploadUrl"]
            .as_str()
            .unwrap()
            .contains("X-Goog-Signature")
    );
    let asset = r.json["data"]["assetUuid"].as_str().unwrap().to_owned();
    let aurl = format!("{BASE}/templates/{uuid}/assets/{asset}");
    let r = app
        .post(&format!("{aurl}/confirm"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
    assert!(
        r.status.is_client_error(),
        "confirm tanpa objek: {}",
        r.status
    );

    let key: String = app
        .scalar(&format!(
            r#"SELECT "objectKey" FROM "storeVisitFormTemplateAsset" WHERE uuid = '{asset}'"#
        ))
        .await;
    let png = {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    em.put(&key, &png, "image/png");
    let r = app
        .post(&format!("{aurl}/confirm"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["isConfirmed"], true);
    assert_eq!(r.json["data"]["assetUuid"], asset.as_str());

    // Stream + signed URL.
    let r = app.get(&aurl).token(&c.manager.token).send().await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.bytes, png);
    let r = app
        .get(&format!("{aurl}/url"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.headers["cache-control"], "private, max-age=300");
    assert!(r.json["url"].as_str().is_some());
    assert!(r.json["expiresAt"].is_string() || r.json["expiresAt"].is_number());
    let r = app
        .get(&format!("{BASE}/templates/{uuid}/assets/{MISSING}"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "FORM_ASSET_NOT_FOUND");
    let r = app
        .get(&format!("{BASE}/templates/{uuid}/assets/nope/url"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(400);

    // Kop merujuk aset terkonfirmasi → validate bersih; aset asing → error path.
    let mut schema = valid_schema();
    schema["headers"] =
        json!([{ "key": "logo", "type": "image", "source": "upload", "assetUuid": asset }]);
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/validate"))
        .token(&c.manager.token)
        .json(&json!({ "schema": schema }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"]["errors"], json!([]));
    schema["headers"][0]["assetUuid"] = json!(MISSING);
    let r = app
        .post(&format!("{BASE}/templates/{uuid}/validate"))
        .token(&c.manager.token)
        .json(&json!({ "schema": schema }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"]["errors"],
        json!([{
            "path": "headers[0].assetUuid",
            "message": format!("asset \"{MISSING}\" is not a confirmed upload of this template")
        }])
    );
    // Pratinjau dengan kop berlogo → tetap PDF.
    schema["headers"][0]["assetUuid"] = json!(asset);
    let r = app
        .post(&url)
        .token(&c.manager.token)
        .json(&json!({ "schema": schema }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert!(r.bytes.starts_with(b"%PDF"));

    cleanup(&app, c.id).await;
}
