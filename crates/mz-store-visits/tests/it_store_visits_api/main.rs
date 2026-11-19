//! Uji integrasi `mz-store-visits` terhadap Postgres nyata (template `mudah_test_template`) dengan
//! emulator GCS di proses uji (`STORAGE_EMULATOR_HOST`). Satu binary, satu submodul per area.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-store-visits --test it_store_visits_api`.
//!
//! Fixture dibuat lewat SQL di company fixture (`app.company()`, id 2) dan diberi kode unik per test
//! ([`tag`]) supaya test paralel tidak bertabrakan. Test yang mengubah pengaturan company
//! (`storeVisit*`) memegang [`SERIAL`].

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

mod admin_sync;
mod groups_plans;
mod stores;
mod visits;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mz_core::AppState;
use mz_testkit::{Company, TestApp};

pub const BUCKET: &str = "it-store-visits-bucket";
/// `FILE_URL_SECRET` uji (≥ 32 byte).
pub const FILE_SECRET: &str = "it-store-visits-file-url-secret-0123456789";

/// Test yang mengubah baris `company` (pengaturan kunjungan, kuota export) memegang kunci ini.
pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// key → (byte, content type).
pub type Store = Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>;

pub struct Emulator {
    pub host: String,
    pub store: Store,
    pub fail_upload: Arc<AtomicBool>,
    pub deleted: Arc<Mutex<Vec<String>>>,
}

impl Emulator {
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.store.lock().unwrap().keys().cloned().collect();
        k.sort();
        k
    }
    /// Simulasikan PUT klien ke signed URL.
    pub fn put(&self, key: &str, data: &[u8], content_type: &str) {
        self.store
            .lock()
            .unwrap()
            .insert(key.to_owned(), (data.to_vec(), content_type.to_owned()));
    }
    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }
}

/// Emulator GCS JSON API kecil (salinan pola `mz-visit-forms/tests/common`).
pub async fn start_emulator() -> Emulator {
    let store: Store = Arc::default();
    let fail = Arc::new(AtomicBool::new(false));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
    let (s, f, d) = (store.clone(), fail.clone(), deleted.clone());
    let handler = move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let (objects, fail, deleted) = (s.clone(), f.clone(), d.clone());
        async move {
            let path = uri.path().to_owned();
            let query = uri.query().unwrap_or_default().to_owned();
            if method == Method::POST && path.starts_with("/upload/storage/v1/b/") {
                if fail.load(Ordering::SeqCst) {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let name = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .map(mz_core::qs::decode)
                    .unwrap_or_default();
                let ct = headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                objects.lock().unwrap().insert(name, (body.to_vec(), ct));
                return (StatusCode::OK, "{}").into_response();
            }
            let Some(key) = path
                .strip_prefix("/storage/v1/b/")
                .and_then(|rest| rest.split_once("/o/"))
                .map(|(_, k)| mz_core::qs::decode(k))
            else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let mut objects = objects.lock().unwrap();
            let reply: Response = match method {
                Method::GET => match objects.get(&key) {
                    None => StatusCode::NOT_FOUND.into_response(),
                    Some((b, ct)) if query.contains("alt=media") => {
                        (StatusCode::OK, [("content-type", ct.clone())], b.clone()).into_response()
                    }
                    Some((b, ct)) => (
                        StatusCode::OK,
                        serde_json::json!({
                            "contentType": ct,
                            "size": b.len().to_string(),
                            "generation": "1",
                            "etag": format!("etag-{}", b.len()),
                        })
                        .to_string(),
                    )
                        .into_response(),
                },
                Method::DELETE => {
                    if objects.remove(&key).is_some() {
                        deleted.lock().unwrap().push(key);
                        StatusCode::NO_CONTENT.into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            };
            reply
        }
    };
    let app = axum::Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Emulator {
        host: format!("http://{addr}"),
        store,
        fail_upload: fail,
        deleted,
    }
}

/// Router crate + layer audit (seperti `mz-server`) + route reveal `mz-table-fields`.
pub fn build(state: &AppState) -> Router {
    mz_store_visits::router(state)
        .merge(mz_table_fields::router(state))
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// `TestApp` + emulator + company fixture dengan gerbang sales terbuka. `None` = DB tidak diset.
pub async fn setup() -> Option<(TestApp, Emulator, Company)> {
    let em = start_emulator().await;
    let key_path = format!(
        "{}/it_store_visits_{}_sa.json",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id()
    );
    std::fs::write(
        &key_path,
        serde_json::json!({
            "client_email": "sa@test.iam.gserviceaccount.com",
            "private_key": include_str!("../../../mz-files/src/test_key.pem"),
        })
        .to_string(),
    )
    .ok()?;
    let vars = [
        ("STORAGE_EMULATOR_HOST", em.host.as_str()),
        ("GCP_BUCKET_NAME", BUCKET),
        ("GCP_KEY_PATH", key_path.as_str()),
        ("FILE_URL_SECRET", FILE_SECRET),
        ("HOST_BE", "https://api.test"),
    ];
    let app = TestApp::with_vars(build, &vars).await?;
    let c = app.company().await;
    open_sales_gates(&app, c.id).await;
    Some((app, em, c))
}

/// Paket `form`/`order`/`product` + toko & auto check-in tanpa batas (test paralel berbagi company) lewat `companyPlanOverride` dan permission READ/WRITE/DELETE
/// fitur sales untuk role sistem.
pub async fn open_sales_gates(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"INSERT INTO "companyPlanOverride" ("companyId", form, "order", product, "maxStores", "maxAutoCheckIns")
           VALUES ({company_id}, true, true, true, -1, -1)
           ON CONFLICT ("companyId") DO UPDATE SET form = true, "order" = true, product = true,
             "maxStores" = -1, "maxAutoCheckIns" = -1;
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS x(a)
           CROSS JOIN (VALUES ('stores'), ('store-groups'), ('store-visits'), ('store-visit-plans'),
                              ('customers')) AS f(k)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER', 'STAFF')
           ON CONFLICT DO NOTHING;"#
    ))
    .await;
}

/// Penanda unik pendek (untuk kode toko/region): huruf besar + angka, ≤ 12 karakter.
pub fn tag() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let s = format!(
        "{:X}{:X}",
        (nanos as u64) & 0xFF_FFFF,
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    s.chars().take(12).collect()
}

/// Toko fixture (lewat SQL). Kembalikan `(id, uuid)`.
pub async fn insert_store(
    app: &TestApp,
    company_id: i32,
    created_by: i32,
    code: &str,
    lat: f64,
    lng: f64,
    group_id: Option<i32>,
) -> (i32, String) {
    sqlx::query_as(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "groupId")
           VALUES ($1, $2, $3, 'Owner', 'Jl. Uji 1', $4, $5, $6, $7) RETURNING id, uuid::text"#,
    )
    .bind(company_id)
    .bind(code)
    .bind(format!("Toko {code}"))
    .bind(lat)
    .bind(lng)
    .bind(created_by)
    .bind(group_id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// Region fixture (lewat SQL). Kembalikan `(id, uuid)`.
pub async fn insert_group(
    app: &TestApp,
    company_id: i32,
    created_by: i32,
    code: &str,
) -> (i32, String) {
    sqlx::query_as(
        r#"INSERT INTO "storeGroup" ("companyId", code, name, "createdById")
           VALUES ($1, $2, $3, $4) RETURNING id, uuid::text"#,
    )
    .bind(company_id)
    .bind(code)
    .bind(format!("Region {code}"))
    .bind(created_by)
    .fetch_one(app.db())
    .await
    .unwrap()
}
