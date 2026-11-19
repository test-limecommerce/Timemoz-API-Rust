//! `mz-parity` — putar request yang sama ke Mudah-API (Node) dan mudah-rs (Rust),
//! lalu bandingkan status, header penting, dan body JSON.
//!
//! Dijalankan lewat `tools/parity/run.sh` (yang menyiapkan env). Konfigurasi env:
//!
//! | Env | Bawaan |
//! |---|---|
//! | `PARITY_NODE_URL` | `http://127.0.0.1:$PARITY_NODE_PORT` (3000) |
//! | `PARITY_RS_URL` | `http://127.0.0.1:$PARITY_RS_PORT` (3100) |
//! | `PARITY_FIXTURE_DB_URL` | wajib — DB milik Node (sebelum request apa pun) |
//! | `PARITY_JWT_SECRET` | wajib — `SECRET_TOKEN` yang dipakai kedua server |
//! | `PARITY_DIR` | `tools/parity` |
//! | `PARITY_OUT` | `.parity` |
//!
//! Argumen: `[--filter <teks>] [--only-read]`. Exit 1 bila ada kasus GAGAL.

mod case;
mod diff;
mod fixtures;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::case::{Case, Suite, Vars};
use crate::diff::{Diff, Opts, Severity};

const DEFAULT_COMPARED_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "cache-control",
    "location",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum Verdict {
    Pass,
    Warn,
    /// Selisih yang ditandai `deviation` di kasus (disengaja & tercatat).
    Deviation,
    Fail,
    Skip,
    Error,
}

#[derive(Debug, Serialize)]
struct Outcome {
    suite: String,
    case: String,
    verdict: Verdict,
    method: String,
    path: String,
    node_status: Option<u16>,
    rust_status: Option<u16>,
    node_ms: Option<u128>,
    rust_ms: Option<u128>,
    notes: Vec<String>,
    #[serde(skip)]
    diffs: Vec<Diff>,
    diff_lines: Vec<String>,
}

struct Captured {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
    ms: u128,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn mint_token(secret: &str, user_id: &str) -> anyhow::Result<String> {
    let id: i64 = user_id
        .parse()
        .with_context(|| format!("id aktor bukan angka: {user_id}"))?;
    let claims = json!({ "userId": id, "iat": chrono::Utc::now().timestamp() });
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

/// Token operator seperti `OperatorAuthService.login`: `{ username, sub }` (+ `iat`).
fn mint_operator_token(secret: &str, id: &str, username: &str) -> anyhow::Result<String> {
    let id: i64 = id
        .parse()
        .with_context(|| format!("id operator bukan angka: {id}"))?;
    let claims = json!({ "username": username, "sub": id, "iat": chrono::Utc::now().timestamp() });
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

async fn send(
    http: &reqwest::Client,
    base: &str,
    case: &Case,
    token: Option<&str>,
) -> anyhow::Result<Captured> {
    let method = reqwest::Method::from_bytes(case.method.to_uppercase().as_bytes())?;
    let mut req = http.request(
        method,
        format!("{}{}", base.trim_end_matches('/'), case.path),
    );
    if !case.query.is_empty() {
        req = req.query(&case.query);
    }
    for (k, v) in &case.headers {
        req = req.header(k, v);
    }
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(raw) = &case.raw_body {
        req = req
            .header("content-type", "application/json")
            .body(raw.clone());
    } else if let Some(body) = &case.body {
        req = req.json(body);
    }
    let started = Instant::now();
    let res = req.send().await?;
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.bytes().await?.to_vec();
    Ok(Captured {
        status,
        headers,
        body,
        ms: started.elapsed().as_millis(),
    })
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| {
        // `application/json; charset=utf-8` vs `application/json;charset=utf-8`.
        s.split(';')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("; ")
            .to_lowercase()
    })
}

fn compare_case(
    case: &Case,
    node: &Captured,
    rust: &Captured,
    window: (i64, i64),
) -> (Verdict, Vec<Diff>, Vec<String>) {
    let mut diffs = Vec::new();
    let mut notes = Vec::new();
    if node.status != rust.status {
        diffs.push(Diff {
            path: "status".into(),
            severity: Severity::Fail,
            node: node.status.to_string(),
            rust: rust.status.to_string(),
        });
    }
    let extra: Vec<&str> = case.compare_headers.iter().map(String::as_str).collect();
    for name in DEFAULT_COMPARED_HEADERS.iter().copied().chain(extra) {
        if case
            .ignore
            .iter()
            .any(|i| i.strip_prefix("header:") == Some(name))
        {
            continue;
        }
        let (a, b) = (
            header_str(&node.headers, name),
            header_str(&rust.headers, name),
        );
        if a != b {
            diffs.push(Diff {
                path: format!("header:{name}"),
                severity: Severity::Fail,
                node: a.unwrap_or_else(|| "<tidak ada>".into()),
                rust: b.unwrap_or_else(|| "<tidak ada>".into()),
            });
        }
    }
    let ct = header_str(&node.headers, "content-type").unwrap_or_default();
    match (
        serde_json::from_slice::<Value>(&node.body),
        serde_json::from_slice::<Value>(&rust.body),
    ) {
        (Ok(a), Ok(b)) => {
            let opts = Opts {
                ignore: &case.ignore,
                unordered: &case.unordered,
                window_ms: window,
            };
            diffs.extend(diff::compare(&a, &b, &opts));
        }
        _ if ct.contains("application/pdf") => {
            let ok = node.body.starts_with(b"%PDF") && rust.body.starts_with(b"%PDF");
            notes.push(format!(
                "PDF: isi tidak dibandingkan byte-per-byte (node {} B, rust {} B)",
                node.body.len(),
                rust.body.len()
            ));
            if !ok {
                diffs.push(Diff {
                    path: "body".into(),
                    severity: Severity::Fail,
                    node: "bukan PDF?".into(),
                    rust: "bukan PDF?".into(),
                });
            }
        }
        _ => {
            let (ha, hb) = (
                hex::encode(Sha256::digest(&node.body)),
                hex::encode(Sha256::digest(&rust.body)),
            );
            if ha != hb {
                let snip = |b: &[u8]| {
                    String::from_utf8_lossy(b.get(..b.len().min(200)).unwrap_or_default())
                        .into_owned()
                };
                diffs.push(Diff {
                    path: "body(raw)".into(),
                    severity: Severity::Fail,
                    node: snip(&node.body),
                    rust: snip(&rust.body),
                });
            }
        }
    }
    (verdict_of(&diffs), diffs, notes)
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn write_report(out_dir: &PathBuf, outcomes: &[Outcome], started: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let count = |v: Verdict| outcomes.iter().filter(|o| o.verdict == v).count();
    let mut md = String::new();
    md.push_str(&format!(
        "# Laporan paritas Node vs Rust\n\nDijalankan: {started}\n\n"
    ));
    md.push_str(&format!(
        "| PASS | WARN | FAIL | ERROR | SKIP | Total | DEVIATION |\n|---|---|---|---|---|---|---|\n| {} | {} | {} | {} | {} | {} | {} |\n\n",
        count(Verdict::Pass),
        count(Verdict::Warn),
        count(Verdict::Fail),
        count(Verdict::Error),
        count(Verdict::Skip),
        outcomes.len(),
        count(Verdict::Deviation)
    ));
    let mut node_ms: Vec<u128> = outcomes.iter().filter_map(|o| o.node_ms).collect();
    let mut rust_ms: Vec<u128> = outcomes.iter().filter_map(|o| o.rust_ms).collect();
    node_ms.sort_unstable();
    rust_ms.sort_unstable();
    let pct = |v: &[u128], p: usize| {
        v.get(v.len().saturating_sub(1) * p / 100)
            .copied()
            .unwrap_or(0)
    };
    md.push_str(&format!(
        "Latensi (satu request berurutan, BUKAN uji beban): Node p50 {} ms / p95 {} ms — Rust p50 {} ms / p95 {} ms\n\n",
        pct(&node_ms, 50),
        pct(&node_ms, 95),
        pct(&rust_ms, 50),
        pct(&rust_ms, 95)
    ));
    let mut by_suite: BTreeMap<&str, Vec<&Outcome>> = BTreeMap::new();
    for o in outcomes {
        by_suite.entry(o.suite.as_str()).or_default().push(o);
    }
    for (suite, list) in by_suite {
        md.push_str(&format!("## {suite}\n\n| Kasus | Hasil | Request | Status N/R | ms N/R |\n|---|---|---|---|---|\n"));
        for o in &list {
            md.push_str(&format!(
                "| {} | {:?} | `{} {}` | {}/{} | {}/{} |\n",
                md_escape(&o.case),
                o.verdict,
                o.method,
                md_escape(&o.path),
                o.node_status.map_or("-".into(), |s| s.to_string()),
                o.rust_status.map_or("-".into(), |s| s.to_string()),
                o.node_ms.map_or("-".into(), |s| s.to_string()),
                o.rust_ms.map_or("-".into(), |s| s.to_string()),
            ));
        }
        for o in list
            .iter()
            .filter(|o| !o.diff_lines.is_empty() || !o.notes.is_empty())
        {
            md.push_str(&format!(
                "\n**{}** ({:?})\n\n",
                md_escape(&o.case),
                o.verdict
            ));
            for n in &o.notes {
                md.push_str(&format!("- catatan: {}\n", md_escape(n)));
            }
            for l in &o.diff_lines {
                md.push_str(&format!("- {}\n", md_escape(l)));
            }
        }
        md.push('\n');
    }
    let md_path = out_dir.join("report.md");
    std::fs::write(&md_path, md)?;
    std::fs::write(
        out_dir.join("report.json"),
        serde_json::to_vec_pretty(outcomes)?,
    )?;
    Ok(md_path)
}

fn verdict_of(diffs: &[Diff]) -> Verdict {
    if diffs.iter().any(|d| d.severity == Severity::Fail) {
        Verdict::Fail
    } else if diffs.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Warn
    }
}

fn diff_lines(diffs: &[Diff]) -> Vec<String> {
    let mut out: Vec<String> = diffs
        .iter()
        .take(40)
        .map(|d| {
            format!(
                "{:?} `{}`: node={} rust={}",
                d.severity, d.path, d.node, d.rust
            )
        })
        .collect();
    if diffs.len() > 40 {
        out.push(format!("… {} selisih lain", diffs.len() - 40));
    }
    out
}

/// Nilai JSON pointer dari body respons sebagai teks (string apa adanya, lainnya `to_string`).
fn capture_value(c: &Captured, pointer: &str) -> Option<String> {
    let v: Value = serde_json::from_slice(&c.body).ok()?;
    match v.pointer(pointer)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Jalankan query SQL kasus; semua kolom wajib `::text`. Hasil: array objek.
async fn query_rows(db: &sqlx::PgPool, sql: &str) -> anyhow::Result<Value> {
    use sqlx::{Column, Row};
    let rows = sqlx::query(sql).fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::new();
        for col in row.columns() {
            let v: Option<String> = row
                .try_get(col.ordinal())
                .with_context(|| format!("kolom `{}` harus `::text`", col.name()))?;
            obj.insert(col.name().to_owned(), v.map_or(Value::Null, Value::String));
        }
        out.push(Value::Object(obj));
    }
    Ok(Value::Array(out))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let filter = args
        .iter()
        .position(|a| a == "--filter")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let only_read = args.iter().any(|a| a == "--only-read");

    let node_url = env_or(
        "PARITY_NODE_URL",
        &format!("http://127.0.0.1:{}", env_or("PARITY_NODE_PORT", "3000")),
    );
    let rust_url = env_or(
        "PARITY_RS_URL",
        &format!("http://127.0.0.1:{}", env_or("PARITY_RS_PORT", "3100")),
    );
    let db_url =
        std::env::var("PARITY_FIXTURE_DB_URL").context("PARITY_FIXTURE_DB_URL wajib diisi")?;
    let secret = std::env::var("PARITY_JWT_SECRET").context("PARITY_JWT_SECRET wajib diisi")?;
    let dir = PathBuf::from(env_or("PARITY_DIR", "tools/parity"));
    let out = PathBuf::from(env_or("PARITY_OUT", ".parity"));

    let fx_file = fixtures::load_all(&dir)?;
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&db_url)
        .await?;
    let mut vars = Vars::new();
    let now = chrono::Utc::now();
    vars.insert("today".into(), now.format("%Y-%m-%d").to_string());
    vars.insert(
        "now".into(),
        now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    let missing = fixtures::resolve(&db, &fx_file, &mut vars).await?;
    db.close().await;
    for m in &missing {
        eprintln!("peringatan: fixture opsional `{m}` kosong di dump");
    }

    // Nilai `{{run}}`: unik per run, untuk data yang harus unik (username, kode).
    vars.insert("run".into(), now.format("%m%d%H%M%S").to_string());
    let mut vars_node = vars.clone();
    let mut vars_rust = vars.clone();

    let node_db_url = env_or("PARITY_NODE_DB_URL", &db_url);
    let rust_db_url =
        std::env::var("PARITY_RS_DB_URL").unwrap_or_else(|_| db_url.replace("_node", "_rs"));
    let db_node = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&node_db_url)?;
    let db_rust = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&rust_db_url)?;

    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    for (actor, var) in &fx_file.actors {
        if let Some(id) = vars.get(var).filter(|v| !v.is_empty()) {
            tokens.insert(actor.clone(), mint_token(&secret, id)?);
        }
    }
    for (actor, fixture) in &fx_file.operators {
        let id = vars.get(&format!("{fixture}.id")).filter(|v| !v.is_empty());
        let username = vars
            .get(&format!("{fixture}.username"))
            .cloned()
            .unwrap_or_default();
        if let Some(id) = id {
            tokens.insert(actor.clone(), mint_operator_token(&secret, id, &username)?);
        }
    }

    let mut suites = case::load_suites(&dir.join("cases"))?;
    // Suite baca dulu, lalu suite yang menulis (berurutan, satu kali).
    suites.sort_by_key(|s: &Suite| s.mutates);
    if only_read {
        suites.retain(|s| !s.mutates);
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let started = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let window_start = now.timestamp_millis();
    let mut outcomes = Vec::new();

    for suite in &suites {
        for raw in &suite.cases {
            let label = format!("{}/{}", suite.suite, raw.name);
            if filter.as_deref().is_some_and(|f| !label.contains(f)) {
                continue;
            }
            let mut o = Outcome {
                suite: suite.suite.clone(),
                case: raw.name.clone(),
                verdict: Verdict::Skip,
                method: raw.method.clone(),
                path: raw.path.clone(),
                node_status: None,
                rust_status: None,
                node_ms: None,
                rust_ms: None,
                notes: Vec::new(),
                diffs: Vec::new(),
                diff_lines: Vec::new(),
            };
            if let Some(reason) = &raw.skip {
                o.notes.push(format!("dilewati: {reason}"));
                outcomes.push(o);
                continue;
            }
            let (case_n, case_r) =
                match (case::render(raw, &vars_node), case::render(raw, &vars_rust)) {
                    (Ok(n), Ok(r)) => (n, r),
                    (Err(e), _) | (_, Err(e)) => {
                        o.verdict = Verdict::Error;
                        o.notes.push(format!("{e:#}"));
                        outcomes.push(o);
                        continue;
                    }
                };
            o.path = case_n.path.clone();
            let window = || (window_start, chrono::Utc::now().timestamp_millis());

            // ── Kasus SQL: efek samping di DB kedua sisi ──────────────────────
            if let (Some(sql_n), Some(sql_r)) = (&case_n.sql, &case_r.sql) {
                o.method = "SQL".into();
                o.path = sql_n.chars().take(80).collect();
                match (
                    query_rows(&db_node, sql_n).await,
                    query_rows(&db_rust, sql_r).await,
                ) {
                    (Ok(a), Ok(b)) => {
                        let opts = Opts {
                            ignore: &case_n.ignore,
                            unordered: &case_n.unordered,
                            window_ms: window(),
                        };
                        let diffs = diff::compare(&a, &b, &opts);
                        o.verdict = verdict_of(&diffs);
                        o.diff_lines = diff_lines(&diffs);
                        o.diffs = diffs;
                    }
                    (a, b) => {
                        o.verdict = Verdict::Error;
                        for (side, r) in [("node", a), ("rust", b)] {
                            if let Err(e) = r {
                                o.notes.push(format!("{side}: {e:#}"));
                            }
                        }
                    }
                }
                // `deviation` juga berlaku untuk kasus SQL (sebelumnya terlewat karena `continue`).
                if o.verdict == Verdict::Fail
                    && let Some(reason) = &raw.deviation
                {
                    o.verdict = Verdict::Deviation;
                    o.notes.push(format!("deviasi disengaja: {reason}"));
                }
                println!("{:<5} {label}", format!("{:?}", o.verdict).to_uppercase());
                outcomes.push(o);
                continue;
            }

            // ── Kasus HTTP ────────────────────────────────────────────────────
            let token_for =
                |c: &Case| -> Result<Option<String>, String> {
                    if let Some(t) = c.auth_token.as_deref().filter(|t| !t.is_empty()) {
                        return Ok(Some(t.to_owned()));
                    }
                    match c.actor.as_deref() {
                        Some(a) => tokens.get(a).cloned().map(Some).ok_or_else(|| {
                            format!("aktor `{a}` tidak terdefinisi / kosong di dump")
                        }),
                        None => Ok(None),
                    }
                };
            let (tok_n, tok_r) = match (token_for(&case_n), token_for(&case_r)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    o.verdict = Verdict::Error;
                    o.notes.push(e);
                    outcomes.push(o);
                    continue;
                }
            };
            let node = send(&http, &node_url, &case_n, tok_n.as_deref()).await;
            let rust = send(&http, &rust_url, &case_r, tok_r.as_deref()).await;
            match (node, rust) {
                (Ok(n), Ok(r)) => {
                    o.node_status = Some(n.status);
                    o.rust_status = Some(r.status);
                    o.node_ms = Some(n.ms);
                    o.rust_ms = Some(r.ms);
                    let (verdict, diffs, notes) = compare_case(&case_n, &n, &r, window());
                    o.verdict = verdict;
                    o.notes.extend(notes);
                    o.diff_lines = diff_lines(&diffs);
                    o.diffs = diffs;
                    // Capture per server untuk langkah berikutnya.
                    for (name, pointer) in &case_n.capture {
                        for (side, cap, vars) in
                            [("node", &n, &mut vars_node), ("rust", &r, &mut vars_rust)]
                        {
                            match capture_value(cap, pointer) {
                                Some(v) => {
                                    vars.insert(format!("cap.{name}"), v);
                                }
                                None => o.notes.push(format!(
                                    "capture `{name}` ({pointer}) tidak ada di respons {side}"
                                )),
                            }
                        }
                    }
                }
                (n, r) => {
                    o.verdict = Verdict::Error;
                    if let Err(e) = n {
                        o.notes.push(format!("node: {e}"));
                    }
                    if let Err(e) = r {
                        o.notes.push(format!("rust: {e}"));
                    }
                }
            }
            if o.verdict == Verdict::Fail
                && let Some(reason) = &raw.deviation
            {
                o.verdict = Verdict::Deviation;
                o.notes.push(format!("deviasi disengaja: {reason}"));
            }
            println!("{:<5} {label}", format!("{:?}", o.verdict).to_uppercase());
            outcomes.push(o);
        }
    }

    let report = write_report(&out, &outcomes, &started)?;
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Fail | Verdict::Error))
        .count();
    println!("\nLaporan: {}", report.display());
    if failed > 0 {
        println!("{failed} kasus GAGAL/ERROR");
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Semua file kasus & fixture di repo harus valid terhadap format runner
    /// (tanpa server/DB), dan setiap aktor yang dipakai kasus harus terdefinisi.
    #[test]
    fn repo_cases_and_fixtures_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/parity");
        let fx = crate::fixtures::load_all(&dir).unwrap_or_else(|e| unreachable!("{e:#}"));
        let suites =
            crate::case::load_suites(&dir.join("cases")).unwrap_or_else(|e| unreachable!("{e:#}"));
        assert!(!suites.is_empty());
        let fixture_names: Vec<&str> = fx.fixtures.iter().map(|f| f.name.as_str()).collect();
        for s in &suites {
            for c in &s.cases {
                if let Some(a) = &c.actor {
                    assert!(
                        fx.actors.contains_key(a) || fx.operators.contains_key(a),
                        "aktor `{a}` di `{}` tidak ada",
                        c.name
                    );
                }
                // Placeholder fixture harus merujuk fixture yang ada.
                let raw = serde_json::to_string(&(
                    &c.path,
                    &c.query,
                    &c.body,
                    &c.headers,
                    &c.auth_token,
                    &c.sql,
                ))
                .unwrap_or_default();
                assert!(
                    c.sql.is_some() || (!c.method.is_empty() && !c.path.is_empty()),
                    "kasus `{}` butuh method+path atau sql",
                    c.name
                );
                for part in raw.split("{{").skip(1) {
                    let key = part
                        .split("}}")
                        .next()
                        .unwrap_or_default()
                        .trim_start_matches("num:");
                    let head = key.split('.').next().unwrap_or_default();
                    assert!(
                        key == "today"
                            || key == "now"
                            || key == "run"
                            || head == "cap"
                            || fixture_names.contains(&head),
                        "placeholder `{key}` di kasus `{}` tidak punya fixture",
                        c.name
                    );
                }
            }
        }
    }
}
