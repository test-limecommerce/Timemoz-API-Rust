//! Port `src/monitoring/digitalocean-metrics.client.ts`: klien baca-saja ke DigitalOcean —
//! Monitoring API (droplet) dan metrics endpoint Managed Database (Prometheus, port 9273,
//! sertifikat CA milik DO).
//!
//! Env: `DO_TOKEN` (scope droplet:read, monitoring:read, database:read,
//! database:view_credentials), `DO_DROPLET_ID`, `DO_DATABASE_ID`. Env kosong atau API gagal →
//! metrik terkait `None`, tidak pernah gagal. Semua HTTP lewat [`Backend`] (uji memakai tiruan).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use mz_core::jsfmt;
use serde::Deserialize;
use serde_json::Value;

use super::Backend;
use super::report::ResourceUsage;

/// Node: src/monitoring/digitalocean-metrics.client.ts:8 `DROPLET_WINDOW_SEC`.
/// Monitoring API mengembalikan sampel tiap ~2 menit; 10 menit cukup untuk selalu punya dua
/// sampel terakhir (CPU dihitung dari selisihnya).
const DROPLET_WINDOW_SEC: i64 = 600;
/// Node: src/monitoring/digitalocean-metrics.client.ts:14 `DB_CPU_SAMPLES`.
pub const DB_CPU_SAMPLES: u32 = 4;
/// Node: src/monitoring/digitalocean-metrics.client.ts:15 `DB_CPU_SAMPLE_GAP_MS`.
pub const DB_CPU_SAMPLE_GAP: Duration = Duration::from_millis(15_000);

/// Node: src/monitoring/digitalocean-metrics.client.ts:29 `MatrixSeries` (lunak: kolom yang
/// tidak ada = kosong).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatrixSeries {
    #[serde(default)]
    pub metric: HashMap<String, Value>,
    /// `[unixSeconds, "nilai"]`.
    #[serde(default)]
    pub values: Vec<Vec<Value>>,
}

/// Kredensial + alamat metrics endpoint database (`DbMetricsAccess`), di-cache di memori.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbMetricsAccess {
    pub url: String,
    pub username: String,
    pub password: String,
    /// PEM CA milik DO (hasil decode base64) — satu-satunya CA yang dipercaya untuk `url`.
    pub ca_pem: String,
}

/// `Math.round(n * 100) / 100` (setengah dibulatkan ke +∞ seperti `Math.round`).
fn round2(n: f64) -> f64 {
    let x = n * 100.0;
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    r / 100.0
}

/// `Number(values[i][1])`: string lewat `Number()` JS, angka apa adanya, selain itu NaN.
fn sample_number(sample: &[Value]) -> f64 {
    match sample.get(1) {
        Some(Value::String(s)) => jsfmt::number(s),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

fn label_is(series: &MatrixSeries, key: &str, value: &str) -> bool {
    series.metric.get(key).and_then(Value::as_str) == Some(value)
}

/// Node: src/monitoring/digitalocean-metrics.client.ts:52 `cpuPercentFromCounters`.
///
/// Persen CPU dari counter per-mode (detik kumulatif, semua core): selisih dua sampel
/// terakhir, `1 - Δidle / Δtotal`. `None` bila sampel kurang dari dua atau counter tidak naik
/// (mis. agent baru restart).
pub fn cpu_percent_from_counters(result: Option<&[MatrixSeries]>) -> Option<f64> {
    let result = result.filter(|r| !r.is_empty())?;
    let mut idle_delta = 0.0;
    let mut total_delta = 0.0;
    for series in result {
        let [.., prev, last] = series.values.as_slice() else {
            return None;
        };
        let delta = sample_number(last) - sample_number(prev);
        if !delta.is_finite() || delta < 0.0 {
            return None;
        }
        total_delta += delta;
        if label_is(series, "mode", "idle") {
            idle_delta += delta;
        }
    }
    if total_delta <= 0.0 {
        return None;
    }
    Some(round2((1.0 - idle_delta / total_delta) * 100.0))
}

/// Node: src/monitoring/digitalocean-metrics.client.ts:75 `lastValue`.
/// Nilai terakhir deret PERTAMA yang labelnya cocok dengan `labels`.
pub fn last_value(result: Option<&[MatrixSeries]>, labels: &[(&str, &str)]) -> Option<f64> {
    let series = result?
        .iter()
        .find(|s| labels.iter().all(|(k, v)| label_is(s, k, v)))?;
    let n = sample_number(series.values.last()?);
    n.is_finite().then_some(n)
}

/// Node: src/monitoring/digitalocean-metrics.client.ts:90 `usedPercent` = `1 - free / total`.
pub fn used_percent(free: Option<f64>, total: Option<f64>) -> Option<f64> {
    let (free, total) = (free?, total?);
    if total <= 0.0 {
        return None;
    }
    Some(round2((1.0 - free / total) * 100.0))
}

/// Node: src/monitoring/digitalocean-metrics.client.ts:102 `promValue`.
///
/// Satu nilai dari teks eksposisi Prometheus. Beberapa deret (mis. cluster multi-node)
/// dirata-rata. `labels` menyaring deret yang labelnya cocok persis.
pub fn prom_value(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let mut samples: Vec<f64> = Vec::new();
    for line in text.split('\n') {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let (label_part, value_part) = if let Some(inner) = rest.strip_prefix('{') {
            let Some((labels, value)) = inner.split_once('}') else {
                continue;
            };
            (labels, value)
        } else if rest.starts_with(' ') {
            ("", rest)
        } else {
            continue;
        };
        let pairs: Vec<&str> = label_part.split(',').collect();
        if !labels
            .iter()
            .all(|(k, v)| pairs.contains(&format!("{k}=\"{v}\"").as_str()))
        {
            continue;
        }
        let first = jsfmt::trim(value_part)
            .split(' ')
            .next()
            .unwrap_or_default();
        let value = jsfmt::number(first);
        if value.is_finite() {
            samples.push(value);
        }
    }
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

/// `r.data?.data?.result` dari respons Monitoring API.
fn matrix_result(body: &Value) -> Option<Vec<MatrixSeries>> {
    let result = body.get("data")?.get("result")?;
    serde_json::from_value(result.clone()).ok()
}

/// `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Nilai JS "truthy" sebagai teks (`${x}`): string tidak kosong, angka bukan 0.
fn truthy_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64().is_some_and(|f| f != 0.0) => Some(n.to_string()),
        _ => None,
    }
}

/// Node: src/monitoring/digitalocean-metrics.client.ts:145 `DigitalOceanMetricsClient`.
pub struct DigitalOceanMetrics<B> {
    backend: Arc<B>,
    /// `DO_TOKEN` (trim; kosong = klien mati).
    token: Option<String>,
    droplet_id: Option<String>,
    database_id: Option<String>,
    /// Kredensial metrics, CA, dan alamat endpoint jarang berubah: disimpan di memori,
    /// dibuang saat pengambilan gagal supaya diambil ulang berikutnya.
    db_access: Mutex<Option<DbMetricsAccess>>,
}

fn env_trimmed<B: Backend>(backend: &B, key: &str) -> Option<String> {
    backend
        .env(key)
        .map(|v| jsfmt::trim(&v).to_owned())
        .filter(|v| !v.is_empty())
}

impl<B: Backend> DigitalOceanMetrics<B> {
    /// Env dibaca sekali (Node: konstruktor).
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            token: env_trimmed(&*backend, "DO_TOKEN"),
            droplet_id: env_trimmed(&*backend, "DO_DROPLET_ID"),
            database_id: env_trimmed(&*backend, "DO_DATABASE_ID"),
            backend,
            db_access: Mutex::new(None),
        }
    }

    /// Node: src/monitoring/digitalocean-metrics.client.ts:165 `getDropletUsage`.
    pub async fn droplet_usage(&self) -> ResourceUsage {
        let (Some(token), Some(droplet_id)) = (&self.token, &self.droplet_id) else {
            return ResourceUsage::EMPTY;
        };
        let end = chrono::Utc::now().timestamp();
        let params = [
            ("host_id", droplet_id.clone()),
            ("start", (end - DROPLET_WINDOW_SEC).to_string()),
            ("end", end.to_string()),
        ];
        let fetch = |metric: &'static str| {
            let params = &params;
            async move {
                let path = format!("/monitoring/metrics/droplet/{metric}");
                match self.backend.do_get(token, &path, params).await {
                    Ok(body) => matrix_result(&body),
                    Err(e) => {
                        tracing::warn!("Gagal membaca metrik droplet {metric}: {e}");
                        None
                    }
                }
            }
        };
        let (cpu, mem_available, mem_total, fs_free, fs_size) = tokio::join!(
            fetch("cpu"),
            fetch("memory_available"),
            fetch("memory_total"),
            fetch("filesystem_free"),
            fetch("filesystem_size"),
        );

        let root = [("mountpoint", "/")];
        ResourceUsage {
            cpu_percent: cpu_percent_from_counters(cpu.as_deref()),
            mem_percent: used_percent(
                last_value(mem_available.as_deref(), &[]),
                last_value(mem_total.as_deref(), &[]),
            ),
            disk_percent: used_percent(
                last_value(fs_free.as_deref(), &root),
                last_value(fs_size.as_deref(), &root),
            ),
        }
    }

    /// Node: src/monitoring/digitalocean-metrics.client.ts:212 `getDatabaseUsage`.
    ///
    /// CPU = rata-rata `samples` bacaan berjarak `gap`; RAM & disk dari bacaan terakhir yang
    /// berhasil. Bacaan yang gagal dilewati — selama minimal satu berhasil, angka tetap
    /// dilaporkan. Produksi: [`DB_CPU_SAMPLES`] × [`DB_CPU_SAMPLE_GAP`].
    pub async fn database_usage(&self, samples: u32, gap: Duration) -> ResourceUsage {
        let (Some(token), Some(database_id)) = (&self.token, &self.database_id) else {
            return ResourceUsage::EMPTY;
        };
        let mut texts: Vec<String> = Vec::new();
        for i in 0..samples {
            if i > 0 {
                tokio::time::sleep(gap).await;
            }
            // Dimuat ulang per bacaan: sesudah bacaan gagal (mis. kredensial dirotasi)
            // cache dikosongkan, jadi bacaan berikutnya memakai kredensial baru.
            let cached = self.db_access.lock().ok().and_then(|g| g.clone());
            let result = async {
                let access = match cached {
                    Some(a) => a,
                    None => self.load_db_access(token, database_id).await?,
                };
                if let Ok(mut g) = self.db_access.lock() {
                    *g = Some(access.clone());
                }
                self.backend.db_metrics(&access).await
            }
            .await;
            match result {
                Ok(text) => texts.push(text),
                Err(e) => {
                    if let Ok(mut g) = self.db_access.lock() {
                        *g = None;
                    }
                    tracing::warn!("Gagal membaca metrik database: {e}");
                }
            }
        }
        let Some(last) = texts.last() else {
            return ResourceUsage::EMPTY;
        };

        let busy: Vec<f64> = texts
            .iter()
            .filter_map(|t| prom_value(t, "cpu_usage_idle", &[("cpu", "cpu-total")]))
            .map(|idle| 100.0 - idle)
            .collect();
        ResourceUsage {
            cpu_percent: (!busy.is_empty())
                .then(|| round2(busy.iter().sum::<f64>() / busy.len() as f64)),
            mem_percent: prom_value(last, "mem_used_percent", &[]).map(round2),
            disk_percent: prom_value(last, "disk_used_percent", &[]).map(round2),
        }
    }

    /// Node: src/monitoring/digitalocean-metrics.client.ts:259 `loadDbAccess`.
    async fn load_db_access(
        &self,
        token: &str,
        database_id: &str,
    ) -> anyhow::Result<DbMetricsAccess> {
        let id = encode_uri_component(database_id);
        let (cluster_path, ca_path) = (format!("/databases/{id}"), format!("/databases/{id}/ca"));
        let (cluster, ca, creds) = tokio::try_join!(
            self.backend.do_get(token, &cluster_path, &[]),
            self.backend.do_get(token, &ca_path, &[]),
            self.backend
                .do_get(token, "/databases/metrics/credentials", &[]),
        )?;

        let endpoint = cluster
            .pointer("/database/metrics_endpoints")
            .and_then(Value::as_array)
            .and_then(|a| a.first());
        let host = truthy_text(endpoint.and_then(|e| e.get("host")));
        let port = truthy_text(endpoint.and_then(|e| e.get("port")));
        let certificate = truthy_text(ca.pointer("/ca/certificate"));
        let username = truthy_text(creds.pointer("/credentials/basic_auth_username"));
        let password = creds
            .pointer("/credentials/basic_auth_password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (Some(host), Some(port), Some(certificate), Some(username)) =
            (host, port, certificate, username)
        else {
            anyhow::bail!("metrics endpoint, CA, atau kredensial tidak lengkap");
        };

        // `Buffer.from(certificate, 'base64')` mengabaikan karakter non-base64 (mis. newline).
        let cleaned: String = certificate
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_'))
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(cleaned.replace('-', "+").replace('_', "/"))?;
        Ok(DbMetricsAccess {
            url: format!("https://{host}:{port}/metrics"),
            username,
            password,
            ca_pem: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }
}

/// Port `digitalocean-metrics.client.spec.ts`.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::test_support::FakeBackend;
    use super::*;
    use serde_json::json;

    fn series(metric: &[(&str, &str)], values: &[(i64, &str)]) -> MatrixSeries {
        MatrixSeries {
            metric: metric
                .iter()
                .map(|(k, v)| ((*k).to_owned(), json!(v)))
                .collect(),
            values: values
                .iter()
                .map(|(t, v)| vec![json!(t), json!(v)])
                .collect(),
        }
    }

    #[test]
    fn cpu_real_droplet_two_vcpu() {
        let modes: [(&str, f64, f64); 8] = [
            ("idle", 43_658_726.38, 43_658_901.95),
            ("iowait", 3436.04, 3436.08),
            ("irq", 0.0, 0.0),
            ("nice", 3361.34, 3361.34),
            ("softirq", 31_567.75, 31_569.63),
            ("steal", 149_359.93, 149_361.11),
            ("system", 168_143.07, 168_148.77),
            ("user", 958_642.61, 958_695.51),
        ];
        let result: Vec<MatrixSeries> = modes
            .iter()
            .map(|(mode, a, b)| {
                series(
                    &[("mode", mode)],
                    &[(1, &a.to_string()), (2, &b.to_string())],
                )
            })
            .collect();
        let cpu = cpu_percent_from_counters(Some(&result)).unwrap();
        assert!((cpu - 26.0).abs() < 0.5, "{cpu}");
    }

    #[test]
    fn cpu_uses_last_two_samples() {
        let result = [
            series(&[("mode", "idle")], &[(1, "0"), (2, "100"), (3, "110")]),
            series(&[("mode", "user")], &[(1, "0"), (2, "0"), (3, "90")]),
        ];
        assert_eq!(cpu_percent_from_counters(Some(&result)), Some(90.0));
    }

    #[test]
    fn cpu_none_cases() {
        let one = [series(&[("mode", "idle")], &[(1, "5")])];
        let down = [series(&[("mode", "idle")], &[(1, "100"), (2, "50")])];
        let flat = [series(&[("mode", "idle")], &[(1, "100"), (2, "100")])];
        assert_eq!(cpu_percent_from_counters(Some(&[])), None, "kosong");
        assert_eq!(cpu_percent_from_counters(None), None, "undefined");
        assert_eq!(cpu_percent_from_counters(Some(&one)), None, "satu sampel");
        assert_eq!(
            cpu_percent_from_counters(Some(&down)),
            None,
            "counter turun"
        );
        assert_eq!(cpu_percent_from_counters(Some(&flat)), None, "counter diam");
    }

    #[test]
    fn last_value_and_used_percent() {
        let fs = [
            series(
                &[("mountpoint", "/boot"), ("device", "/dev/vda15")],
                &[(1, "1")],
            ),
            series(
                &[("mountpoint", "/"), ("device", "/dev/vda1")],
                &[(1, "1"), (2, "47442784256")],
            ),
        ];
        assert_eq!(
            last_value(Some(&fs), &[("mountpoint", "/")]),
            Some(47_442_784_256.0)
        );
        let disk = used_percent(Some(47_442_784_256.0), Some(61_285_326_848.0)).unwrap();
        assert!((disk - 22.59).abs() < 0.005, "{disk}");
        assert_eq!(used_percent(None, Some(100.0)), None);
        assert_eq!(used_percent(Some(50.0), None), None);
        assert_eq!(used_percent(Some(50.0), Some(0.0)), None);
    }

    #[test]
    fn matrix_result_parses_do_response() {
        let body = json!({
            "status": "success",
            "data": {
                "resultType": "matrix",
                "result": [
                    { "metric": { "host_id": "549306589", "mode": "idle" },
                      "values": [[1_700_000_000, "10"], [1_700_000_120, "20"]] }
                ]
            }
        });
        let r = matrix_result(&body).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(last_value(Some(&r), &[("mode", "idle")]), Some(20.0));
        assert!(matrix_result(&json!({})).is_none());
    }

    const PROM: &str = "# TYPE cpu_usage_idle gauge\n\
        cpu_usage_idle{cpu=\"cpu-total\",host=\"h\"} 75.5\n\
        cpu_usage_idle{cpu=\"cpu0\",host=\"h\"} 10\n\
        cpu_usage_idle_extra{cpu=\"cpu-total\"} 1\n\
        mem_used_percent{host=\"h\"} 67.8\n\
        mem_used_percent{host=\"h2\"} 32.2\n\
        disk_used_percent 14.9";

    #[test]
    fn prom_value_cases() {
        assert_eq!(
            prom_value(PROM, "cpu_usage_idle", &[("cpu", "cpu-total")]),
            Some(75.5)
        );
        assert_eq!(
            prom_value(PROM, "cpu_usage_idle", &[("cpu", "cpu0")]),
            Some(10.0)
        );
        assert_eq!(prom_value(PROM, "mem_used_percent", &[]), Some(50.0));
        assert_eq!(prom_value(PROM, "disk_used_percent", &[]), Some(14.9));
        assert_eq!(prom_value(PROM, "nope", &[]), None);
    }

    #[test]
    fn round2_like_math_round() {
        assert_eq!(round2(27.5), 27.5);
        assert_eq!(round2(68.04), 68.04);
        assert_eq!(round2(-0.005), -0.0);
    }

    #[test]
    fn encode_uri_component_like_js() {
        assert_eq!(encode_uri_component("db-uuid"), "db-uuid");
        assert_eq!(encode_uri_component("a/b c"), "a%2Fb%20c");
    }

    fn env_all() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DO_TOKEN", "token"),
            ("DO_DROPLET_ID", "549306589"),
            ("DO_DATABASE_ID", "db-uuid"),
        ]
    }

    #[tokio::test]
    async fn missing_env_returns_empty_without_calls() {
        for key in ["DO_TOKEN", "DO_DROPLET_ID", "DO_DATABASE_ID"] {
            let env: Vec<_> = env_all().into_iter().filter(|(k, _)| *k != key).collect();
            let fake = Arc::new(FakeBackend::new(&env));
            let client = DigitalOceanMetrics::new(fake.clone());
            if key != "DO_DATABASE_ID" {
                assert_eq!(client.droplet_usage().await, ResourceUsage::EMPTY, "{key}");
            }
            if key != "DO_DROPLET_ID" {
                assert_eq!(
                    client.database_usage(1, Duration::ZERO).await,
                    ResourceUsage::EMPTY,
                    "{key}"
                );
            }
            assert!(fake.do_calls().is_empty(), "{key}");
        }
    }

    #[tokio::test]
    async fn droplet_one_metric_failing_keeps_others() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        let m = |metric: &str, mp: Option<&str>, v: &str| {
            let labels = mp.map(|m| json!({ "mountpoint": m })).unwrap_or(json!({}));
            fake.on_do(
                &format!("/monitoring/metrics/droplet/{metric}"),
                Ok(json!({ "data": { "result": [{ "metric": labels, "values": [[1, v]] }] } })),
            );
        };
        m("memory_available", None, "25");
        m("memory_total", None, "100");
        m("filesystem_free", Some("/"), "60");
        m("filesystem_size", Some("/"), "80");
        fake.on_do("/monitoring/metrics/droplet/cpu", Err("503".into()));

        let usage = DigitalOceanMetrics::new(fake.clone()).droplet_usage().await;
        assert_eq!(
            usage,
            ResourceUsage {
                cpu_percent: None,
                mem_percent: Some(75.0),
                disk_percent: Some(25.0),
            }
        );
        let calls = fake.do_calls();
        let (_, query) = calls
            .iter()
            .find(|(p, _)| p == "/monitoring/metrics/droplet/filesystem_size")
            .unwrap();
        assert!(query.contains(&("host_id".to_owned(), "549306589".to_owned())));
        let start: i64 = query
            .iter()
            .find(|(k, _)| k == "start")
            .unwrap()
            .1
            .parse()
            .unwrap();
        let end: i64 = query
            .iter()
            .find(|(k, _)| k == "end")
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert_eq!(end - start, 600);
    }

    fn stub_db_access(fake: &FakeBackend) {
        fake.on_do(
            "/databases/db-uuid",
            Ok(json!({ "database": { "metrics_endpoints": [{ "host": "db.host", "port": 9273 }] } })),
        );
        fake.on_do(
            "/databases/db-uuid/ca",
            Ok(json!({ "ca": { "certificate": "UEVN" } })),
        );
        fake.on_do(
            "/databases/metrics/credentials",
            Ok(json!({ "credentials": { "basic_auth_username": "u", "basic_auth_password": "p" } })),
        );
    }

    #[tokio::test]
    async fn database_prometheus_with_basic_auth_and_ca() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        fake.push_metrics(Ok(
            "cpu_usage_idle{cpu=\"cpu-total\"} 72.5\nmem_used_percent{host=\"h\"} 68.04\ndisk_used_percent{path=\"/\"} 14.9\n",
        ));
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(1, Duration::ZERO)
            .await;
        assert_eq!(
            usage,
            ResourceUsage {
                cpu_percent: Some(27.5),
                mem_percent: Some(68.04),
                disk_percent: Some(14.9),
            }
        );
        assert_eq!(
            fake.metrics_calls(),
            vec![DbMetricsAccess {
                url: "https://db.host:9273/metrics".into(),
                username: "u".into(),
                password: "p".into(),
                ca_pem: "PEM".into(),
            }]
        );
    }

    #[tokio::test]
    async fn database_access_cached_dropped_after_failure() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        fake.push_metrics(Ok("mem_used_percent 1"));
        fake.push_metrics(Ok("mem_used_percent 2"));
        fake.push_metrics(Err("401"));
        fake.push_metrics(Ok("mem_used_percent 3"));
        let client = DigitalOceanMetrics::new(fake.clone());

        client.database_usage(1, Duration::ZERO).await;
        client.database_usage(1, Duration::ZERO).await;
        assert_eq!(fake.do_calls().len(), 3);

        assert_eq!(
            client.database_usage(1, Duration::ZERO).await,
            ResourceUsage::EMPTY
        );
        assert_eq!(
            client.database_usage(1, Duration::ZERO).await.mem_percent,
            Some(3.0)
        );
        assert_eq!(fake.do_calls().len(), 6);
    }

    #[tokio::test]
    async fn database_failed_read_mid_run_reloads_access() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        fake.push_metrics(Err("401"));
        fake.push_metrics(Ok("mem_used_percent 5"));
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(2, Duration::ZERO)
            .await;
        assert_eq!(usage.mem_percent, Some(5.0));
        assert_eq!(fake.do_calls().len(), 6);
    }

    #[tokio::test]
    async fn database_incomplete_credentials_is_empty() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        fake.on_do("/databases/db-uuid", Ok(json!({})));
        fake.on_do("/databases/db-uuid/ca", Ok(json!({})));
        fake.on_do("/databases/metrics/credentials", Ok(json!({})));
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(1, Duration::ZERO)
            .await;
        assert_eq!(usage, ResourceUsage::EMPTY);
        assert!(fake.metrics_calls().is_empty());
    }

    #[tokio::test]
    async fn database_cpu_averaged_mem_disk_from_last() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        // Satu lonjakan 92% di antara bacaan normal tidak boleh jadi angka laporan.
        fake.push_metrics(Ok(
            "cpu_usage_idle{cpu=\"cpu-total\"} 8\nmem_used_percent 70",
        ));
        fake.push_metrics(Ok("cpu_usage_idle{cpu=\"cpu-total\"} 82"));
        fake.push_metrics(Ok("cpu_usage_idle{cpu=\"cpu-total\"} 80"));
        fake.push_metrics(Ok(
            "cpu_usage_idle{cpu=\"cpu-total\"} 82\nmem_used_percent 73.6\ndisk_used_percent 15",
        ));
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(DB_CPU_SAMPLES, Duration::ZERO)
            .await;
        assert_eq!(
            usage,
            ResourceUsage {
                cpu_percent: Some(37.0),
                mem_percent: Some(73.6),
                disk_percent: Some(15.0),
            }
        );
        assert_eq!(fake.metrics_calls().len(), 4);
    }

    #[tokio::test]
    async fn database_failed_sample_skipped() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        fake.push_metrics(Ok("cpu_usage_idle{cpu=\"cpu-total\"} 80"));
        fake.push_metrics(Err("timeout"));
        fake.push_metrics(Ok(
            "cpu_usage_idle{cpu=\"cpu-total\"} 90\nmem_used_percent 60",
        ));
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(3, Duration::ZERO)
            .await;
        assert_eq!(
            usage,
            ResourceUsage {
                cpu_percent: Some(15.0),
                mem_percent: Some(60.0),
                disk_percent: None,
            }
        );
    }

    #[tokio::test]
    async fn database_samples_spaced_by_gap() {
        let fake = Arc::new(FakeBackend::new(&env_all()));
        stub_db_access(&fake);
        fake.push_metrics(Ok("cpu_usage_idle{cpu=\"cpu-total\"} 80"));
        fake.push_metrics(Ok("cpu_usage_idle{cpu=\"cpu-total\"} 80"));
        let started = std::time::Instant::now();
        let usage = DigitalOceanMetrics::new(fake.clone())
            .database_usage(2, Duration::from_millis(50))
            .await;
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(usage.cpu_percent, Some(20.0));
        assert_eq!(fake.metrics_calls().len(), 2);
    }
}
