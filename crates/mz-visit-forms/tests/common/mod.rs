//! Pembantu uji integrasi bersama `mz-visit-forms`:
//! - [`start_emulator`]: emulator GCS JSON API kecil di proses uji (`STORAGE_EMULATOR_HOST`,
//!   seperti `@google-cloud/storage`). TIDAK pernah menyentuh GCS sungguhan.
//!   (Salinan `crates/mz-billing/tests/common/mod.rs`, ditambah content type per object.)
//! - [`app`]: `TestApp` dengan router crate ini + layer audit + emulator.
//! - [`open_form_gates`]: modul SALES, paket `form`/`order`, permission `store-visit-forms`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mz_core::AppState;
use mz_testkit::TestApp;

pub const BUCKET: &str = "it-visit-forms-bucket";
/// `FILE_URL_SECRET` uji (URL file mobile bertanda tangan).
pub const FILE_SECRET: &str = "it-visit-forms-file-secret-0123456789abcdef";

/// key → (byte, content type).
pub type Store = Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>;

pub struct Emulator {
    /// `http://127.0.0.1:<port>`.
    pub host: String,
    pub store: Store,
    /// `true` → unggahan dijawab 500.
    pub fail_upload: Arc<AtomicBool>,
    /// Key yang pernah dihapus (urut).
    pub deleted: Arc<Mutex<Vec<String>>>,
}

impl Emulator {
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.store.lock().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.store.lock().unwrap().get(key).map(|(b, _)| b.clone())
    }

    /// Simulasikan PUT klien ke signed URL.
    pub fn put(&self, key: &str, data: &[u8], content_type: &str) {
        self.store
            .lock()
            .unwrap()
            .insert(key.to_owned(), (data.to_vec(), content_type.to_owned()));
    }

    pub fn set_fail_upload(&self, v: bool) {
        self.fail_upload.store(v, Ordering::SeqCst);
    }

    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }
}

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

/// Router crate + layer audit (seperti `mz-server`).
pub fn build(state: &AppState) -> Router {
    mz_visit_forms::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// `TestApp` + emulator GCS. `None` = uji dilewati (DB tidak diset).
pub async fn app() -> Option<(TestApp, Emulator)> {
    let em = start_emulator().await;
    // Kunci service account uji: signed URL baca/tulis ditandatangani lokal (tanpa ADC).
    let key_path = format!(
        "{}/it_visit_forms_{}_sa.json",
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
    ];
    let app = TestApp::with_vars(build, &vars).await?;
    Some((app, em))
}

/// Gerbang route form untuk company fixture: modul SALES (sudah dibuka `app.company()`),
/// paket `form` + `order` lewat `companyPlanOverride`, permission `store-visit-forms`
/// untuk role bawaan.
pub async fn open_form_gates(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"INSERT INTO "companyPlanOverride" ("companyId", form, "order", product)
           VALUES ({company_id}, true, true, true)
           ON CONFLICT ("companyId") DO UPDATE SET form = true, "order" = true, product = true;
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS x(a)
           CROSS JOIN (VALUES ('store-visit-forms'), ('store-visits')) AS f(k)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER', 'STAFF')
           ON CONFLICT DO NOTHING;"#
    ))
    .await;
}
