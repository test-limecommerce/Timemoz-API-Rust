//! Pembantu uji bersama: emulator GCS JSON API kecil di proses uji (`STORAGE_EMULATOR_HOST`,
//! seperti `@google-cloud/storage`) — TIDAK pernah menyentuh GCS sungguhan — plus kunci service
//! account uji (`mz-files/src/test_key.pem`) untuk menandatangani URL secara lokal.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::json;

/// key → (content-type, byte).
pub type Store = Arc<Mutex<HashMap<String, (String, Vec<u8>)>>>;

pub struct Emulator {
    pub host: String,
    pub store: Store,
}

impl Emulator {
    pub fn get(&self, key: &str) -> Option<(String, Vec<u8>)> {
        self.store.lock().unwrap().get(key).cloned()
    }
    pub fn put(&self, key: &str, content_type: &str, data: &[u8]) {
        self.store
            .lock()
            .unwrap()
            .insert(key.to_owned(), (content_type.to_owned(), data.to_vec()));
    }
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.store.lock().unwrap().keys().cloned().collect();
        k.sort();
        k
    }
}

pub async fn start_emulator() -> Emulator {
    let store: Store = Arc::default();
    let s = store.clone();
    let handler = move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let objects = s.clone();
        async move {
            let path = uri.path().to_owned();
            let query = uri.query().unwrap_or_default().to_owned();
            if method == Method::POST && path.starts_with("/upload/storage/v1/b/") {
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
                objects.lock().unwrap().insert(name, (ct, body.to_vec()));
                return (StatusCode::OK, "{}").into_response();
            }
            let Some(key) = path
                .strip_prefix("/storage/v1/b/")
                .and_then(|rest| rest.split_once("/o/"))
                .map(|(_, k)| mz_core::qs::decode(k))
            else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let objects = objects.lock().unwrap();
            let reply: Response = match (method, objects.get(&key)) {
                (Method::GET, None) => StatusCode::NOT_FOUND.into_response(),
                (Method::GET, Some((_, b))) if query.contains("alt=media") => {
                    (StatusCode::OK, b.clone()).into_response()
                }
                (Method::GET, Some((ct, b))) => (
                    StatusCode::OK,
                    json!({
                        "contentType": ct,
                        "size": b.len().to_string(),
                        "generation": "7",
                        "etag": "e",
                    })
                    .to_string(),
                )
                    .into_response(),
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            };
            reply
        }
    };
    let app = Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Emulator {
        host: format!("http://{addr}"),
        store,
    }
}

pub const BUCKET: &str = "it-attendance-bucket";
pub const FILE_SECRET: &str = "it-attendance-file-url-secret-0123456789";

fn build(state: &AppState) -> Router {
    mz_attendance::router(state).with_state(state.clone())
}

/// TestApp + emulator GCS + kunci penanda tangan uji.
pub async fn app(tag: &str) -> Option<(TestApp, Emulator)> {
    let em = start_emulator().await;
    let key_path = format!(
        "{}/it_attendance_{tag}_sa.json",
        env!("CARGO_TARGET_TMPDIR")
    );
    std::fs::write(
        &key_path,
        json!({
            "client_email": "sa@test.iam.gserviceaccount.com",
            "private_key": include_str!("../../../mz-files/src/test_key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    let app = TestApp::with_vars(
        build,
        &[
            ("STORAGE_EMULATOR_HOST", em.host.as_str()),
            ("GCP_BUCKET_NAME", BUCKET),
            ("GCP_KEY_PATH", &key_path),
            ("FILE_URL_SECRET", FILE_SECRET),
        ],
    )
    .await?;
    Some((app, em))
}

/// Body multipart sederhana: field teks + berkas opsional `(field, filename, mime, bytes)`.
pub fn multipart(
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &str, &[u8])>,
) -> (String, Vec<u8>) {
    const B: &str = "----mzattendanceboundary";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((field, filename, mime, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{B}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={B}"), body)
}

/// Hapus data absensi fixture company (shift, break, geofence & zona uji, log geofence).
pub async fn reset(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"DELETE FROM "mobileShift" WHERE "companyId" = {company_id};
           DELETE FROM "geofenceAuditLog" WHERE "geofenceId" IN (SELECT id FROM geofence WHERE "companyId" = {company_id});
           DELETE FROM geofence WHERE "companyId" = {company_id};
           DELETE FROM "negativeZone" WHERE "companyId" = {company_id};
           DELETE FROM "mobileBreakPolicy" WHERE "companyId" = {company_id};
           UPDATE company SET "mobileRequireSelfie" = false, "mobileBlockMockLocation" = false,
                              "timeZone" = 'UTC'
            WHERE id = {company_id};"#
    ))
    .await;
}

/// Geofence uji; kembalikan (id, uuid).
pub async fn geofence(
    app: &TestApp,
    company_id: i32,
    creator: i32,
    name: &str,
    lat: f64,
    lng: f64,
    radius: i32,
    enforced: bool,
    schedule: Option<&str>,
) -> (i32, String) {
    sqlx::query_as(
        r#"INSERT INTO geofence ("companyId", name, address, lat, lng, "radiusMeters", enforced,
                                 "applicableTo", "applicableIds", "createdById", timezone, "weeklySchedule")
           VALUES ($1, $2, 'Jl. Uji 1', $3, $4, $5, $6, 'all', '{}', $7, 'UTC', $8::jsonb)
           RETURNING id, uuid::text"#,
    )
    .bind(company_id)
    .bind(name)
    .bind(lat)
    .bind(lng)
    .bind(radius)
    .bind(enforced)
    .bind(creator)
    .bind(schedule)
    .fetch_one(app.db())
    .await
    .unwrap()
}

pub fn gps(lat: f64, lng: f64, mock: bool) -> String {
    json!({ "lat": lat, "lng": lng, "accuracy": 10, "isMock": mock }).to_string()
}
