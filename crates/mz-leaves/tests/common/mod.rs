//! Pembantu uji bersama `mz-leaves`: emulator GCS JSON API kecil di proses uji
//! (`STORAGE_EMULATOR_HOST`, tidak pernah menyentuh GCS sungguhan), kunci service account uji
//! untuk menandatangani URL lokal, dan fixture cuti di company template (`app.company()`).
//! Uji dalam satu binary berbagi company → `SERIAL` + [`reset`] di awal tiap uji.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{Datelike, NaiveDate, Weekday};
use mz_core::AppState;
use mz_testkit::{Company, TestApp};
use serde_json::json;

pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.store.lock().unwrap().keys().cloned().collect();
        k.sort();
        k
    }
}

// DUPLIKAT dari mz-attendance/tests/common — sengaja dibiarkan (dicek 2026-11-18): emulator di sini
// hanya menerima unggahan (GET/metadata → 404), versi absensi melayani GET; emulator billing/invoices
// kini `mz_testkit::gcs`.
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
            let reply: Response = StatusCode::NOT_FOUND.into_response();
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

pub const BUCKET: &str = "it-leaves-bucket";

fn build(state: &AppState) -> Router {
    mz_leaves::router(state).with_state(state.clone())
}

/// TestApp + emulator GCS + kunci penanda tangan uji.
pub async fn app(tag: &str) -> Option<(TestApp, Emulator)> {
    let em = start_emulator().await;
    let key_path = format!("{}/it_leaves_{tag}_sa.json", env!("CARGO_TARGET_TMPDIR"));
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
        ],
    )
    .await?;
    Some((app, em))
}

/// Tahun berjalan (UTC).
pub fn this_year() -> i32 {
    chrono::Utc::now().year()
}

/// Senin pertama bulan `month` tahun `year`.
pub fn first_monday(year: i32, month: u32) -> NaiveDate {
    let mut d = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    while d.weekday() != Weekday::Mon {
        d = d.succ_opt().unwrap();
    }
    d
}

pub fn key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn plus(d: NaiveDate, days: i64) -> NaiveDate {
    d + chrono::Duration::days(days)
}

/// Bersihkan data cuti company fixture dan kembalikan kebijakan bawaan.
pub async fn reset(app: &TestApp, fx: &Company) {
    let c = fx.id;
    app.exec(&format!(
        r#"DELETE FROM "leaveBalanceAdjustment" WHERE "companyId" = {c};
           DELETE FROM "holidayLeaveDeduction" WHERE "companyId" = {c};
           DELETE FROM "googleCalendarEvent" WHERE "leaveRequestId" IN (SELECT id FROM "leaveRequest" WHERE "companyId" = {c});
           DELETE FROM "leaveRequest" WHERE "companyId" = {c};
           DELETE FROM "leaveBalance" WHERE "companyId" = {c};
           DELETE FROM "companyHoliday" WHERE "companyId" = {c};
           DELETE FROM "companyWorkPattern" WHERE "companyId" = {c};
           DELETE FROM "leavePolicyApprover" WHERE "companyId" = {c};
           DELETE FROM "employeeLeavePolicy" WHERE "companyId" = {c};
           DELETE FROM "employeeLeaveTier" WHERE "companyId" = {c};
           DELETE FROM "overtimeRequest" WHERE "companyId" = {c};
           UPDATE "companyLeaveType" SET "isActive" = false WHERE "companyId" = {c};
           UPDATE company SET "timeZone" = 'UTC' WHERE id = {c};
           INSERT INTO "leavePolicy" ("companyId") VALUES ({c}) ON CONFLICT ("companyId") DO NOTHING;
           UPDATE "leavePolicy" SET "annualLeaveDays" = 12, "sickLeaveDays" = NULL,
                  "sickLeaveMode" = 'DEDUCT_ANNUAL', "carryOverEnabled" = false,
                  "maxCarryOverDays" = NULL, "carryOverExpiryMonth" = NULL,
                  "resetType" = 'ANNUAL', "entitlementType" = 'UNIFORM', "accrualMode" = 'UPFRONT',
                  "monthlyAccrualDays" = NULL
            WHERE "companyId" = {c};"#
    ))
    .await;
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// `companyLeaveType` aktif baru.
pub async fn leave_type(
    app: &TestApp,
    co: i32,
    deduct: bool,
    source: &str,
    requires_doc: bool,
    quota: Option<i32>,
) -> i32 {
    let q = quota.map_or("NULL".to_owned(), |q| q.to_string());
    app.scalar(&format!(
        r#"INSERT INTO "companyLeaveType" ("companyId", name, code, "deductBalance", "balanceSource",
             "requiresDocument", "annualQuota")
           VALUES ({co}, 'Cuti {source}', 'T{}', {deduct}, '{source}', {requires_doc}, {q}) RETURNING id"#,
        uniq()
    ))
    .await
}

pub struct Req {
    pub id: i32,
    pub uuid: String,
}

/// Pengajuan cuti langsung di DB.
#[allow(clippy::too_many_arguments)]
pub async fn request(
    app: &TestApp,
    co: i32,
    user: i32,
    type_id: Option<i32>,
    legacy: Option<&str>,
    start: NaiveDate,
    end: NaiveDate,
    total: f64,
    status: &str,
    approver: Option<i32>,
) -> Req {
    let t = type_id.map_or("NULL".to_owned(), |t| t.to_string());
    let l = legacy.map_or("NULL".to_owned(), |l| format!("'{l}'"));
    let a = approver.map_or("NULL".to_owned(), |a| a.to_string());
    let (id, uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "leaveTypeId", "leaveType", "startDate",
             "endDate", "totalDays", reason, status, "approverId")
           VALUES ({user}, {co}, {t}, {l}, '{}', '{}', {total}, 'alasan uji', '{status}', {a})
           RETURNING id, uuid::text"#,
        key(start),
        key(end)
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    Req { id, uuid }
}

/// Saldo cuti `(total, used, carry, carryUsed, sickTotal, sickUsed)`.
#[allow(clippy::too_many_arguments)]
pub async fn balance(
    app: &TestApp,
    co: i32,
    user: i32,
    year: i32,
    total: f64,
    used: f64,
    carry: f64,
    sick_total: f64,
    sick_used: f64,
) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed",
             "carryOverDays", "carryOverUsed", "sickLeaveTotal", "sickLeaveUsed")
           VALUES ({user}, {co}, {year}, {total}, {used}, {carry}, 0, {sick_total}, {sick_used}) RETURNING id"#
    ))
    .await
}

/// Approver kebijakan.
pub async fn policy_approver(app: &TestApp, co: i32, user: i32) {
    app.exec(&format!(
        r#"INSERT INTO "leavePolicyApprover" ("policyId", "userId", "companyId")
           SELECT id, {user}, {co} FROM "leavePolicy" WHERE "companyId" = {co}"#
    ))
    .await;
}

/// `(annualLeaveTotal, annualLeaveUsed, carryOverDays, carryOverUsed, sickLeaveUsed)`.
pub async fn bal_row(app: &TestApp, id: i32) -> (f64, f64, f64, f64, f64) {
    sqlx::query_as(
        r#"SELECT "annualLeaveTotal", "annualLeaveUsed", "carryOverDays", "carryOverUsed", "sickLeaveUsed"
           FROM "leaveBalance" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// `(field, delta, source, adjustedById, leaveRequestId, reason)` riwayat user, urut id.
pub async fn adjustments(
    app: &TestApp,
    user: i32,
) -> Vec<(String, f64, String, Option<i32>, Option<i32>, String)> {
    sqlx::query_as(
        r#"SELECT field, delta, source::text, "adjustedById", "leaveRequestId", reason
           FROM "leaveBalanceAdjustment" WHERE "userId" = $1 ORDER BY id"#,
    )
    .bind(user)
    .fetch_all(app.db())
    .await
    .unwrap()
}

pub async fn status_of(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        r#"SELECT status::text FROM "leaveRequest" WHERE id = {id}"#
    ))
    .await
}

/// Body multipart sederhana: field teks + berkas opsional `(field, filename, mime, bytes)`.
pub fn multipart(
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &str, &[u8])>,
) -> (String, Vec<u8>) {
    const B: &str = "----mzleavesboundary";
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
