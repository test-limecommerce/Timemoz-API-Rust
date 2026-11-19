//! Pembantu uji integrasi `mz-products`:
//! - [`start_emulator`]: emulator GCS JSON API kecil di proses uji (`STORAGE_EMULATOR_HOST`).
//!   TIDAK pernah menyentuh GCS sungguhan. (Salinan `crates/mz-visit-forms/tests/common/mod.rs`.)
//! - [`setup`]: `TestApp` + emulator + fixture company 2 dengan gerbang produk terbuka.
//! - [`tag`]: akhiran unik untuk kode/SKU (data uji tidak dihapus; DB uji per binary).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

pub const BUCKET: &str = "it-products-bucket";
/// `FILE_URL_SECRET` uji (URL file mobile bertanda tangan).
pub const FILE_SECRET: &str = "it-products-file-secret";

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
    mz_products::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

pub static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

pub struct Ctx {
    pub app: TestApp,
    pub em: Emulator,
    pub fx: Company,
    _guard: MutexGuard<'static, ()>,
}

pub const HOST_BE: &str = "https://api.test";

/// `TestApp` + emulator + gerbang produk untuk company fixture. `None` = uji dilewati.
pub async fn setup() -> Option<Ctx> {
    let guard = SERIAL.lock().await;
    let em = start_emulator().await;
    // Kunci service account uji: signed URL ditandatangani lokal (tanpa ADC).
    let key_path = format!(
        "{}/it_products_{}_sa.json",
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
        ("HOST_BE", HOST_BE),
    ];
    let app = TestApp::with_vars(build, &vars).await?;
    let fx = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = true WHERE id = {cid};
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, k.f, a.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('products'), ('product-masters'), ('price-lists')) AS k(f)
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS a(a)
             WHERE al."isSystem" ON CONFLICT DO NOTHING;"#,
        cid = fx.id
    ))
    .await;
    Some(Ctx {
        app,
        em,
        fx,
        _guard: guard,
    })
}

/// Akhiran unik huruf besar/angka (aman untuk `code` master & SKU).
pub fn tag() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{:X}{}",
        nanos % 0xFFFF_FFFF,
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn code_of(r: &TestResponse) -> String {
    r.json["code"].as_str().unwrap_or_default().to_owned()
}

pub fn messages(r: &TestResponse) -> Vec<String> {
    r.json["message"]
        .as_array()
        .unwrap_or_else(|| panic!("message bukan array: {}", r.text()))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

pub fn assert_key_order(text: &str, keys: &[&str]) {
    let mut from = 0;
    for k in keys {
        let needle = format!("\"{k}\":");
        let pos = text[from..]
            .find(&needle)
            .unwrap_or_else(|| panic!("kunci `{k}` tidak ada sesudah {from} di {text}"));
        from += pos + needle.len();
    }
}

/// Buat satu master (`kind`) lewat API, kembalikan `data`.
pub async fn new_master(c: &Ctx, kind: &str, body: Value) -> Value {
    let r = c
        .app
        .post(&format!("/v1/admin/product-masters/{kind}"))
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data().clone()
}

/// uuid master dari kode (company fixture).
pub async fn master_uuid(c: &Ctx, table: &str, code: &str) -> String {
    c.app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "{table}" WHERE "companyId" = {} AND code = '{code}'"#,
            c.fx.id
        ))
        .await
}
