//! Emulator GCS JSON API kecil di proses uji (`STORAGE_EMULATOR_HOST`, seperti
//! `@google-cloud/storage`). TIDAK pernah menyentuh GCS sungguhan. Dipakai `tests/common`
//! mz-billing dan mz-invoices (dulu salinan identik). Crate lain (attendance, leaves,
//! visit-forms, products, ...) punya emulator sendiri dengan perilaku berbeda (menyimpan
//! content-type, URL bertanda tangan, dst.).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

pub type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

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
        let mut k: Vec<String> = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    pub fn put(&self, key: &str, data: &[u8]) {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), data.to_vec());
    }

    pub fn set_fail_upload(&self, v: bool) {
        self.fail_upload.store(v, Ordering::SeqCst);
    }
}

pub async fn start_emulator() -> Emulator {
    let store: Store = Arc::default();
    let fail = Arc::new(AtomicBool::new(false));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
    let (s, f, d) = (store.clone(), fail.clone(), deleted.clone());
    let handler = move |method: Method, uri: Uri, body: Bytes| {
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
                objects
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name, body.to_vec());
                return (StatusCode::OK, "{}").into_response();
            }
            let Some(key) = path
                .strip_prefix("/storage/v1/b/")
                .and_then(|rest| rest.split_once("/o/"))
                .map(|(_, k)| mz_core::qs::decode(k))
            else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let mut objects = objects.lock().unwrap_or_else(PoisonError::into_inner);
            let reply: Response = match method {
                Method::GET => match objects.get(&key) {
                    None => StatusCode::NOT_FOUND.into_response(),
                    Some(b) if query.contains("alt=media") => {
                        (StatusCode::OK, b.clone()).into_response()
                    }
                    Some(b) => (
                        StatusCode::OK,
                        serde_json::json!({
                            "contentType": "application/pdf",
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
                        deleted
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(key);
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
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => panic!("emulator GCS: bind gagal: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("emulator GCS: local_addr gagal: {e}"),
    };
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
