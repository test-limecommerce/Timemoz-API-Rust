//! Format berkas kasus (`tools/parity/cases/*.json`) dan templating fixture.
//!
//! ```json
//! {
//!   "suite": "trackings",
//!   "mutates": false,
//!   "cases": [
//!     { "name": "list sebagai admin", "actor": "admin", "method": "GET",
//!       "path": "/trackings", "query": { "takenDate": "{{today}}" } },
//!     { "name": "tanpa token", "method": "GET", "path": "/trackings" },
//!     { "name": "buat", "actor": "staff", "method": "POST", "path": "/trackings",
//!       "body": { "userId": "{{num:staff.id}}" }, "ignore": ["/data/createdAt"] }
//!   ]
//! }
//! ```
//!
//! Placeholder:
//! - `{{nama.kolom}}` — nilai fixture sebagai string (di path, query, atau string body);
//! - `"{{num:nama.kolom}}"` — termasuk tanda kutip → diganti angka/JSON mentah;
//! - `{{today}}`, `{{now}}` — tanggal UTC hari ini (`YYYY-MM-DD`) / ISO sekarang;
//! - `{{cap.nama}}` — nilai yang di-`capture` langkah sebelumnya, per server;
//! - `{{run}}` — sufiks unik per run (untuk username/kode yang harus unik).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub suite: String,
    /// `true` = kasus menulis DB. Suite ini dijalankan SESUDAH semua suite baca,
    /// berurutan, dan tidak diulang tanpa `reset.sh`.
    #[serde(default)]
    pub mutates: bool,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    /// Nama aktor di `fixtures.json` (bagian `actors`). Kosong = tanpa token.
    #[serde(default)]
    pub actor: Option<String>,
    /// Token mentah (mis. `{{cap.token}}` dari respons login sebelumnya). Mengalahkan `actor`.
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub path: String,
    /// Kasus SQL: query yang SAMA dijalankan ke DB Node & DB Rust lalu barisnya
    /// dibandingkan (efek samping: audit log, seat, blacklist, ...). Semua kolom `::text`.
    /// Kalau diisi, `method/path` diabaikan.
    #[serde(default)]
    pub sql: Option<String>,
    /// Simpan nilai dari respons untuk langkah berikutnya: `{ "nama": "/data/uuid" }` →
    /// `{{cap.nama}}`. Disimpan TERPISAH per server (uuid Node ≠ uuid Rust).
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<Value>,
    /// Body mentah (mis. JSON rusak). Mengalahkan `body`.
    #[serde(default)]
    pub raw_body: Option<String>,
    /// JSON pointer yang diabaikan saat membandingkan. `*` = semua indeks/kunci.
    /// Awalan `header:` = abaikan header respons itu (mis. `header:location`).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// JSON pointer array yang urutannya tidak penting.
    #[serde(default)]
    pub unordered: Vec<String>,
    /// Header respons tambahan yang wajib sama.
    #[serde(default)]
    pub compare_headers: Vec<String>,
    /// Lewati kasus (dengan alasan) tanpa menghapusnya.
    #[serde(default)]
    pub skip: Option<String>,
    /// Selisih yang DISENGAJA (mis. bug Node yang tidak ditiru). Kasus tetap dijalankan dan
    /// diff tetap dilaporkan, tapi dihitung DEVIATION, bukan FAIL. Wajib merujuk `plan/deviations`.
    #[serde(default)]
    pub deviation: Option<String>,
}

pub fn load_suites(dir: &Path) -> anyhow::Result<Vec<Suite>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("membaca {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut suites = Vec::new();
    for f in files {
        let raw = std::fs::read_to_string(&f)?;
        let suite: Suite = serde_json::from_str(&raw)
            .with_context(|| format!("format kasus salah di {}", f.display()))?;
        suites.push(suite);
    }
    Ok(suites)
}

/// Nilai fixture yang sudah di-resolve: `nama.kolom` → nilai teks.
pub type Vars = BTreeMap<String, String>;

/// Ganti placeholder di satu kasus. Placeholder tak dikenal = error (bukan
/// diam-diam dikirim apa adanya, yang akan membuat kedua server sama-sama 404).
pub fn render(case: &Case, vars: &Vars) -> anyhow::Result<Case> {
    let raw = serde_json::to_string(&SerCase::from(case))?;
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(start) = rest.find("{{") {
        let (before, after) = rest.split_at(start);
        let Some(end) = after.find("}}") else {
            bail!("placeholder tidak ditutup di kasus `{}`", case.name)
        };
        let key = after.get(2..end).unwrap_or_default();
        let after_ph = after.get(end + 2..).unwrap_or_default();
        if let Some(num_key) = key.strip_prefix("num:") {
            // `"{{num:x}}"` → buang kutip pembuka (sudah di `before`) & penutup.
            let value = lookup(vars, num_key, &case.name)?;
            let trimmed = before.strip_suffix('"').unwrap_or(before);
            out.push_str(trimmed);
            out.push_str(value);
            rest = after_ph.strip_prefix('"').unwrap_or(after_ph);
            continue;
        }
        out.push_str(before);
        let value = lookup(vars, key, &case.name)?;
        // Nilai masuk ke dalam string JSON: escape seperlunya.
        let escaped = serde_json::to_string(value)?;
        out.push_str(
            escaped
                .get(1..escaped.len().saturating_sub(1))
                .unwrap_or_default(),
        );
        rest = after_ph;
    }
    out.push_str(rest);
    let ser: SerCase =
        serde_json::from_str(&out).with_context(|| format!("render kasus `{}`", case.name))?;
    Ok(ser.into())
}

fn lookup<'a>(vars: &'a Vars, key: &str, case: &str) -> anyhow::Result<&'a str> {
    vars.get(key)
        .map(String::as_str)
        .with_context(|| format!("placeholder `{{{{{key}}}}}` tidak dikenal di kasus `{case}`"))
}

/// Salinan `Case` yang bisa diserialisasi (untuk templating).
#[derive(serde::Serialize, Deserialize)]
struct SerCase {
    name: String,
    actor: Option<String>,
    auth_token: Option<String>,
    method: String,
    path: String,
    sql: Option<String>,
    capture: BTreeMap<String, String>,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Option<Value>,
    raw_body: Option<String>,
    ignore: Vec<String>,
    unordered: Vec<String>,
    compare_headers: Vec<String>,
    skip: Option<String>,
    deviation: Option<String>,
}

impl From<&Case> for SerCase {
    fn from(c: &Case) -> Self {
        Self {
            name: c.name.clone(),
            actor: c.actor.clone(),
            auth_token: c.auth_token.clone(),
            method: c.method.clone(),
            path: c.path.clone(),
            sql: c.sql.clone(),
            capture: c.capture.clone(),
            query: c.query.clone(),
            headers: c.headers.clone(),
            body: c.body.clone(),
            raw_body: c.raw_body.clone(),
            ignore: c.ignore.clone(),
            unordered: c.unordered.clone(),
            compare_headers: c.compare_headers.clone(),
            skip: c.skip.clone(),
            deviation: c.deviation.clone(),
        }
    }
}

impl From<SerCase> for Case {
    fn from(c: SerCase) -> Self {
        Self {
            name: c.name,
            actor: c.actor,
            auth_token: c.auth_token,
            method: c.method,
            path: c.path,
            sql: c.sql,
            capture: c.capture,
            query: c.query,
            headers: c.headers,
            body: c.body,
            raw_body: c.raw_body,
            ignore: c.ignore,
            unordered: c.unordered,
            compare_headers: c.compare_headers,
            skip: c.skip,
            deviation: c.deviation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn case(v: Value) -> Case {
        serde_json::from_value(v).unwrap_or_else(|e| unreachable!("{e}"))
    }

    #[test]
    fn renders_string_and_number_placeholders() {
        let mut vars = Vars::new();
        vars.insert("admin.uuid".into(), "abc-1".into());
        vars.insert("admin.id".into(), "42".into());
        let c = case(json!({
            "name": "x", "method": "GET", "path": "/users/{{admin.uuid}}",
            "body": { "id": "{{num:admin.id}}", "label": "u-{{admin.id}}" }
        }));
        let r = render(&c, &vars).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(r.path, "/users/abc-1");
        assert_eq!(r.body, Some(json!({ "id": 42, "label": "u-42" })));
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let c = case(json!({ "name": "x", "method": "GET", "path": "/{{nope.id}}" }));
        assert!(render(&c, &Vars::new()).is_err());
    }
}
