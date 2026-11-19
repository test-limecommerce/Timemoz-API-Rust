//! Port `src/monitoring/*` (`MonitoringModule`): laporan berkala ke webhook Discord
//! (`DISCORD_MONITORING_WEBHOOK`) berisi jumlah user yang sedang tracking, sales yang sedang
//! kunjungan, serta CPU/RAM/disk droplet dan Managed Database DigitalOcean.
//!
//! - Aktif hanya bila `MONITORING_CRON_ENABLED=true` (persis, seperti Node) DAN
//!   `DISCORD_MONITORING_WEBHOOK` terisi (tanpa webhook: warn, tidak dijadwalkan). Gerbangnya
//!   env ini, BUKAN `MZ_JOBS` — sama dengan Node, dan laporan ini bukan cron `@nestjs/schedule`.
//! - Interval `MONITORING_INTERVAL_MINUTES` (1–1440, default 10), diselaraskan ke jam dinding
//!   (interval 10 → :00, :10, :20, …).
//! - Setiap replika menjadwalkan; hanya pemenang `SET NX PX` Redis
//!   `monitoring-report:<intervalMs>:<slot>` yang mengirim (kunci & nilai sama dengan Node).
//! - Tidak pernah gagal ke pemanggil: galat dilog, slot berikutnya tetap dijadwalkan.
//!
//! Semua I/O (Redis, Postgres, DigitalOcean, Discord, env) lewat [`Backend`]; produksi
//! [`Live`], unit test memakai tiruan tanpa jaringan dan tanpa env proses. Penjadwal hanya
//! dinyalakan oleh [`spawn`], yang dipanggil `mz-server` `main.rs` (bukan oleh `mz_server::app`).
//!
//! Deviasi: `plan/deviations/mz-misc.md` ("Laporan monitoring Discord").

pub mod digitalocean;
pub mod report;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as CDuration, NaiveDate, Utc};
use fred::prelude::*;
use fred::types::{Expiration, SetOptions};
use mz_core::AppState;
use mz_core::jsfmt;
use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

pub use digitalocean::{DB_CPU_SAMPLE_GAP, DB_CPU_SAMPLES, DbMetricsAccess, DigitalOceanMetrics};
pub use report::{
    ACTIVE_TRACKING_WINDOW_MS, BOT_NAME, MonitoringReport, ResourceUsage, alert_user_ids_from_env,
    build_discord_payload, interval_minutes_from_env, is_discord_webhook_url,
};

/// Node: `timeout: 10_000` (axios Discord & DigitalOcean).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Node: src/monitoring/digitalocean-metrics.client.ts:158 `baseURL`.
pub const DO_API_BASE: &str = "https://api.digitalocean.com/v2";

/// Hasil `countActivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityCounts {
    pub active_user_tracking: i64,
    pub active_sales_on_visit: i64,
}

/// Semua I/O laporan monitoring. Produksi: [`Live`]. Uji: tiruan (tidak ada jaringan).
pub trait Backend: Send + Sync + 'static {
    /// Env (`Config::var`: kosong = `None`).
    fn env(&self, key: &str) -> Option<String>;
    /// `SET key value NX PX ttl_ms`; `Ok(true)` = kunci baru diset (`'OK'`).
    fn claim(
        &self,
        key: &str,
        value: &str,
        ttl_ms: u64,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;
    /// [`count_activity`].
    fn count_activity(
        &self,
        now: DateTime<Utc>,
    ) -> impl Future<Output = anyhow::Result<ActivityCounts>> + Send;
    /// `GET {DO_API_BASE}{path}?query` dengan `Authorization: Bearer <token>`; badan JSON.
    /// Status non-2xx = galat (axios).
    fn do_get(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;
    /// `GET access.url` (basic auth) dengan HANYA `access.ca_pem` sebagai CA tepercaya; badan teks.
    fn db_metrics(
        &self,
        access: &DbMetricsAccess,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// `POST url` JSON (webhook Discord). Status non-2xx = galat.
    fn post_json(&self, url: &str, body: &Value)
    -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Parameter query `countActivity`: `(since, now, takenDate dari, takenDate sampai)`.
/// `takenDate` = tanggal lokal user, jadi dibuka ±1 hari dari tanggal Jakarta.
pub fn activity_params(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>, NaiveDate, NaiveDate) {
    let since = now - CDuration::milliseconds(ACTIVE_TRACKING_WINDOW_MS);
    (since, now, jakarta_date(now, -1), jakarta_date(now, 1))
}

/// Node: src/monitoring/monitoring-report.service.ts:110 `jakartaDate` (YYYY-MM-DD di
/// Asia/Jakarta, digeser `days` hari).
fn jakarta_date(now: DateTime<Utc>, days: i64) -> NaiveDate {
    (now + CDuration::days(days))
        .with_timezone(&chrono_tz::Asia::Jakarta)
        .date_naive()
}

/// Node: src/monitoring/monitoring-report.service.ts:334 `countActivity`.
///
/// Tracking: segmen `status = 1`, bukan manual time, `endAtUtc` dalam 10 menit terakhir;
/// filter `takenDate` juga yang membuat Postgres memakai index `takenDate`. Kolom waktu
/// `TIMESTAMP` tanpa zona berisi UTC, jadi parameter `timestamptz` diubah lewat
/// `AT TIME ZONE 'UTC'` (SQL sama dengan Node). Sales: satu sales maksimal satu kunjungan
/// `active` (partial unique index), jadi `COUNT(*)` = jumlah sales.
pub async fn count_activity(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<ActivityCounts> {
    let (since, now, from, to) = activity_params(now);
    let tracking = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(DISTINCT t."userId") AS total
           FROM   "tracking" t
           JOIN   "trackingTimeline" tl
                  ON tl."trackingId" = t."id"
                 AND tl."status" = 1
                 AND tl."manualTimeStatus" IS NULL
                 AND tl."endAtUtc"   >= ($1::timestamptz AT TIME ZONE 'UTC')
                 AND tl."startAtUtc" <= ($2::timestamptz AT TIME ZONE 'UTC')
           WHERE  t."takenDate" BETWEEN $3::date AND $4::date"#,
    )
    .bind(since)
    .bind(now)
    .bind(from)
    .bind(to)
    .fetch_one(db);
    let visits = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) AS total
           FROM   "storeVisit"
           WHERE  "state" = 'active'"#,
    )
    .fetch_one(db);
    let (active_user_tracking, active_sales_on_visit) = tokio::try_join!(tracking, visits)?;
    Ok(ActivityCounts {
        active_user_tracking,
        active_sales_on_visit,
    })
}

/// Kunci klaim slot: `monitoring-report:<intervalMs>:<floor(now / intervalMs)>` (sama dengan
/// Node, tanpa prefix — cache-manager memanggil client Redis langsung).
pub fn slot_key(interval_ms: u64, now: DateTime<Utc>) -> String {
    let interval = i64::try_from(interval_ms).unwrap_or(i64::MAX).max(1);
    let slot = now.timestamp_millis().div_euclid(interval);
    format!("monitoring-report:{interval_ms}:{slot}")
}

/// Node: src/monitoring/monitoring-report.service.ts:243 — jeda ke batas slot berikutnya
/// (`ceil((now + 1) / interval) * interval - now`), selalu 1..=interval ms.
pub fn delay_until_next_slot(now_ms: i64, interval_ms: u64) -> Duration {
    let interval = i64::try_from(interval_ms).unwrap_or(i64::MAX).max(1);
    let next = (now_ms.saturating_add(interval)).div_euclid(interval) * interval;
    Duration::from_millis(u64::try_from(next - now_ms).unwrap_or(1))
}

/// Keputusan `onApplicationBootstrap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `MONITORING_CRON_ENABLED` bukan `"true"`.
    Disabled,
    /// Aktif tetapi `DISCORD_MONITORING_WEBHOOK` kosong (warn).
    MissingWebhook,
    Enabled {
        minutes: u32,
    },
}

/// Node: src/monitoring/monitoring-report.service.ts:223 `onApplicationBootstrap`.
pub fn gate(enabled: Option<&str>, webhook: Option<&str>, interval: Option<&str>) -> Gate {
    if enabled != Some("true") {
        return Gate::Disabled;
    }
    if webhook.is_none_or(|w| jsfmt::trim(w).is_empty()) {
        return Gate::MissingWebhook;
    }
    Gate::Enabled {
        minutes: interval_minutes_from_env(interval),
    }
}

/// Node: src/monitoring/monitoring-report.service.ts:211 `MonitoringReportService`.
pub struct Reporter<B> {
    backend: Arc<B>,
    digital_ocean: DigitalOceanMetrics<B>,
    db_samples: u32,
    db_gap: Duration,
}

impl<B: Backend> Reporter<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self::with_db_sampling(backend, DB_CPU_SAMPLES, DB_CPU_SAMPLE_GAP)
    }

    /// Seperti [`Reporter::new`] dengan jumlah/jarak sampel CPU database lain (uji).
    pub fn with_db_sampling(backend: Arc<B>, samples: u32, gap: Duration) -> Self {
        Self {
            digital_ocean: DigitalOceanMetrics::new(backend.clone()),
            backend,
            db_samples: samples,
            db_gap: gap,
        }
    }

    /// Node: src/monitoring/monitoring-report.service.ts:253 `runScheduled`.
    /// Satu putaran terjadwal. Tidak pernah gagal: penjadwal tetap jalan.
    pub async fn run_scheduled(&self, interval_ms: u64, now: DateTime<Utc>) {
        let result = async {
            if !self.claim_slot(interval_ms, now).await {
                return Ok(());
            }
            let report = self.collect(now).await;
            self.send(&report).await
        }
        .await;
        if let Err(e) = result {
            tracing::error!("Gagal mengirim laporan monitoring: {e}");
        }
    }

    /// Node: src/monitoring/monitoring-report.service.ts:273 `claimSlot`.
    ///
    /// Hanya satu instance per slot yang boleh mengirim: klaim atomik `SET NX` di Redis (bukan
    /// cek-lalu-tulis). Redis gagal → tetap kirim: laporan ganda lebih baik daripada laporan
    /// yang hilang tepat saat ada masalah.
    pub async fn claim_slot(&self, interval_ms: u64, now: DateTime<Utc>) -> bool {
        let key = slot_key(interval_ms, now);
        let pid = std::process::id().to_string();
        match self.backend.claim(&key, &pid, interval_ms).await {
            Ok(claimed) => claimed,
            Err(e) => {
                tracing::warn!("Klaim slot monitoring gagal, tetap mengirim: {e}");
                true
            }
        }
    }

    /// Node: src/monitoring/monitoring-report.service.ts:307 `collect`.
    /// Masing-masing sumber gagal sendiri-sendiri (→ `None`).
    pub async fn collect(&self, now: DateTime<Utc>) -> MonitoringReport {
        let (counts, server, database) = tokio::join!(
            self.backend.count_activity(now),
            self.digital_ocean.droplet_usage(),
            self.digital_ocean
                .database_usage(self.db_samples, self.db_gap),
        );
        let counts = counts
            .inspect_err(|e| tracing::error!("Gagal menghitung aktivitas: {e}"))
            .ok();
        MonitoringReport {
            at: now,
            active_user_tracking: counts.map(|c| c.active_user_tracking),
            active_sales_on_visit: counts.map(|c| c.active_sales_on_visit),
            server,
            database,
        }
    }

    /// Node: src/monitoring/monitoring-report.service.ts:366 `send`. Webhook dan penerima alert
    /// dibaca dari env setiap kirim (seperti `process.env` Node).
    pub async fn send(&self, report: &MonitoringReport) -> anyhow::Result<()> {
        let webhook = self
            .backend
            .env("DISCORD_MONITORING_WEBHOOK")
            .map(|w| jsfmt::trim(&w).to_owned())
            .filter(|w| !w.is_empty());
        let Some(webhook) = webhook else {
            anyhow::bail!("DISCORD_MONITORING_WEBHOOK belum diisi");
        };
        if !is_discord_webhook_url(&webhook) {
            anyhow::bail!("DISCORD_MONITORING_WEBHOOK bukan URL webhook Discord");
        }
        let ids = alert_user_ids_from_env(
            self.backend
                .env("MONITORING_ALERT_DISCORD_USER_IDS")
                .as_deref(),
        );
        self.backend
            .post_json(&webhook, &build_discord_payload(report, &ids))
            .await
    }
}

/// Loop penjadwal: tunggu batas slot, jalankan satu putaran, ulangi. Berhenti segera saat
/// `stop` dibatalkan, termasuk di tengah putaran (laporan yang sedang dikumpulkan dibuang).
async fn run_loop<B: Backend>(
    reporter: Arc<Reporter<B>>,
    interval_ms: u64,
    stop: CancellationToken,
) {
    loop {
        let delay = delay_until_next_slot(Utc::now().timestamp_millis(), interval_ms);
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = reporter.run_scheduled(interval_ms, Utc::now()) => {}
        }
    }
}

/// Pegangan penjadwal yang sedang jalan (Node `onModuleDestroy` = [`MonitoringHandle::stop`]).
pub struct MonitoringHandle {
    stop: CancellationToken,
}

impl MonitoringHandle {
    /// Hentikan penjadwal (timer berikutnya tidak jalan; putaran yang sedang jalan dibatalkan).
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// Node: src/monitoring/monitoring-report.service.ts:223 `onApplicationBootstrap`.
///
/// Dipanggil SEKALI dari `mz-server` `main.rs`. Gerbang: `MONITORING_CRON_ENABLED=true` +
/// `DISCORD_MONITORING_WEBHOOK` (bukan `MZ_JOBS`). Task dilacak `mz_core::bg`, jadi
/// `bg::drain` saat shutdown menunggu sampai loop keluar sesudah [`MonitoringHandle::stop`].
pub fn spawn(state: &AppState) -> Option<MonitoringHandle> {
    let config = &state.config;
    let minutes = match gate(
        config.var("MONITORING_CRON_ENABLED").as_deref(),
        config.var("DISCORD_MONITORING_WEBHOOK").as_deref(),
        config.var("MONITORING_INTERVAL_MINUTES").as_deref(),
    ) {
        Gate::Disabled => return None,
        Gate::MissingWebhook => {
            tracing::warn!(
                "MONITORING_CRON_ENABLED=true tetapi DISCORD_MONITORING_WEBHOOK kosong; laporan tidak dijadwalkan."
            );
            return None;
        }
        Gate::Enabled { minutes } => minutes,
    };
    tracing::info!("Laporan monitoring ke Discord tiap {minutes} menit.");
    let reporter = Arc::new(Reporter::new(Arc::new(Live::new(state.clone()))));
    let stop = CancellationToken::new();
    mz_core::bg::spawn(run_loop(
        reporter,
        u64::from(minutes) * 60_000,
        stop.clone(),
    ));
    Some(MonitoringHandle { stop })
}

/// [`Backend`] produksi: Redis/Postgres dari `AppState`, HTTP lewat `state.http` (klien
/// bersama) dengan batas waktu 10 dtk per request, kecuali metrics endpoint database yang
/// butuh klien sendiri (hanya CA milik DO yang dipercaya, seperti `https.Agent({ ca })` Node).
pub struct Live {
    state: AppState,
    /// Klien metrics database per PEM CA (dibangun ulang bila CA berubah).
    db_client: Mutex<Option<(String, reqwest::Client)>>,
}

impl Live {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            db_client: Mutex::new(None),
        }
    }

    fn db_client(&self, ca_pem: &str) -> anyhow::Result<reqwest::Client> {
        let mut guard = self
            .db_client
            .lock()
            .map_err(|_| anyhow::anyhow!("lock klien metrics database rusak"))?;
        if let Some((pem, client)) = guard.as_ref()
            && pem == ca_pem
        {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes())?)
            .connect_timeout(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        *guard = Some((ca_pem.to_owned(), client.clone()));
        Ok(client)
    }
}

impl Backend for Live {
    fn env(&self, key: &str) -> Option<String> {
        self.state.config.var(key)
    }

    async fn claim(&self, key: &str, value: &str, ttl_ms: u64) -> anyhow::Result<bool> {
        let px = i64::try_from(ttl_ms).unwrap_or(i64::MAX);
        let res: Option<String> = self
            .state
            .redis
            .next()
            .set(
                key,
                value,
                Some(Expiration::PX(px)),
                Some(SetOptions::NX),
                false,
            )
            .await?;
        Ok(res.is_some())
    }

    async fn count_activity(&self, now: DateTime<Utc>) -> anyhow::Result<ActivityCounts> {
        Ok(count_activity(&self.state.db, now).await?)
    }

    async fn do_get(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        Ok(self
            .state
            .http
            .get(format!("{DO_API_BASE}{path}"))
            .bearer_auth(token)
            .query(query)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?)
    }

    async fn db_metrics(&self, access: &DbMetricsAccess) -> anyhow::Result<String> {
        let client = self.db_client(&access.ca_pem)?;
        Ok(client
            .get(&access.url)
            .basic_auth(&access.username, Some(&access.password))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }

    async fn post_json(&self, url: &str, body: &Value) -> anyhow::Result<()> {
        self.state
            .http
            .post(url)
            .json(body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Tiruan [`Backend`] untuk unit test: env dari daftar eksplisit (BUKAN env proses / `.env`),
/// semua "jaringan" hanya dicatat. Tidak ada request keluar.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod test_support {
    use std::collections::{HashMap, VecDeque};

    use super::*;

    /// Satu panggilan `do_get`: `(path, query)`.
    pub type DoCall = (String, Vec<(String, String)>);

    pub struct FakeBackend {
        env: HashMap<String, String>,
        do_responses: Mutex<HashMap<String, Result<Value, String>>>,
        do_calls: Mutex<Vec<DoCall>>,
        metrics: Mutex<VecDeque<Result<String, String>>>,
        metrics_calls: Mutex<Vec<DbMetricsAccess>>,
        pub claim_result: Mutex<Result<bool, String>>,
        pub claims: Mutex<Vec<(String, String, u64)>>,
        pub counts: Mutex<Result<ActivityCounts, String>>,
        pub count_calls: Mutex<Vec<DateTime<Utc>>>,
        pub post_result: Mutex<Result<(), String>>,
        pub posts: Mutex<Vec<(String, Value)>>,
    }

    impl FakeBackend {
        pub fn new(env: &[(&str, &str)]) -> Self {
            Self {
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                do_responses: Mutex::new(HashMap::new()),
                do_calls: Mutex::new(Vec::new()),
                metrics: Mutex::new(VecDeque::new()),
                metrics_calls: Mutex::new(Vec::new()),
                claim_result: Mutex::new(Ok(true)),
                claims: Mutex::new(Vec::new()),
                counts: Mutex::new(Ok(ActivityCounts {
                    active_user_tracking: 12,
                    active_sales_on_visit: 3,
                })),
                count_calls: Mutex::new(Vec::new()),
                post_result: Mutex::new(Ok(())),
                posts: Mutex::new(Vec::new()),
            }
        }

        pub fn on_do(&self, path: &str, response: Result<Value, String>) {
            self.do_responses
                .lock()
                .unwrap()
                .insert(path.to_owned(), response);
        }

        pub fn push_metrics(&self, response: Result<&str, &str>) {
            self.metrics
                .lock()
                .unwrap()
                .push_back(response.map(str::to_owned).map_err(str::to_owned));
        }

        pub fn do_calls(&self) -> Vec<DoCall> {
            self.do_calls.lock().unwrap().clone()
        }

        pub fn metrics_calls(&self) -> Vec<DbMetricsAccess> {
            self.metrics_calls.lock().unwrap().clone()
        }
    }

    impl Backend for FakeBackend {
        fn env(&self, key: &str) -> Option<String> {
            self.env.get(key).filter(|v| !v.trim().is_empty()).cloned()
        }

        async fn claim(&self, key: &str, value: &str, ttl_ms: u64) -> anyhow::Result<bool> {
            self.claims
                .lock()
                .unwrap()
                .push((key.to_owned(), value.to_owned(), ttl_ms));
            self.claim_result
                .lock()
                .unwrap()
                .clone()
                .map_err(anyhow::Error::msg)
        }

        async fn count_activity(&self, now: DateTime<Utc>) -> anyhow::Result<ActivityCounts> {
            self.count_calls.lock().unwrap().push(now);
            self.counts
                .lock()
                .unwrap()
                .clone()
                .map_err(anyhow::Error::msg)
        }

        async fn do_get(
            &self,
            _token: &str,
            path: &str,
            query: &[(&str, String)],
        ) -> anyhow::Result<Value> {
            self.do_calls.lock().unwrap().push((
                path.to_owned(),
                query
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), v.clone()))
                    .collect(),
            ));
            let response = self.do_responses.lock().unwrap().get(path).cloned();
            response
                .unwrap_or_else(|| Err("404".to_owned()))
                .map_err(anyhow::Error::msg)
        }

        async fn db_metrics(&self, access: &DbMetricsAccess) -> anyhow::Result<String> {
            self.metrics_calls.lock().unwrap().push(access.clone());
            let next = self.metrics.lock().unwrap().pop_front();
            next.unwrap_or_else(|| Err("tidak ada respons".to_owned()))
                .map_err(anyhow::Error::msg)
        }

        async fn post_json(&self, url: &str, body: &Value) -> anyhow::Result<()> {
            self.posts
                .lock()
                .unwrap()
                .push((url.to_owned(), body.clone()));
            self.post_result
                .lock()
                .unwrap()
                .clone()
                .map_err(anyhow::Error::msg)
        }
    }
}

/// Port `monitoring-report.service.spec.ts` (`MonitoringReportService`).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::test_support::FakeBackend;
    use super::*;

    const WEBHOOK: &str = "https://discord.com/api/webhooks/123/abc";
    const TEN_MIN: u64 = 600_000;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn fake(extra: &[(&str, &str)]) -> Arc<FakeBackend> {
        let mut env = vec![
            ("DISCORD_MONITORING_WEBHOOK", WEBHOOK),
            ("DO_TOKEN", "token"),
            ("DO_DROPLET_ID", "549306589"),
            ("DO_DATABASE_ID", "db-uuid"),
        ];
        env.extend_from_slice(extra);
        let f = FakeBackend::new(&env);
        // Droplet: CPU 20, RAM 40, disk 22.
        let series = |labels: Value, values: Value| json!({ "metric": labels, "values": values });
        let ok = |result: Vec<Value>| Ok(json!({ "data": { "result": result } }));
        f.on_do(
            "/monitoring/metrics/droplet/cpu",
            ok(vec![
                series(json!({ "mode": "idle" }), json!([[1, "0"], [2, "80"]])),
                series(json!({ "mode": "user" }), json!([[1, "0"], [2, "20"]])),
            ]),
        );
        f.on_do(
            "/monitoring/metrics/droplet/memory_available",
            ok(vec![series(json!({}), json!([[1, "60"]]))]),
        );
        f.on_do(
            "/monitoring/metrics/droplet/memory_total",
            ok(vec![series(json!({}), json!([[1, "100"]]))]),
        );
        f.on_do(
            "/monitoring/metrics/droplet/filesystem_free",
            ok(vec![series(
                json!({ "mountpoint": "/" }),
                json!([[1, "78"]]),
            )]),
        );
        f.on_do(
            "/monitoring/metrics/droplet/filesystem_size",
            ok(vec![series(
                json!({ "mountpoint": "/" }),
                json!([[1, "100"]]),
            )]),
        );
        // Database: CPU 25, RAM 67, disk 15.
        f.on_do(
            "/databases/db-uuid",
            Ok(json!({ "database": { "metrics_endpoints": [{ "host": "db.host", "port": 9273 }] } })),
        );
        f.on_do(
            "/databases/db-uuid/ca",
            Ok(json!({ "ca": { "certificate": "UEVN" } })),
        );
        f.on_do(
            "/databases/metrics/credentials",
            Ok(json!({ "credentials": { "basic_auth_username": "u", "basic_auth_password": "p" } })),
        );
        f.push_metrics(Ok(
            "cpu_usage_idle{cpu=\"cpu-total\"} 75\nmem_used_percent 67\ndisk_used_percent 15",
        ));
        Arc::new(f)
    }

    fn reporter(f: &Arc<FakeBackend>) -> Reporter<FakeBackend> {
        Reporter::with_db_sampling(f.clone(), 1, Duration::ZERO)
    }

    fn usage(cpu: f64, mem: f64, disk: f64) -> ResourceUsage {
        ResourceUsage {
            cpu_percent: Some(cpu),
            mem_percent: Some(mem),
            disk_percent: Some(disk),
        }
    }

    fn sample_report() -> MonitoringReport {
        MonitoringReport {
            at: at("2026-10-23T04:20:00.000Z"),
            active_user_tracking: Some(1234),
            active_sales_on_visit: Some(37),
            server: usage(21.5, 38.4, 22.6),
            database: usage(24.5, 66.9, 14.9),
        }
    }

    #[test]
    fn count_activity_window_and_jakarta_dates() {
        let now = at("2026-10-23T18:30:00.000Z"); // 01:30 WIB tgl 24
        let (since, until, from, to) = activity_params(now);
        assert_eq!(since, at("2026-10-23T18:20:00.000Z"));
        assert_eq!(until, now);
        assert_eq!(from, NaiveDate::from_ymd_opt(2026, 10, 23).unwrap());
        assert_eq!(to, NaiveDate::from_ymd_opt(2026, 10, 25).unwrap());
    }

    #[tokio::test]
    async fn collect_combines_counts_and_metrics() {
        let f = fake(&[]);
        let now = at("2026-10-23T04:20:00.000Z");
        assert_eq!(
            reporter(&f).collect(now).await,
            MonitoringReport {
                at: now,
                active_user_tracking: Some(12),
                active_sales_on_visit: Some(3),
                server: usage(20.0, 40.0, 22.0),
                database: usage(25.0, 67.0, 15.0),
            }
        );
        assert_eq!(*f.count_calls.lock().unwrap(), vec![now]);
    }

    #[tokio::test]
    async fn collect_db_failure_keeps_infra_metrics() {
        let f = fake(&[]);
        *f.counts.lock().unwrap() = Err("db down".into());
        let r = reporter(&f).collect(Utc::now()).await;
        assert_eq!(r.active_user_tracking, None);
        assert_eq!(r.active_sales_on_visit, None);
        assert_eq!(r.server, usage(20.0, 40.0, 22.0));
    }

    #[tokio::test]
    async fn send_posts_payload_to_webhook() {
        let f = fake(&[]);
        reporter(&f).send(&sample_report()).await.unwrap();
        let posts = f.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        // URL yang "dikirim" = nilai dari env tiruan; tiruan tidak membuka koneksi apa pun.
        assert_eq!(posts[0].0, WEBHOOK);
        assert_eq!(posts[0].1["username"], "Timemoz Monitoring");
    }

    #[tokio::test]
    async fn send_alert_ids_from_env() {
        let f = fake(&[("MONITORING_ALERT_DISCORD_USER_IDS", "123456789012345678")]);
        let mut r = sample_report();
        r.server = usage(90.0, 10.0, 10.0);
        reporter(&f).send(&r).await.unwrap();
        let posts = f.posts.lock().unwrap().clone();
        assert_eq!(
            posts[0].1["content"],
            "<@123456789012345678>\n\u{26a0}\u{fe0f} Server CPU 90.0% (> 80%)"
        );
    }

    #[tokio::test]
    async fn send_without_webhook_fails() {
        let f = Arc::new(FakeBackend::new(&[]));
        let err = reporter(&f).send(&sample_report()).await.unwrap_err();
        assert_eq!(err.to_string(), "DISCORD_MONITORING_WEBHOOK belum diisi");
        assert!(f.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_rejects_non_discord_url_without_request() {
        let f = Arc::new(FakeBackend::new(&[(
            "DISCORD_MONITORING_WEBHOOK",
            "https://evil.com/api/webhooks/1/x",
        )]));
        let err = reporter(&f).send(&sample_report()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "DISCORD_MONITORING_WEBHOOK bukan URL webhook Discord"
        );
        assert!(f.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn claim_set_nx_px_per_slot() {
        let f = fake(&[]);
        let now = at("2026-10-23T04:27:00.000Z");
        assert!(reporter(&f).claim_slot(TEN_MIN, now).await);
        let slot = now.timestamp_millis() / 600_000;
        let claims = f.claims.lock().unwrap().clone();
        assert_eq!(claims[0].0, format!("monitoring-report:600000:{slot}"));
        assert_eq!(claims[0].1, std::process::id().to_string());
        assert_eq!(claims[0].2, TEN_MIN);
    }

    #[tokio::test]
    async fn claim_taken_by_other_instance() {
        let f = fake(&[]);
        *f.claim_result.lock().unwrap() = Ok(false);
        assert!(!reporter(&f).claim_slot(TEN_MIN, Utc::now()).await);
    }

    #[test]
    fn slot_key_same_within_slot() {
        let a = slot_key(TEN_MIN, at("2026-10-23T04:20:00.000Z"));
        let b = slot_key(TEN_MIN, at("2026-10-23T04:29:59.999Z"));
        let c = slot_key(TEN_MIN, at("2026-10-23T04:30:00.000Z"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn claim_redis_error_fails_open() {
        let f = fake(&[]);
        *f.claim_result.lock().unwrap() = Err("ECONNREFUSED".into());
        assert!(reporter(&f).claim_slot(TEN_MIN, Utc::now()).await);
    }

    #[tokio::test]
    async fn run_scheduled_claimed_sends() {
        let f = fake(&[]);
        reporter(&f).run_scheduled(TEN_MIN, Utc::now()).await;
        assert_eq!(f.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_scheduled_other_instance_no_query_no_send() {
        let f = fake(&[]);
        *f.claim_result.lock().unwrap() = Ok(false);
        reporter(&f).run_scheduled(TEN_MIN, Utc::now()).await;
        assert!(f.posts.lock().unwrap().is_empty());
        assert!(f.count_calls.lock().unwrap().is_empty());
        assert!(f.do_calls().is_empty());
    }

    #[tokio::test]
    async fn run_scheduled_discord_failure_does_not_fail() {
        let f = fake(&[]);
        *f.post_result.lock().unwrap() = Err("429".into());
        reporter(&f).run_scheduled(TEN_MIN, Utc::now()).await;
        assert_eq!(f.posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn gate_cases() {
        assert_eq!(gate(None, Some(WEBHOOK), None), Gate::Disabled);
        assert_eq!(gate(Some("TRUE"), Some(WEBHOOK), None), Gate::Disabled);
        assert_eq!(gate(Some("1"), Some(WEBHOOK), None), Gate::Disabled);
        assert_eq!(gate(Some("true"), None, None), Gate::MissingWebhook);
        assert_eq!(gate(Some("true"), Some("  "), None), Gate::MissingWebhook);
        assert_eq!(
            gate(Some("true"), Some(WEBHOOK), None),
            Gate::Enabled { minutes: 10 }
        );
        assert_eq!(
            gate(Some("true"), Some(WEBHOOK), Some("5")),
            Gate::Enabled { minutes: 5 }
        );
    }

    #[test]
    fn first_run_aligned_to_next_slot() {
        let now = at("2026-10-23T04:23:00.000Z").timestamp_millis();
        // Default 10 menit: jalan pertama 04:30, lalu tiap 10 menit.
        assert_eq!(
            delay_until_next_slot(now, TEN_MIN),
            Duration::from_secs(7 * 60)
        );
        let edge = at("2026-10-23T04:29:59.999Z").timestamp_millis();
        assert_eq!(
            delay_until_next_slot(edge, TEN_MIN),
            Duration::from_millis(1)
        );
        let on = at("2026-10-23T04:30:00.000Z").timestamp_millis();
        assert_eq!(
            delay_until_next_slot(on, TEN_MIN),
            Duration::from_secs(10 * 60)
        );
        // MONITORING_INTERVAL_MINUTES=5 → 04:25.
        assert_eq!(
            delay_until_next_slot(now, 300_000),
            Duration::from_secs(2 * 60)
        );
        // Interval 60 → jam penuh berikutnya.
        let h = Utc
            .with_ymd_and_hms(2026, 10, 23, 4, 0, 1)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            delay_until_next_slot(h, 3_600_000),
            Duration::from_secs(3599)
        );
    }

    #[tokio::test]
    async fn stop_ends_loop_without_running() {
        let f = fake(&[]);
        let stop = CancellationToken::new();
        let task = tokio::spawn(run_loop(Arc::new(reporter(&f)), 60 * TEN_MIN, stop.clone()));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(f.claims.lock().unwrap().is_empty());
        assert!(f.posts.lock().unwrap().is_empty());
    }
}
