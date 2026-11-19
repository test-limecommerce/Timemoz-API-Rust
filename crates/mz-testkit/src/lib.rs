//! Harness uji integrasi terhadap Postgres & Redis NYATA.
//!
//! - Setiap PROSES binary test (`tests/<nama>.rs`) mendapat satu database
//!   `mz_test_<nama>__p<pid>` hasil `CREATE DATABASE … TEMPLATE $MZ_TEST_TEMPLATE`
//!   (default `mudah_test_template`: dump lokal + migrasi Prisma terbaru,
//!   data sudah dianonimkan). PID di nama membuat binary bernama sama (mis. `it_jobs` di
//!   empat crate) atau binary yang sama dijalankan paralel tidak saling men-drop database.
//!   Database `mz_test_*__p<pid>` milik proses yang sudah mati (cek `/proc/<pid>`, hanya
//!   Linux; tanpa `/proc` pembersihan dilewati) di-drop saat binary berikutnya mulai.
//! - Redis memakai DB `$MZ_TEST_REDIS_DB` (default 15) supaya tidak
//!   bercampur dengan kunci Node lokal (DB 0).
//! - Tanpa `MZ_TEST_DATABASE_URL` test dilewati (dicetak `SKIP`), kecuali
//!   `MZ_TEST_REQUIRE_DB=1` (CI) yang membuatnya gagal.
//!
//! ```ignore
//! #[tokio::test]
//! async fn list_projects() {
//!     let Some(app) = TestApp::new(mz_server::app).await else { return };
//!     let fx = app.company().await;
//!     let res = app.get("/projects").token(&fx.super_admin).send().await;
//!     res.assert_ok();
//! }
//! ```
//!
//! Env:
//! - `MZ_TEST_DATABASE_URL` — URL server Postgres (nama database diabaikan),
//!   mis. `postgres://postgres:pw@127.0.0.1:5432/postgres`.
//! - `MZ_TEST_TEMPLATE` — nama database template (default `mudah_test_template`).
//! - `MZ_TEST_REDIS_URL` — tidak dipakai; host/port dari `REDIS_HOST`/`REDIS_PORT`
//!   (default localhost:6379), password `REDIS_PASSWORD`.

#![allow(clippy::missing_panics_doc, clippy::panic)]

pub mod gcs;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use mz_core::AppState;
use mz_core::config::Config;
use serde_json::{Map, Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;

/// Secret JWT khusus test (sama dengan `Config::for_test`).
pub const JWT_SECRET: &str = "test-secret";

/// Env berisi kredensial layanan luar yang TIDAK boleh ada saat uji integrasi.
const FORBIDDEN_ENV: &[&str] = &[
    "MAILTRAP_TOKEN",
    "MIXPANEL_PROJECT_TOKEN",
    "DISCORD_WEBHOOK_URL",
    "PAYPAL_CLIENT_ID",
    "PAYPAL_CLIENT_SECRET",
    "GOOGLE_OAUTH_CLIENT_SECRET",
    "JIRA_CLIENT_SECRET",
    "GCP_KEY_PATH",
    // Laporan monitoring (`mz_misc::monitoring`): webhook Discord & API DigitalOcean.
    "DISCORD_MONITORING_WEBHOOK",
    "DO_TOKEN",
];

/// Nilai `Config::var` untuk test (menang atas env proses; tidak mengubah env).
pub const TEST_VARS: &[(&str, &str)] = &[
    ("IDLETIME_IN_SECOND", "180"),
    ("EXPIRED_AT", "30"),
    ("HOST_FE", "http://fe.test"),
    ("HOST_BE", "http://be.test"),
    ("GOOGLE_REDIRECT_URL", "auth/google-redirect"),
    ("GOOGLE_OAUTH_CLIENT_ID", "test-client"),
];

/// Hasil inisialisasi database per binary: `Ok(url)` / `Err(alasan skip)`.
static DB_URL: OnceLock<Result<String, String>> = OnceLock::new();

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// `mz_test_<target>` dari nama executable test (`it_projects-1a2b3c` → `it_projects`); skema
/// nama lama (tanpa PID).
fn db_base() -> String {
    let exe = std::env::current_exe().ok();
    let stem = exe
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let target = stem.rsplit_once('-').map_or(stem, |(a, _)| a);
    // Batas identifier Postgres 63 byte: `mz_test_` + 42 + `__p` + PID (≤ 10 digit).
    let clean: String = target
        .chars()
        .take(42)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("mz_test_{clean}")
}

/// Pemisah PID di nama database uji.
const PID_SEP: &str = "__p";

/// `mz_test_<target>__p<pid>`: unik per proses uji.
fn db_name() -> String {
    format!("{}{PID_SEP}{}", db_base(), std::process::id())
}

/// PID pemilik database uji `mz_test_*__p<pid>`; `None` = bukan nama berformat itu.
fn owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("mz_test_")?;
    let (_, pid) = rest.rsplit_once(PID_SEP)?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// `Some(hidup?)` lewat `/proc/<pid>`; `None` bila `/proc` tidak ada (bukan Linux).
fn pid_alive(pid: u32) -> Option<bool> {
    let proc = std::path::Path::new("/proc");
    proc.is_dir().then(|| proc.join(pid.to_string()).exists())
}

/// Drop database uji milik proses yang sudah mati (+ nama lama tanpa PID untuk binary ini,
/// yang dulu di-drop setiap kali binary mulai). Galat diabaikan: hanya pembersihan.
async fn drop_stale_databases(conn: &mut PgConnection) {
    let me = std::process::id();
    let names: Vec<String> = sqlx::query_scalar(
        r"SELECT datname::text FROM pg_database WHERE datname LIKE 'mz\_test\_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap_or_default();
    let legacy = db_base();
    for name in names {
        let stale = match owner_pid(&name) {
            Some(pid) => pid != me && pid_alive(pid) == Some(false),
            None => name == legacy,
        };
        if stale {
            let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
                .execute(&mut *conn)
                .await;
        }
    }
}

/// Ganti path (nama database) pada URL Postgres; parameter `schema=` (gaya
/// Prisma) dibuang karena tidak dikenal sqlx.
fn with_database(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
    let authority = rest.split_once('/').map_or(rest, |(a, _)| a);
    let query: Vec<&str> = query
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with("schema="))
        .collect();
    if query.is_empty() {
        format!("{scheme}://{authority}/{db}")
    } else {
        format!("{scheme}://{authority}/{db}?{}", query.join("&"))
    }
}

async fn create_database(server_url: &str) -> Result<String, String> {
    let template = env_opt("MZ_TEST_TEMPLATE").unwrap_or_else(|| "mudah_test_template".into());
    let name = db_name();
    let admin_url = with_database(server_url, "postgres");
    let mut conn = PgConnection::connect(&admin_url)
        .await
        .map_err(|e| format!("koneksi ke {admin_url} gagal: {e}"))?;
    drop_stale_databases(&mut conn).await;
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
        .execute(&mut conn)
        .await
        .map_err(|e| format!("drop {name}: {e}"))?;
    // Template tidak boleh sedang dipakai sesi lain; ulangi sebentar bila bentrok
    // dengan binary lain yang sedang meng-clone.
    let mut last = String::new();
    for attempt in 0..30 {
        match sqlx::query(&format!(
            r#"CREATE DATABASE "{name}" TEMPLATE "{template}" STRATEGY FILE_COPY"#
        ))
        .execute(&mut conn)
        .await
        {
            Ok(_) => return Ok(with_database(server_url, &name)),
            Err(e) => {
                last = e.to_string();
                if !last.contains("being accessed by other users") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200 + attempt * 100)).await;
            }
        }
    }
    Err(format!("create {name} dari template {template}: {last}"))
}

fn init_db() -> Result<String, String> {
    let Some(server_url) = env_opt("MZ_TEST_DATABASE_URL") else {
        return Err("MZ_TEST_DATABASE_URL tidak diset".into());
    };
    // Kredensial layanan luar di env = test bisa mengirim email/event/pembayaran
    // sungguhan. Tolak; jalankan test dari shell tanpa `.env` produksi/dev.
    let leaked: Vec<&str> = FORBIDDEN_ENV
        .iter()
        .copied()
        .filter(|k| env_opt(k).is_some())
        .collect();
    assert!(
        leaked.is_empty(),
        "uji integrasi menolak berjalan: env {leaked:?} diset (bisa memanggil layanan luar sungguhan)"
    );
    // Dijalankan di thread + runtime sendiri: setiap `#[tokio::test]` punya
    // runtime masing-masing yang mati di akhir test.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(create_database(&server_url))
    })
    .join()
    .unwrap_or_else(|_| Err("thread pembuat database panic".into()))
}

/// Aplikasi lengkap (router `mz-server`) di atas database test.
pub struct TestApp {
    pub state: AppState,
    router: Router,
    ip_seq: AtomicU32,
}

impl TestApp {
    /// `None` = test dilewati (env tidak diset). Panggil:
    /// `let Some(app) = TestApp::new(mz_server::app).await else { return };`
    pub async fn new(build: fn(&AppState) -> Router) -> Option<Self> {
        Self::with_vars(build, &[]).await
    }

    /// Seperti [`TestApp::new`], dengan `Config::var_overrides` tambahan
    /// (menang atas [`TEST_VARS`]; mis. `CACHE_INVALIDATION_CHANNEL` unik).
    pub async fn with_vars(build: fn(&AppState) -> Router, vars: &[(&str, &str)]) -> Option<Self> {
        let url = match DB_URL.get_or_init(init_db) {
            Ok(url) => url.clone(),
            Err(reason) => {
                assert!(
                    env_opt("MZ_TEST_REQUIRE_DB").is_none(),
                    "MZ_TEST_REQUIRE_DB=1 tetapi database test tidak siap: {reason}"
                );
                eprintln!("SKIP uji integrasi: {reason}");
                return None;
            }
        };
        let mut config = Config::for_test();
        config.database_url = url;
        config.db_max_connections = 5;
        if let Some(h) = env_opt("REDIS_HOST") {
            config.redis_host = h;
        }
        if let Some(p) = env_opt("REDIS_PORT").and_then(|p| p.parse().ok()) {
            config.redis_port = p;
        }
        config.redis_password = env_opt("REDIS_PASSWORD");
        // Env non-rahasia yang di produksi diisi `.env` (dibaca lewat `Config::var`).
        for (k, v) in TEST_VARS {
            config
                .var_overrides
                .insert((*k).to_owned(), (*v).to_owned());
        }
        for (k, v) in vars {
            config
                .var_overrides
                .insert((*k).to_owned(), (*v).to_owned());
        }
        config.redis_db = Some(
            env_opt("MZ_TEST_REDIS_DB")
                .and_then(|v| v.parse().ok())
                .unwrap_or(15),
        );
        let state = AppState::connect(config)
            .await
            .unwrap_or_else(|e| panic!("AppState::connect gagal: {e:#}"));
        let router = build(&state);
        Some(Self {
            state,
            router,
            // Awal acak: Redis DB test dipakai bersama semua test/binary, jadi IP
            // pertama tiap TestApp tidak boleh sama (limiter per IP).
            ip_seq: AtomicU32::new(uuid::Uuid::new_v4().as_u128() as u32 & 0x00ff_ffff),
        })
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    /// `AppState` aplikasi uji (untuk memanggil fungsi service/latar secara langsung).
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// JWT user biasa `{userId}` tanpa `sid` (tidak ada cek sesi device).
    pub fn token(&self, user_id: i32) -> String {
        self.token_with(json!({ "userId": user_id }))
    }

    /// JWT dengan payload bebas (`iat` ditambahkan bila belum ada).
    pub fn token_with(&self, payload: Value) -> String {
        let mut map: Map<String, Value> = match payload {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        map.entry("iat")
            .or_insert_with(|| Value::from(chrono_now_secs()));
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &map,
            &jsonwebtoken::EncodingKey::from_secret(self.state.config.jwt_secret.as_bytes()),
        )
        .unwrap_or_else(|e| panic!("encode jwt: {e}"))
    }

    /// IP unik per request (`x-forwarded-for`) supaya limiter per-IP tidak saling ganggu.
    fn next_ip(&self) -> String {
        let n = self.ip_seq.fetch_add(1, Ordering::Relaxed);
        let [_, a, b, c] = n.to_be_bytes();
        format!("10.{a}.{b}.{c}")
    }

    pub fn request(&self, method: Method, uri: &str) -> RequestBuilder<'_> {
        RequestBuilder {
            app: self,
            method,
            uri: uri.to_owned(),
            headers: HeaderMap::new(),
            body: Body::empty(),
            content_type: None,
            ip: None,
        }
    }
    pub fn get(&self, uri: &str) -> RequestBuilder<'_> {
        self.request(Method::GET, uri)
    }
    pub fn post(&self, uri: &str) -> RequestBuilder<'_> {
        self.request(Method::POST, uri)
    }
    pub fn patch(&self, uri: &str) -> RequestBuilder<'_> {
        self.request(Method::PATCH, uri)
    }
    pub fn put(&self, uri: &str) -> RequestBuilder<'_> {
        self.request(Method::PUT, uri)
    }
    pub fn delete(&self, uri: &str) -> RequestBuilder<'_> {
        self.request(Method::DELETE, uri)
    }

    /// Fixture company dengan keempat role bawaan (lihat [`Company`]).
    pub async fn company(&self) -> Company {
        Company::load(self).await
    }

    /// Company kedua minimal dengan satu ADMIN aktif sebagai satu-satunya anggotanya (company
    /// default + langganan ACTIVE, jadi lolos gerbang). Untuk uji lintas-tenant: dia admin yang
    /// SAH, tetapi bukan di [`Company`] fixture. Data tidak dihapus (seperti fixture lain);
    /// username/nama unik per pemanggilan.
    pub async fn other_company_admin(&self) -> OtherCompany {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tag = format!("{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed));
        let company: (i32, String) = sqlx::query_as(&format!(
            "INSERT INTO company (name, status) VALUES ('Other {tag}', 1) RETURNING id, uuid::text"
        ))
        .fetch_one(self.db())
        .await
        .unwrap_or_else(|e| panic!("other company: {e}"));
        let user: (i32, String, String) = sqlx::query_as(&format!(
            "INSERT INTO users (username, password, name, status)
             VALUES ('other_{tag}', 'x', 'Other {tag}', 1) RETURNING id, uuid::text, username"
        ))
        .fetch_one(self.db())
        .await
        .unwrap_or_else(|e| panic!("other admin: {e}"));
        let (company_id, user_id) = (company.0, user.0);
        self.exec(&format!(
            r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
               VALUES ({user_id}, {company_id}, 1, 300, true);
               INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
               VALUES ({user_id}, (SELECT id FROM "accessLevel" WHERE code = 'ADMIN' AND "isSystem" ORDER BY id LIMIT 1),
                       {company_id}, 1);
               INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers")
               VALUES ({company_id}, (SELECT id FROM "pricingTiers" ORDER BY id LIMIT 1), NOW() - interval '1 day',
                       NOW() + interval '1 year', 'ACTIVE'::"SubscriptionStatus", 10);"#
        ))
        .await;
        OtherCompany {
            id: company_id,
            uuid: company.1,
            admin: User {
                id: user_id,
                uuid: user.1,
                username: user.2,
                token: self.token(user_id),
            },
        }
    }

    /// Satu nilai skalar dari SQL (helper asersi).
    pub async fn scalar<T>(&self, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar::<_, T>(sql)
            .fetch_one(self.db())
            .await
            .unwrap_or_else(|e| panic!("scalar `{sql}`: {e}"))
    }

    /// Jalankan SQL bebas (setup data).
    pub async fn exec(&self, sql: &str) {
        sqlx::raw_sql(sql)
            .execute(self.db())
            .await
            .unwrap_or_else(|e| panic!("exec `{sql}`: {e}"));
    }
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

pub struct RequestBuilder<'a> {
    app: &'a TestApp,
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Body,
    content_type: Option<String>,
    ip: Option<String>,
}

impl RequestBuilder<'_> {
    #[must_use]
    pub fn token(mut self, token: &str) -> Self {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            self.headers.insert(http::header::AUTHORIZATION, v);
        }
        self
    }
    /// Shortcut: token untuk user id.
    #[must_use]
    pub fn user(self, user_id: i32) -> Self {
        let t = self.app.token(user_id);
        self.token(&t)
    }
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
            self.headers.insert(n, v);
        }
        self
    }
    /// IP tetap (untuk menguji rate limit per IP).
    #[must_use]
    pub fn ip(mut self, ip: &str) -> Self {
        self.ip = Some(ip.to_owned());
        self
    }
    #[must_use]
    pub fn json(mut self, body: &Value) -> Self {
        self.body = Body::from(serde_json::to_vec(body).unwrap_or_default());
        self.content_type = Some("application/json".into());
        self
    }
    #[must_use]
    pub fn raw(mut self, content_type: &str, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self.content_type = Some(content_type.to_owned());
        self
    }

    pub async fn send(self) -> TestResponse {
        let ip = self.ip.unwrap_or_else(|| self.app.next_ip());
        let mut req = Request::builder()
            .method(self.method.clone())
            .uri(&self.uri)
            .body(self.body)
            .unwrap_or_else(|e| panic!("request {} {}: {e}", self.method, self.uri));
        *req.headers_mut() = self.headers;
        if let Ok(v) = HeaderValue::from_str(&ip) {
            req.headers_mut().insert("x-forwarded-for", v);
        }
        if let Some(ct) = self.content_type
            && let Ok(v) = HeaderValue::from_str(&ct)
        {
            req.headers_mut().insert(http::header::CONTENT_TYPE, v);
        }
        let res = self
            .app
            .router
            .clone()
            .oneshot(req)
            .await
            .unwrap_or_else(|e| match e {});
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes().to_vec())
            .unwrap_or_default();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        TestResponse {
            method: self.method,
            uri: self.uri,
            status,
            headers,
            bytes,
            json,
        }
    }
}

#[derive(Debug)]
pub struct TestResponse {
    pub method: Method,
    pub uri: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
    pub json: Value,
}

impl TestResponse {
    fn ctx(&self) -> String {
        let body = if self.json.is_null() {
            String::from_utf8_lossy(&self.bytes)
                .chars()
                .take(2000)
                .collect()
        } else {
            let s = self.json.to_string();
            s.chars().take(2000).collect::<String>()
        };
        format!("{} {} -> {}\n{body}", self.method, self.uri, self.status)
    }

    /// Status harus persis `expected`.
    pub fn assert_status(&self, expected: u16) -> &Self {
        assert_eq!(self.status.as_u16(), expected, "{}", self.ctx());
        self
    }

    /// 2xx DAN, bila body punya `success`, nilainya `true`.
    pub fn assert_ok(&self) -> &Self {
        assert!(self.status.is_success(), "{}", self.ctx());
        if let Some(s) = self.json.get("success") {
            assert_eq!(s, &Value::Bool(true), "{}", self.ctx());
        }
        self
    }

    /// Bukan 5xx dan body tidak memuat jejak galat SQL (`column … does not exist`, dll.).
    /// Dipakai untuk route yang hasilnya bergantung data, tapi SQL-nya harus sah.
    pub fn assert_no_sql_error(&self) -> &Self {
        assert!(!self.status.is_server_error(), "{}", self.ctx());
        let text = String::from_utf8_lossy(&self.bytes).to_lowercase();
        for needle in SQL_ERROR_NEEDLES {
            assert!(
                !text.contains(needle),
                "jejak galat SQL `{needle}`: {}",
                self.ctx()
            );
        }
        self
    }

    pub fn data(&self) -> &Value {
        self.json.get("data").unwrap_or(&Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Potongan pesan galat Postgres/sqlx yang menandakan SQL atau decode salah.
pub const SQL_ERROR_NEEDLES: &[&str] = &[
    "does not exist",
    "syntax error",
    "error occurred while decoding",
    "mismatched types",
    "unexpected null",
    "operator does not exist",
    "could not determine data type",
    "invalid input syntax",
    "error returned from database",
    "no column found",
    "column index out of bounds",
    "is ambiguous",
    "violates",
];

/// Company uji dengan user aktif per role bawaan. Dipilih dari template:
/// company yang punya SUPER_ADMIN, ADMIN, MANAGER, STAFF aktif (default
/// company 2). Saat dimuat, langganan terkini dibuat ACTIVE s.d. +1 tahun dan
/// modul TIME_TRACKING + SALES_TRACKER dinyalakan supaya gerbang
/// subscription-lock / feature-module terbuka.
#[derive(Debug, Clone)]
pub struct Company {
    pub id: i32,
    pub uuid: String,
    pub super_admin: User,
    pub admin: User,
    pub manager: User,
    pub staff: User,
    /// STAFF kedua (untuk uji akses lintas user).
    pub staff2: User,
}

/// Company kedua hasil [`TestApp::other_company_admin`].
#[derive(Debug, Clone)]
pub struct OtherCompany {
    pub id: i32,
    pub uuid: String,
    pub admin: User,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i32,
    pub uuid: String,
    pub username: String,
    pub token: String,
}

impl Company {
    async fn load(app: &TestApp) -> Self {
        let id: i32 = env_opt("MZ_TEST_COMPANY_ID")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let uuid: String = app
            .scalar(&format!("SELECT uuid::text FROM company WHERE id = {id}"))
            .await;
        Self::open_gates(app, id).await;
        let pick = |code: &'static str, nth: i64| async move {
            let row: (i32, String, String) = sqlx::query_as(
                r#"SELECT u.id, u.uuid::text, u.username
                   FROM users u
                   JOIN "userCompany" uc ON uc."userId" = u.id AND uc.status = 1 AND uc."companyId" = $1
                   JOIN "userAccessLevel" ual ON ual."userId" = u.id AND ual.status = 1 AND ual."companyId" = $1
                   JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                   WHERE u.status = 1 AND al.code = $2
                     AND NOT EXISTS (SELECT 1 FROM "userCompany" o
                                     WHERE o."userId" = u.id AND o.status = 1 AND o."companyId" <> $1)
                   ORDER BY u.id OFFSET $3 LIMIT 1"#,
            )
            .bind(id)
            .bind(code)
            .bind(nth)
            .fetch_one(app.db())
            .await
            .unwrap_or_else(|e| panic!("fixture: user {code}#{nth} di company {id}: {e}"));
            User {
                id: row.0,
                uuid: row.1,
                username: row.2,
                token: app.token(row.0),
            }
        };
        Self {
            id,
            uuid,
            super_admin: pick("SUPER_ADMIN", 0).await,
            admin: pick("ADMIN", 0).await,
            manager: pick("MANAGER", 0).await,
            staff: pick("STAFF", 0).await,
            staff2: pick("STAFF", 1).await,
        }
    }

    async fn open_gates(app: &TestApp, id: i32) {
        app.exec(&format!(
            r#"UPDATE company SET "moduleTimeTrackingEnabled" = true, "moduleSalesTrackerEnabled" = true
               WHERE id = {id};
               -- Template anonim bisa tanpa langganan untuk company ini.
               INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers")
               SELECT {id}, (SELECT id FROM "pricingTiers" ORDER BY id LIMIT 1), NOW() - interval '1 day',
                      NOW() + interval '1 year', 'ACTIVE'::"SubscriptionStatus", 10
               WHERE NOT EXISTS (SELECT 1 FROM subscriptions WHERE "companyId" = {id});
               UPDATE subscriptions SET status = 'ACTIVE', "endDate" = NOW() + interval '1 year'
               WHERE id = (SELECT id FROM subscriptions WHERE "companyId" = {id}
                           ORDER BY "startDate" DESC, id DESC LIMIT 1);"#
        ))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{PID_SEP, db_name, owner_pid, with_database};

    #[test]
    fn nama_database_memuat_pid() {
        let name = db_name();
        assert!(name.starts_with("mz_test_"));
        assert!(name.ends_with(&format!("{PID_SEP}{}", std::process::id())));
        assert!(name.len() <= 63);
        assert_eq!(owner_pid(&name), Some(std::process::id()));
        assert_eq!(owner_pid("mz_test_it_jobs__p123"), Some(123));
        assert_eq!(owner_pid("mz_test_it_jobs"), None);
        assert_eq!(owner_pid("mz_test_it_jobs__p"), None);
        assert_eq!(owner_pid("mz_test_x__p12a"), None);
        assert_eq!(owner_pid("other__p1"), None);
    }

    #[test]
    fn replaces_database_in_url() {
        assert_eq!(
            with_database("postgres://u:p@h:5432/mudah?schema=public", "x"),
            "postgres://u:p@h:5432/x"
        );
        assert_eq!(
            with_database("postgres://u:p@h:5432/mudah", "x"),
            "postgres://u:p@h:5432/x"
        );
        assert_eq!(
            with_database("postgres://u:p@h:5432", "x"),
            "postgres://u:p@h:5432/x"
        );
    }
}
