//! Import produk (§9): upload multipart → baris job + objek GCS (emulator) + job antrean, proses
//! job (langsung, bukan menunggu worker), dryRun, merge re-import, gambar dari server HTTP tiruan
//! lokal, list/detail/template dan jalur tolak.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use calamine::{Data, Reader, Xlsx};
use mz_products::import::safe_fetch::allow_loopback_only;
use mz_products::import::{columns::PRODUCT_IMPORT_QUEUE, process_job_with};
use rust_xlsxwriter::Workbook;
use serde_json::{Value, json};

use crate::common::*;

const BOUNDARY: &str = "----mzproductsboundary";

fn xlsx(headers: &[&str], rows: &[Vec<String>]) -> Vec<u8> {
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("Products").unwrap();
    for (c, h) in headers.iter().enumerate() {
        ws.write_string(0, c as u16, *h).unwrap();
    }
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                ws.write_string(r as u32 + 1, c as u16, v).unwrap();
            }
        }
    }
    wb.save_to_buffer().unwrap()
}

fn multipart(field: &str, file_name: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(
    c: &Ctx,
    query: &str,
    file_name: &str,
    data: &[u8],
    token: &str,
) -> mz_testkit::TestResponse {
    c.app
        .post(&format!("/v1/admin/products/import{query}"))
        .token(token)
        .raw(
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart("file", file_name, data),
        )
        .send()
        .await
}

/// Server gambar tiruan di loopback (`/ok.png` = PNG, lainnya 404).
async fn image_server() -> String {
    let app = axum::Router::new().fallback(|uri: axum::http::Uri| async move {
        if uri.path() == "/ok.png" {
            (
                StatusCode::OK,
                [("content-type", "image/png")],
                b"png-bytes".to_vec(),
            )
                .into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Upload 202 → id job; antrean berisi job, lalu job dikeluarkan dari antrean (diproses langsung).
async fn upload_ok(c: &Ctx, query: &str, data: &[u8]) -> (String, i32) {
    let r = upload(c, query, "products.xlsx", data, &c.fx.admin.token).await;
    r.assert_status(202);
    assert_eq!(r.data()["status"], "queued");
    let uuid = r.data()["id"].as_str().unwrap().to_owned();
    let id: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT id FROM "productImportJob" WHERE uuid = '{uuid}'"#
        ))
        .await;
    let state = c.app.state();
    let job_key = format!("product-import:{uuid}");
    let stored = mz_core::queue::get(state, PRODUCT_IMPORT_QUEUE, &job_key)
        .await
        .unwrap();
    assert_eq!(stored.map(|s| s.data), Some(json!({ "jobId": id })));
    mz_core::queue::ack(state, PRODUCT_IMPORT_QUEUE, &job_key)
        .await
        .unwrap();
    (uuid, id)
}

async fn detail(c: &Ctx, uuid: &str) -> Value {
    let r = c
        .app
        .get(&format!("/v1/admin/products/imports/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    r.data().clone()
}

const HEADERS: &[&str] = &[
    "sku",
    "name",
    "categoryCode",
    "baseUomCode",
    "uom2Code",
    "uom2Factor",
    "barcode",
    "imageUrl",
];

fn row(vals: &[&str]) -> Vec<String> {
    vals.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn import_proses_merge_dan_gambar() {
    let Some(c) = setup().await else { return };
    let t = tag();
    let cat = format!("IC{t}");
    new_master(
        &c,
        "categories",
        json!({ "code": cat, "name": "Import cat" }),
    )
    .await;
    let img = image_server().await;
    let sku = format!("IMP-{t}");
    let barcode = format!("899{t}");
    let data = xlsx(
        HEADERS,
        &[
            row(&[
                &sku,
                "Produk Import",
                &cat,
                "PCS",
                "BOX",
                "12",
                &barcode,
                &format!("{img}/ok.png"),
            ]),
            row(&[
                &format!("IMP2-{t}"),
                "Gagal",
                "NOPE-CAT",
                "PCS",
                "",
                "",
                "",
                "",
            ]),
            row(&[
                &format!("IMP3-{t}"),
                "Gambar hilang",
                &cat,
                "PCS",
                "",
                "",
                "",
                &format!("{img}/missing.png"),
            ]),
        ],
    );
    let (uuid, id) = upload_ok(&c, "", &data).await;
    let job = detail(&c, &uuid).await;
    assert_key_order(
        &job.to_string(),
        &[
            "id",
            "fileName",
            "dryRun",
            "status",
            "totalRows",
            "processed",
            "succeeded",
            "failed",
            "errors",
            "warnings",
            "startedAt",
            "finishedAt",
            "createdAt",
        ],
    );
    assert_eq!(job["status"], "queued");
    assert_eq!(job["fileName"], "products.xlsx");
    let key: String = c
        .app
        .scalar(&format!(
            r#"SELECT "objectKey" FROM "productImportJob" WHERE id = {id}"#
        ))
        .await;
    assert!(key.starts_with(&format!("product-imports/{}/", c.fx.id)) && key.ends_with(".xlsx"));
    assert!(c.em.get(&key).is_some());

    process_job_with(c.app.state(), id, allow_loopback_only)
        .await
        .unwrap();
    let job = detail(&c, &uuid).await;
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(job["totalRows"], 3);
    assert_eq!(job["succeeded"], 2);
    assert_eq!(job["failed"], 1);
    assert_eq!(job["processed"], 3);
    assert_eq!(job["errors"][0]["row"], 3);
    assert_eq!(job["errors"][0]["column"], "categoryCode");
    assert_eq!(job["errors"][0]["code"], "PRODUCT_MASTER_NOT_FOUND");
    assert_eq!(job["warnings"].as_array().unwrap().len(), 1, "{job}");
    assert_eq!(job["warnings"][0]["row"], 4);
    assert_eq!(job["warnings"][0]["code"], "IMAGE_DOWNLOAD_FAILED");
    assert!(job["startedAt"].is_i64() && job["finishedAt"].is_i64());

    let (pid, image_key): (i32, Option<String>) = sqlx::query_as(&format!(
        r#"SELECT id, "imageObjectKey" FROM product WHERE "companyId" = {} AND sku = '{sku}' AND "deletedAt" IS NULL"#,
        c.fx.id
    ))
    .fetch_one(c.app.db())
    .await
    .unwrap();
    let image_key = image_key.expect("gambar tersimpan");
    assert_eq!(c.em.get(&image_key).as_deref(), Some(&b"png-bytes"[..]));
    let uoms: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "productUom" WHERE "productId" = {pid}"#
        ))
        .await;
    assert_eq!(uoms, 2);
    let box_rate: String = c
        .app
        .scalar(&format!(
            r#"SELECT pu."conversionRate"::text FROM "productUom" pu JOIN uom u ON u.id = pu."uomId"
               WHERE pu."productId" = {pid} AND u.code = 'BOX'"#
        ))
        .await;
    assert_eq!(box_rate, "12.000000");

    // Re-import: barcode kosong mempertahankan barcode lama, nama diperbarui, UOM tidak dihapus.
    let data = xlsx(
        HEADERS,
        &[row(&[&sku, "Nama Baru", &cat, "PCS", "", "", "", ""])],
    );
    let (uuid2, id2) = upload_ok(&c, "", &data).await;
    process_job_with(c.app.state(), id2, allow_loopback_only)
        .await
        .unwrap();
    let job = detail(&c, &uuid2).await;
    assert_eq!(job["succeeded"], 1, "{job}");
    let (name, barcodes, uoms): (String, i64, i64) = sqlx::query_as(&format!(
        r#"SELECT name, (SELECT COUNT(*) FROM "productBarcode" WHERE "productId" = {pid}),
                  (SELECT COUNT(*) FROM "productUom" WHERE "productId" = {pid})
           FROM product WHERE id = {pid}"#
    ))
    .fetch_one(c.app.db())
    .await
    .unwrap();
    assert_eq!((name.as_str(), barcodes, uoms), ("Nama Baru", 1, 2));
    // Audit: createMany produk tercatat (aktor `system`, job tanpa konteks request). Model
    // `product` tidak punya kebijakan kolom di Node → update tanpa kolom berubah (dibuang).
    let creates: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'product'
               AND action::text = 'create' AND "actorType"::text = 'system' AND "companyId" = {}"#,
            c.fx.id
        ))
        .await;
    assert!(creates >= 1, "audit createMany produk tercatat");

    // List riwayat.
    let r = c
        .app
        .get("/v1/admin/products/imports?page=1&limit=2")
        .token(&c.fx.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()[0]["id"], uuid2.as_str());
    assert_eq!(r.json["meta"]["limit"], 2);
    assert!(r.json["meta"]["total"].as_i64().unwrap() >= 2);
}

#[tokio::test]
async fn dry_run_tidak_menulis_dan_file_rusak_gagal() {
    let Some(c) = setup().await else { return };
    let t = tag();
    let cat = format!("DC{t}");
    new_master(&c, "categories", json!({ "code": cat, "name": "Dry cat" })).await;
    let sku = format!("DRY-{t}");
    let data = xlsx(HEADERS, &[row(&[&sku, "Dry", &cat, "PCS", "", "", "", ""])]);
    let (uuid, id) = upload_ok(&c, "?dryRun=true", &data).await;
    assert_eq!(detail(&c, &uuid).await["dryRun"], true);
    process_job_with(c.app.state(), id, allow_loopback_only)
        .await
        .unwrap();
    let job = detail(&c, &uuid).await;
    assert_eq!(
        (job["status"].as_str(), job["succeeded"].as_i64()),
        (Some("completed"), Some(1))
    );
    let n: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM product WHERE sku = '{sku}'"#
        ))
        .await;
    assert_eq!(n, 0, "dryRun di-rollback");

    // Nama .xlsx tapi isi bukan workbook → job failed dengan kode parser.
    let (uuid, id) = upload_ok(&c, "", b"not an xlsx file").await;
    process_job_with(c.app.state(), id, allow_loopback_only)
        .await
        .unwrap();
    let job = detail(&c, &uuid).await;
    assert_eq!(job["status"], "failed");
    assert_eq!(job["errors"][0]["row"], 0);
    assert_eq!(job["errors"][0]["column"], Value::Null);
    assert_eq!(job["errors"][0]["code"], "IMPORT_INVALID_FILE");

    // Header wajib hilang.
    let data = xlsx(&["sku", "name"], &[row(&["A", "B"])]);
    let (uuid, id) = upload_ok(&c, "", &data).await;
    process_job_with(c.app.state(), id, allow_loopback_only)
        .await
        .unwrap();
    let job = detail(&c, &uuid).await;
    assert_eq!(
        job["errors"][0]["message"],
        "Missing required column header(s): categoryCode, baseUomCode"
    );
}

#[tokio::test]
async fn template_dan_jalur_tolak() {
    let Some(c) = setup().await else { return };
    let key = format!("imp_cf_{}", tag().to_lowercase());
    c.app
        .exec(&format!(
            r#"INSERT INTO "productCustomField" ("companyId", entity, key, label, type, "createdById", "sortOrder")
               VALUES ({}, 'product', '{key}', 'Imp', 'text', {}, 0)"#,
            c.fx.id, c.fx.admin.id
        ))
        .await;
    let r = c
        .app
        .get("/v1/admin/products/import/template")
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(
        r.headers["content-disposition"],
        "attachment; filename=\"product-import-template.xlsx\""
    );
    assert_eq!(
        r.headers["content-length"],
        r.bytes.len().to_string().as_str()
    );
    let mut wb: Xlsx<_> = Xlsx::new(std::io::Cursor::new(r.bytes.clone())).unwrap();
    let range = wb.worksheet_range_at(0).unwrap().unwrap();
    let header: Vec<String> = range
        .rows()
        .next()
        .unwrap()
        .iter()
        .map(|d| match d {
            Data::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(header.first().map(String::as_str), Some("sku"));
    assert!(header.contains(&format!("cf:{key}")));

    // Tanpa file (bukan multipart) → 422.
    let r = c
        .app
        .post("/v1/admin/products/import")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({ "code": "IMPORT_INVALID_FILE", "message": "No file uploaded" })
    );
    // Ekstensi salah.
    let r = upload(&c, "", "products.csv", b"a,b", &c.fx.admin.token).await;
    r.assert_status(422);
    assert_eq!(r.json["message"], "File must have a .xlsx extension");
    // Field selain `file`.
    let r = c
        .app
        .post("/v1/admin/products/import")
        .token(&c.fx.admin.token)
        .raw(
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart("upload", "x.xlsx", b"x"),
        )
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["message"], "Unexpected field");
    // > 10 MB → 413.
    let big = vec![b'x'; 10 * 1024 * 1024 + 10];
    let r = upload(&c, "", "big.xlsx", &big, &c.fx.admin.token).await;
    r.assert_status(413);
    assert_eq!(
        r.json,
        json!({ "code": "IMPORT_FILE_TOO_LARGE", "message": "File exceeds the 10 MB limit" })
    );
    // dryRun bukan boolean → 400.
    let r = upload(&c, "?dryRun=maybe", "p.xlsx", b"x", &c.fx.admin.token).await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["dryRun must be a boolean value"]);
    // Role STAFF ditolak RolesGuard (401 "Access denied: insufficient permissions", seperti Node).
    let r = upload(&c, "", "p.xlsx", b"x", &c.fx.staff.token).await;
    r.assert_status(401);
    let r = c
        .app
        .get("/v1/admin/products/imports")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(401);
    // Detail tidak dikenal → 404; bukan uuid → 500 (P2023 Node).
    let r = c
        .app
        .get(&format!(
            "/v1/admin/products/imports/{}",
            uuid::Uuid::new_v4()
        ))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "PRODUCT_IMPORT_NOT_FOUND");
    let r = c
        .app
        .get("/v1/admin/products/imports?limit=0")
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["limit must not be less than 1"]);
    // Tanpa token → 401.
    c.app
        .get("/v1/admin/products/imports")
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn worker_antrean_memproses_job() {
    let Some(c) = setup().await else { return };
    let t = tag();
    let cat = format!("WC{t}");
    new_master(
        &c,
        "categories",
        json!({ "code": cat, "name": "Worker cat" }),
    )
    .await;
    let sku = format!("WRK-{t}");
    let data = xlsx(
        HEADERS,
        &[row(&[&sku, "Worker", &cat, "PCS", "", "", "", ""])],
    );
    let worker = mz_products::spawn_worker(c.app.state());
    let r = upload(&c, "", "w.xlsx", &data, &c.fx.admin.token).await;
    r.assert_status(202);
    let uuid = r.data()["id"].as_str().unwrap().to_owned();
    let mut status = String::new();
    for _ in 0..100 {
        status = detail(&c, &uuid).await["status"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if status == "completed" || status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    worker.abort();
    assert_eq!(status, "completed");
    let n: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM product WHERE sku = '{sku}' AND "deletedAt" IS NULL"#
        ))
        .await;
    assert_eq!(n, 1);
}
