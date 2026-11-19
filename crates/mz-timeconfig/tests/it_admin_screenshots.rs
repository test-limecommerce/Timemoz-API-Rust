//! Uji integrasi `POST /admin/screenshot/compress|purge` dan `DELETE|POST /admin/screenshot/queue*`
//! (operator) terhadap Postgres + Redis nyata. Job masuk antrean persisten Redis
//! (`mz:q:screenshot:*`); worker hanya dipasang di uji alur jeda/lanjut, dengan baris tanpa
//! `fileFullpath` sehingga GCS tidak pernah dihubungi. Yang diuji: gerbang operator, validasi,
//! pemilihan screenshot, respons, antrean, `operatorAuditLog`.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-timeconfig --test it_admin_screenshots`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_core::queue;
use mz_screenshots::jobs::QUEUE;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_timeconfig::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn operator(app: &TestApp, status: i32) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status)
               VALUES ('op{u}', 'x', 'IT Operator', {status}) RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

/// company (retensi `days`) + project + timeline milik staff fixture; mengembalikan
/// (project, timeline).
async fn project_with_retention(app: &TestApp, staff: i32, days: i32) -> (i32, i32) {
    let u = uniq();
    let c: i32 = app
        .scalar(&format!(
            r#"INSERT INTO company (name, status, "screenshotRetentionDays")
               VALUES ('it-adm-shot-{u}', 1, {days}) RETURNING id"#
        ))
        .await;
    let p: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
               VALUES ('it{u}', 'it{u}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), CURRENT_DATE, 1)
               RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({p}, {c}, 1)"#
    ))
    .await;
    let t: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES (CURRENT_DATE, {staff}, 1)
               ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1 RETURNING id"#
        ))
        .await;
    let tl: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
               VALUES ('it', NOW(), NOW(), {t}, {p}, 1) RETURNING id"#
        ))
        .await;
    (p, tl)
}

/// Screenshot dengan `createdAt` = ekspresi SQL; `fileFullpath` kosong (job latar tidak
/// menyentuh GCS untuk key kosong).
async fn shot(app: &TestApp, p: i32, tl: i32, created: &str) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO screenshot ("fileFullpath", "takenAt", "trackingTimelineId", "projectId", status, "createdAt")
           VALUES ('', NOW(), {tl}, {p}, 1, {created}) RETURNING id"#
    ))
    .await
}

/// (action, targetType, targetUuid, after, reason).
type AuditRow = (
    String,
    String,
    Option<String>,
    Option<Value>,
    Option<String>,
);

async fn audit_rows(app: &TestApp, op: i32) -> Vec<Value> {
    let rows: Vec<AuditRow> = sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, after, reason
             FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(a, t, u, after, r)| json!([a, t, u, after, r]))
        .collect()
}

#[tokio::test]
async fn operator_gate_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    // Tanpa token / token user / operator nonaktif → 401 polos (guard sebelum pipe).
    for path in ["/admin/screenshot/compress", "/admin/screenshot/purge"] {
        app.post(path).send().await.assert_status(401);
        app.post(path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
        app.post(path)
            .token(&inactive)
            .send()
            .await
            .assert_status(401);
    }

    // Query: pipe global Node (tanpa whitelist) melempar lebih dulu, jadi "property foo should
    // not exist" tidak muncul selama field lain tidak valid (`mz_core::error::global_pipe_first`).
    let r = app
        .post("/admin/screenshot/compress?foo=x&daysOld=abc")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({
            "message": [
                "daysOld must not be greater than 3650",
                "daysOld must not be less than 1",
                "daysOld must be an integer number"
            ],
            "error": "Bad Request",
            "statusCode": 400
        })
    );

    // Purge: reason wajib (Length lalu IsString); body asing ditolak.
    let r = app
        .post("/admin/screenshot/purge")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "reason must be longer than or equal to 5 characters",
            "reason must be a string"
        ])
    );
    let r = app
        .post("/admin/screenshot/purge?daysOld=0")
        .token(&token)
        .json(&json!({ "reason": "valid reason", "x": 1 }))
        .send()
        .await;
    r.assert_status(400);
    // Query diperiksa lebih dulu (param @Query sebelum @Body).
    assert_eq!(
        r.json["message"],
        json!(["daysOld must not be less than 1"])
    );

    // JSON rusak → 400 body-parser.
    app.post("/admin/screenshot/compress")
        .token(&token)
        .raw("application/json", "{bad")
        .send()
        .await
        .assert_status(400);

    // Gagal validasi tidak menulis audit.
    assert!(audit_rows(&app, op).await.is_empty());
}

#[tokio::test]
async fn compress_and_purge_select_and_audit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;

    // daysOld 3650: jendela compress = hari WIB 3650 hari lalu; dump tidak punya data setua itu.
    let (p, tl) = project_with_retention(&app, c.staff.id, 30).await;
    let wib_day_start = "((date_trunc('day', NOW() AT TIME ZONE 'UTC' + interval '7 hours') - interval '7 hours') - interval '3650 days')";
    let in_window = shot(&app, p, tl, &format!("{wib_day_start} + interval '1 hour'")).await;
    let _before = shot(&app, p, tl, &format!("{wib_day_start} - interval '1 hour'")).await;
    let _after = shot(
        &app,
        p,
        tl,
        &format!("{wib_day_start} + interval '25 hours'"),
    )
    .await;
    let compressed = shot(
        &app,
        p,
        tl,
        &format!("{wib_day_start} + interval '2 hours'"),
    )
    .await;
    app.exec(&format!(
        r#"UPDATE screenshot SET "isCompressed" = true WHERE id = {compressed}"#
    ))
    .await;
    let r = app
        .post("/admin/screenshot/compress?daysOld=3650")
        .token(&token)
        .json(&json!({ "note": "uji", "password": "rahasia" }))
        .send()
        .await;
    r.assert_status(200);
    let raw = r.text();
    assert_eq!(
        raw,
        r#"{"success":true,"message":"Enqueued 1 screenshots for compression","count":1,"daysOld":3650}"#,
        "id terpilih {in_window}"
    );
    // Job masuk antrean Redis persisten (bukan dijalankan in-process): id deterministik + payload.
    let stored = queue::get(
        app.state(),
        QUEUE,
        &format!("screenshot.compress:{in_window}"),
    )
    .await
    .unwrap()
    .expect("job compress ada di antrean");
    assert_eq!(
        stored.data,
        json!({ "kind": "compress", "screenshotId": in_window })
    );
    assert_eq!(stored.max_attempts, 1);

    // Hari WIB berikutnya (3649) memuat baris "+25 jam" saja: batas jendela [awal, awal+1 hari).
    let r = app
        .post("/admin/screenshot/compress?daysOld=3649")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Enqueued 1 screenshots for compression","count":1,"daysOld":3649}"#
    );
    // Tidak ada kandidat → tanpa `daysOld`.
    let r = app
        .post("/admin/screenshot/compress?daysOld=3648")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"No screenshots found older than 3648 days to compress","count":0}"#
    );

    // Purge 3650: createdAt < now − 3650 hari DAN retensi company <= 3650 (tanpa filter status).
    let (p2, tl2) = project_with_retention(&app, c.staff.id, 3650).await;
    let _old = shot(&app, p2, tl2, "NOW() - interval '3651 days'").await;
    let (p3, tl3) = project_with_retention(&app, c.staff.id, 4000).await;
    let _old_long_retention = shot(&app, p3, tl3, "NOW() - interval '3651 days'").await;
    // Baris compress di atas (retensi 30, createdAt ~3650 hari lalu) ikut terpilih bila lebih
    // tua dari cutoff; harapan dihitung dari DB tepat sebelum request. Retensi 4000 tidak.
    let expected: i64 = app
        .scalar(
            r#"SELECT count(*) FROM screenshot s WHERE s."createdAt" < NOW() AT TIME ZONE 'UTC' - interval '3650 days'
                 AND EXISTS (SELECT 1 FROM "projectCompany" pc JOIN company c ON c.id = pc."companyId"
                              WHERE pc."projectId" = s."projectId" AND c."screenshotRetentionDays" <= 3650)"#,
        )
        .await;
    assert!(expected >= 1);
    let r = app
        .post("/admin/screenshot/purge?daysOld=3650")
        .token(&token)
        .json(&json!({ "reason": "  bersihkan uji  " }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        format!(
            r#"{{"success":true,"message":"Enqueued {expected} screenshots for purge","count":{expected},"daysOld":3650}}"#
        )
    );

    // Audit: satu baris per aksi sukses; `after` = body mentah tanpa rahasia; reason mentah.
    assert_eq!(
        audit_rows(&app, op).await,
        vec![
            json!(["screenshot.compress", "screenshot", null, { "note": "uji" }, null]),
            json!(["screenshot.compress", "screenshot", null, {}, null]),
            json!(["screenshot.compress", "screenshot", null, {}, null]),
            json!([
                "screenshot.purge",
                "screenshot",
                null,
                { "reason": "  bersihkan uji  " },
                "  bersihkan uji  "
            ]),
        ]
    );
}

// ── DELETE /admin/screenshot/queue, POST .../queue/pause|resume ─────────────

const QUEUE_ROUTES: [(&str, &str); 3] = [
    ("DELETE", "/admin/screenshot/queue"),
    ("POST", "/admin/screenshot/queue/pause"),
    ("POST", "/admin/screenshot/queue/resume"),
];

fn req<'a>(app: &'a TestApp, method: &str, path: &str) -> mz_testkit::RequestBuilder<'a> {
    match method {
        "DELETE" => app.delete(path),
        _ => app.post(path),
    }
}

/// Kosongkan antrean + penanda jeda/limiter (Redis DB uji dipakai bersama).
async fn reset_queue(app: &TestApp) {
    queue::obliterate(app.state(), QUEUE).await.unwrap();
}

#[tokio::test]
async fn queue_routes_gate_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    reset_queue(&app).await;
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    // Guard lebih dulu dari pipe: tanpa token / token user / operator nonaktif -> 401 polos.
    for (m, path) in QUEUE_ROUTES {
        req(&app, m, path).send().await.assert_status(401);
        req(&app, m, path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
        req(&app, m, path)
            .token(&inactive)
            .send()
            .await
            .assert_status(401);
    }
    assert!(!queue::is_paused(app.state(), QUEUE).await.unwrap());

    // DELETE: ReasonDto wajib (Length lalu IsString), properti asing ditolak; query diabaikan.
    let r = app
        .delete("/admin/screenshot/queue")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({
            "message": [
                "reason must be longer than or equal to 5 characters",
                "reason must be a string"
            ],
            "error": "Bad Request",
            "statusCode": 400
        })
    );
    let r = app
        .delete("/admin/screenshot/queue?foo=1")
        .token(&token)
        .json(&json!({ "reason": "valid reason", "x": 1 }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], json!(["property x should not exist"]));
    app.delete("/admin/screenshot/queue")
        .token(&token)
        .json(&json!({ "reason": "abc" }))
        .send()
        .await
        .assert_status(400);

    // JSON rusak -> 400 body-parser (semua route).
    for (m, path) in QUEUE_ROUTES {
        req(&app, m, path)
            .token(&token)
            .raw("application/json", "{bad")
            .send()
            .await
            .assert_status(400);
    }
    // Pause/resume tanpa DTO: body apa pun diterima (tidak ada whitelist).
    app.post("/admin/screenshot/queue/resume")
        .token(&token)
        .json(&json!({ "apa": "saja" }))
        .send()
        .await
        .assert_status(200);

    // Hanya resume sukses yang mencatat audit; kegagalan guard/validasi tidak.
    assert_eq!(
        audit_rows(&app, op).await,
        vec![json!([
            "screenshot.queue_resume",
            "screenshot",
            null,
            { "apa": "saja" },
            null
        ])]
    );
    assert!(!queue::is_paused(app.state(), QUEUE).await.unwrap());
}

/// Baris DELETED lama tanpa `fileFullpath`: purge-nya tidak butuh GCS (baris DB dihapus).
async fn deleted_row(app: &TestApp, p: i32, tl: i32) -> i32 {
    let id = shot(app, p, tl, "NOW() - interval '100 days'").await;
    app.exec(&format!("UPDATE screenshot SET status = 0 WHERE id = {id}"))
        .await;
    id
}

async fn row_exists(app: &TestApp, id: i32) -> bool {
    let n: i64 = app
        .scalar(&format!("SELECT count(*) FROM screenshot WHERE id = {id}"))
        .await;
    n > 0
}

#[tokio::test]
async fn pause_enqueue_not_processed_resume_processed_and_clear() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    reset_queue(&app).await;
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (p, tl) = project_with_retention(&app, c.staff.id, 30).await;
    let d1 = deleted_row(&app, p, tl).await;
    let d2 = deleted_row(&app, p, tl).await;
    let st = app.state();

    // Pause: respons persis, status tersimpan di Redis.
    let r = app
        .post("/admin/screenshot/queue/pause")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Screenshot queue paused"}"#
    );
    assert!(queue::is_paused(st, QUEUE).await.unwrap());

    // Worker jalan, job masuk, tetapi tidak diproses selama dijeda.
    let worker = mz_screenshots::jobs::spawn_worker(st);
    assert_eq!(
        mz_screenshots::jobs::enqueue_purge(st, &[d1])
            .await
            .unwrap(),
        1
    );
    // Id sama tidak membuat job ganda.
    assert_eq!(
        mz_screenshots::jobs::enqueue_purge(st, &[d1])
            .await
            .unwrap(),
        0
    );
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(row_exists(&app, d1).await);
    assert_eq!(queue::waiting_len(st, QUEUE).await.unwrap(), 1);

    // Clear: hapus job menunggu (waiting 1, failed selalu 0); baris tak tersentuh.
    let r = app
        .delete("/admin/screenshot/queue")
        .token(&token)
        .json(&json!({ "reason": "  bersihkan antrean  " }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Screenshot queue cleared","removed":{"waiting":1,"failed":0}}"#
    );
    assert_eq!(queue::len(st, QUEUE).await.unwrap(), 0);
    let r = app
        .delete("/admin/screenshot/queue")
        .token(&token)
        .json(&json!({ "reason": "bersihkan lagi" }))
        .send()
        .await;
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Screenshot queue cleared","removed":{"waiting":0,"failed":0}}"#
    );
    assert!(row_exists(&app, d1).await);

    // Enqueue ulang dua job (masih dijeda): tetap tidak diproses.
    mz_screenshots::jobs::enqueue_purge(st, &[d1, d2])
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(row_exists(&app, d1).await && row_exists(&app, d2).await);

    // Resume: kedua job diproses (limiter 3 dtk -> sampai ~7 dtk), baris terhapus, antrean kosong.
    let r = app
        .post("/admin/screenshot/queue/resume")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Screenshot queue resumed"}"#
    );
    assert!(!queue::is_paused(st, QUEUE).await.unwrap());
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while (row_exists(&app, d1).await || row_exists(&app, d2).await)
        && std::time::Instant::now() < end
    {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!row_exists(&app, d1).await && !row_exists(&app, d2).await);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while queue::len(st, QUEUE).await.unwrap() > 0 && std::time::Instant::now() < end {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(queue::len(st, QUEUE).await.unwrap(), 0);
    worker.abort();

    assert_eq!(
        audit_rows(&app, op).await,
        vec![
            json!(["screenshot.queue_pause", "screenshot", null, {}, null]),
            json!([
                "screenshot.queue_clear",
                "screenshot",
                null,
                { "reason": "  bersihkan antrean  " },
                "  bersihkan antrean  "
            ]),
            json!([
                "screenshot.queue_clear",
                "screenshot",
                null,
                { "reason": "bersihkan lagi" },
                "bersihkan lagi"
            ]),
            json!(["screenshot.queue_resume", "screenshot", null, {}, null]),
        ]
    );
    reset_queue(&app).await;
}
