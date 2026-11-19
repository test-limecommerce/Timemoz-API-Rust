//! Port `src/admin-jobs/company-data/restore.ts`: restore hasil backup ke database, tanpa
//! bentrok id (mode `clone`: id baru dari sequence tabel tujuan + uuid baru, setiap kolom
//! yang menunjuk id/uuid lama ditulis ulang).
//!
//! Hanya jalur yang dipakai admin job yang di-port (mode `clone`, tanpa `exclude`,
//! `onMissingExternal = fail`, batch 1000, batas transaksi 30 menit, satu transaksi,
//! tanpa prompt). Mode `exact` dan opsi CLI lain tetap milik skrip Node.
//!
//! Tulisan ke model ber-audit dicatat seperti middleware Prisma Node di worker Bull (tanpa
//! konteks request): satu baris `auditLog` ringkasan per `createMany`, ditulis langsung
//! lewat pool — jadi juga tetap ada sesudah dry run di-rollback, sama dengan Node.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Touched};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};

use super::backup::{BACKUP_FORMAT_VERSION, pad_end, pad_start};
use super::errors::{CdResult, Code, Log, fail};
use super::pg::{Catalog, encode_for_insert, key_of, q, text_of};
use super::scope::{
    KNOWN_PARTIAL_UNIQUE_INDEXES, ModelGraph, ModelInfo, PARTIAL_UNIQUE_INDEXES, PartialAction,
    Policy, Row, describe_database, embedded_graph, policy, regenerate, regenerate_on_clone,
    schema_fingerprint, user_scoped_risk,
};

pub const RESTORE_BATCH: usize = 1000;
pub const RESTORE_TIMEOUT_MINUTES: u64 = 30;

/// `--users`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsersMode {
    Reuse,
    Insert,
}

impl UsersMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reuse => "reuse",
            Self::Insert => "insert",
        }
    }
}

/// `RestoreParams` (jalur admin job).
#[derive(Debug, Clone)]
pub struct RestoreParams {
    pub dir: PathBuf,
    pub dry_run: bool,
    pub users: UsersMode,
    pub username_suffix: Option<String>,
    pub rename: Option<String>,
    /// URL database tujuan (deteksi restore ke database asal).
    pub database_url: Option<String>,
    /// Kunci advisory yang ditahan sepanjang transaksi restore (`admin-job:<companyId>`):
    /// selama restore Rust berjalan, klaim job baru untuk company yang sama (Node maupun Rust)
    /// tertahan — lihat `plan/deviations/mz-admin-jobs.md`.
    pub lock_key: Option<String>,
}

/// `validateRestoreParams` (bagian yang relevan untuk admin job).
pub fn validate(params: &RestoreParams) -> CdResult<()> {
    if params.dir.as_os_str().is_empty() {
        return fail(
            Code::InvalidParams,
            "folder backup wajib diisi. Lihat --help.",
        );
    }
    if params.users == UsersMode::Insert
        && params.username_suffix.as_deref().is_none_or(str::is_empty)
    {
        return fail(
            Code::InvalidParams,
            "--users insert butuh --username-suffix supaya username tidak bentrok.",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pembacaan berkas backup
// ---------------------------------------------------------------------------

/// `loadManifest(dir)`.
pub fn load_manifest(dir: &Path) -> CdResult<Value> {
    let file = dir.join("manifest.json");
    if !file.exists() {
        return fail(
            Code::ManifestInvalid,
            format!("manifest.json tidak ditemukan di {}", dir.display()),
        );
    }
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
    let version = manifest.get("formatVersion");
    if version.and_then(Value::as_i64) != Some(BACKUP_FORMAT_VERSION) {
        let shown = version.map_or_else(|| "undefined".to_owned(), Value::to_string);
        return fail(
            Code::ManifestInvalid,
            format!(
                "Format backup versi {shown}, script ini hanya membaca versi {BACKUP_FORMAT_VERSION}."
            ),
        );
    }
    Ok(manifest)
}

/// `resolveBackupFile(dir, file)`: path berkas data, dijamin tetap di dalam folder backup.
pub fn resolve_backup_file(dir: &Path, file: &str) -> CdResult<PathBuf> {
    let bad = || {
        fail::<PathBuf>(
            Code::ManifestInvalid,
            format!("Path berkas di manifest tidak valid: {file}"),
        )
    };
    if file.is_empty() {
        return bad();
    }
    let rel = Path::new(file);
    if rel.is_absolute() {
        return bad();
    }
    let mut depth: i64 = 0;
    for c in rel.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return bad();
                }
            }
            _ => return bad(),
        }
    }
    if depth <= 0 {
        return bad();
    }
    Ok(dir.join(rel))
}

fn open_data(path: &Path) -> CdResult<Box<dyn Read + Send>> {
    let f = File::open(path)?;
    Ok(if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::GzDecoder::new(f))
    } else {
        Box::new(f)
    })
}

/// `verifyChecksum(filePath, expected)`: sha256 isi NDJSON sebelum gzip.
pub fn verify_checksum(path: &Path, expected: &str) -> CdResult<(String, bool)> {
    let mut reader = open_data(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(buf.get(..n).unwrap_or_default());
    }
    let actual = hex::encode(hash.finalize());
    let ok = actual == expected;
    Ok((actual, ok))
}

/// `readBatches(filePath, size)`: baris kosong dilewati.
struct Batches {
    lines: std::io::Lines<BufReader<Box<dyn Read + Send>>>,
    size: usize,
}

impl Batches {
    fn open(path: &Path, size: usize) -> CdResult<Self> {
        Ok(Self {
            lines: BufReader::new(open_data(path)?).lines(),
            size,
        })
    }

    fn next_batch(&mut self) -> CdResult<Option<Vec<Row>>> {
        let mut batch = Vec::new();
        for line in self.lines.by_ref() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            batch.push(serde_json::from_str::<Row>(&line)?);
            if batch.len() >= self.size {
                break;
            }
        }
        Ok(if batch.is_empty() { None } else { Some(batch) })
    }
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// `RestoreTableStat`.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStat {
    pub inserted: u64,
    pub skipped: u64,
    pub severed: u64,
    pub dangling: u64,
    pub deferred: u64,
    pub reused: u64,
    pub unique_nulled: u64,
    pub unique_regenerated: u64,
    pub unique_suffixed: u64,
    pub partial_resolved: u64,
}

struct PendingUuidRef {
    model: String,
    new_id: Value,
    column: String,
    old_uuid: String,
    target: String,
}

struct Pair {
    row: Row,
    out: Row,
}

type IdMap = HashMap<String, HashMap<String, Value>>;

struct Restorer<'a> {
    models: &'a ModelGraph,
    catalog: &'a Catalog,
    manifest: &'a Value,
    params: &'a RestoreParams,
    pool: PgPool,
    audit: AuditHandle,
    id_map: IdMap,
    uuid_map: HashMap<String, HashMap<String, String>>,
    external_id_map: IdMap,
    pending: Vec<PendingUuidRef>,
    stats: IndexMap<String, TableStat>,
    new_company_id: Option<Value>,
    reused_users: u64,
}

fn shown(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

impl Restorer<'_> {
    fn info(&self, name: &str) -> CdResult<&ModelInfo> {
        match self.models.get(name) {
            Some(i) => Ok(i),
            None => fail(
                Code::SchemaMismatch,
                format!("Model {name} ada di backup tapi tidak ada di schema."),
            ),
        }
    }

    fn map_id(&self, target: &str, old: &Value) -> Option<Value> {
        let k = key_of(old);
        self.id_map
            .get(target)
            .and_then(|m| m.get(&k))
            .or_else(|| self.external_id_map.get(target).and_then(|m| m.get(&k)))
            .cloned()
    }

    /// `allocateIds(info, count)`.
    async fn allocate_ids(
        &self,
        tx: &mut PgConnection,
        info: &ModelInfo,
        count: usize,
    ) -> CdResult<Vec<Value>> {
        if count == 0 {
            return Ok(vec![]);
        }
        let pk = info.pk.clone().unwrap_or_default();
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence($1, $2))::bigint AS id FROM generate_series(1, $3)",
        )
        .bind(q(&info.table))
        .bind(&pk)
        .bind(i32::try_from(count).unwrap_or(i32::MAX))
        .persistent(false)
        .fetch_all(&mut *tx)
        .await?;
        if rows.len() != count {
            return fail(
                Code::IdAllocationFailed,
                format!(
                    "Gagal menerbitkan id baru untuk {} (minta {count}, dapat {}).",
                    info.name,
                    rows.len()
                ),
            );
        }
        Ok(rows.into_iter().map(Value::from).collect())
    }

    /// `resolveExternals()`.
    async fn resolve_externals(&mut self, tx: &mut PgConnection) -> CdResult<()> {
        let Some(externals) = self.manifest.get("externals").and_then(Value::as_object) else {
            return Ok(());
        };
        for (model, spec) in externals {
            let info = self.info(model)?;
            let natural: Vec<String> = spec
                .get("naturalKey")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let matcher: Map<String, Value> = spec
                .get("match")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut map = HashMap::new();
            let mut missing = Vec::new();
            let rows = spec
                .get("rows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for row in &rows {
                let mut conds = Vec::new();
                let mut binds: Vec<Option<String>> = Vec::new();
                for (k, v) in &matcher {
                    let ty = self.catalog.sql_type(info, k)?;
                    if v.is_null() {
                        conds.push(format!("t.{} IS NULL", q(k)));
                    } else {
                        binds.push(text_of(v));
                        conds.push(format!("t.{} = ${}::text::{ty}", q(k), binds.len()));
                    }
                }
                for k in &natural {
                    let ty = self.catalog.sql_type(info, k)?;
                    binds.push(row.get(k).and_then(text_of));
                    conds.push(format!(
                        "t.{} IS NOT DISTINCT FROM ${}::text::{ty}",
                        q(k),
                        binds.len()
                    ));
                }
                let sql = format!(
                    "SELECT to_jsonb(t.\"id\") FROM {} t WHERE {} LIMIT 1",
                    q(&info.table),
                    if conds.is_empty() {
                        "TRUE".to_owned()
                    } else {
                        conds.join(" AND ")
                    }
                );
                let mut query = sqlx::query_scalar::<_, Value>(&sql).persistent(false);
                for b in binds {
                    query = query.bind(b);
                }
                match query.fetch_optional(&mut *tx).await? {
                    Some(found) => {
                        map.insert(key_of(row.get("id").unwrap_or(&Value::Null)), found);
                    }
                    None => {
                        let mut parts: Vec<String> = natural
                            .iter()
                            .map(|k| format!("{k}={}", shown(row.get(k))))
                            .collect();
                        parts.extend(
                            matcher
                                .iter()
                                .map(|(k, v)| format!("{k}={}", shown(Some(v)))),
                        );
                        missing.push(format!("{model}({})", parts.join(", ")));
                    }
                }
            }
            self.external_id_map.insert(model.clone(), map);
            if !missing.is_empty() {
                return fail(
                    Code::MissingExternal,
                    format!(
                        "Baris global berikut tidak ada di database tujuan:\n    {}\n  Seed dulu tabel referensinya, atau jalankan ulang dengan --on-missing-external null.",
                        missing.join("\n    ")
                    ),
                );
            }
        }
        Ok(())
    }

    /// `premapExistingUsers(rows)`.
    async fn premap_existing_users(
        &mut self,
        tx: &mut PgConnection,
        rows: &[Row],
    ) -> CdResult<std::collections::HashSet<String>> {
        let mut reused = std::collections::HashSet::new();
        if self.params.users != UsersMode::Reuse || rows.is_empty() {
            return Ok(reused);
        }
        let usernames: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get("username").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let existing: Vec<(i32, uuid::Uuid, String)> =
            sqlx::query_as(r#"SELECT id, uuid, username FROM users WHERE username = ANY($1)"#)
                .bind(&usernames)
                .persistent(false)
                .fetch_all(&mut *tx)
                .await?;
        let by_username: HashMap<&str, &(i32, uuid::Uuid, String)> =
            existing.iter().map(|u| (u.2.as_str(), u)).collect();
        for row in rows {
            let Some(name) = row.get("username").and_then(Value::as_str) else {
                continue;
            };
            if let Some(hit) = by_username.get(name) {
                let old_id = row.get("id").cloned().unwrap_or(Value::Null);
                self.id_map
                    .entry("users".into())
                    .or_default()
                    .insert(key_of(&old_id), Value::from(hit.0));
                if let Some(old_uuid) = row
                    .get("uuid")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.uuid_map
                        .entry("users".into())
                        .or_default()
                        .insert(old_uuid.to_owned(), hit.1.to_string());
                }
                reused.insert(key_of(&old_id));
                self.reused_users += 1;
            }
        }
        Ok(reused)
    }

    /// `transformRow(info, row, stat)`. `None` = baris dilewati.
    fn transform_row(&self, info: &ModelInfo, row: &Row, stat: &mut TableStat) -> Option<Row> {
        let mut out = Row::new();
        for (field, meta) in &info.field_meta {
            let Some(v) = row.get(field) else {
                continue;
            };
            let value = if v.is_null() {
                Value::Null
            } else if meta.is_list && !v.is_array() {
                Value::Array(vec![])
            } else {
                v.clone()
            };
            out.insert(field.clone(), value);
        }

        for fk in &info.fks {
            if !out.contains_key(&fk.column) {
                continue;
            }
            let old = match row.get(&fk.column) {
                None | Some(Value::Null) => {
                    out.insert(fk.column.clone(), Value::Null);
                    continue;
                }
                Some(v) => v,
            };
            if let Some(mapped) = self.map_id(&fk.target, old) {
                out.insert(fk.column.clone(), mapped);
                continue;
            }
            if fk.nullable {
                out.insert(fk.column.clone(), Value::Null);
                stat.severed += 1;
            } else {
                stat.skipped += 1;
                return None;
            }
        }

        for lf in &info.list_fks {
            let Some(Value::Array(values)) = row.get(&lf.column) else {
                continue;
            };
            if values.is_empty() || !(lf.when)(row) {
                continue;
            }
            let mapped: Vec<Value> = values
                .iter()
                .filter_map(|v| self.map_id(&lf.target, v))
                .collect();
            if mapped.len() != values.len() {
                stat.severed += 1;
            }
            out.insert(lf.column.clone(), Value::Array(mapped));
        }

        if let Some(poly) = info.polymorphic {
            let target = row
                .get(poly.discriminator)
                .and_then(Value::as_str)
                .and_then(|d| poly.target(d));
            match row.get(poly.id_column) {
                None | Some(Value::Null) => {}
                Some(old) => {
                    if let Some(mapped) = target.and_then(|t| self.map_id(t, old)) {
                        out.insert(poly.id_column.to_owned(), mapped);
                    } else {
                        out.insert(poly.id_column.to_owned(), Value::from(0));
                        stat.dangling += 1;
                    }
                }
            }
            if let Some(old_uuid) = row
                .get(poly.uuid_column)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let new = target
                    .and_then(|t| self.uuid_map.get(t))
                    .and_then(|m| m.get(old_uuid))
                    .cloned()
                    .unwrap_or_else(|| old_uuid.to_owned());
                out.insert(poly.uuid_column.to_owned(), Value::String(new));
            }
        }
        Some(out)
    }

    /// `remapUuidRefs(info, row, out, newId, stat)`.
    fn remap_uuid_refs(
        &mut self,
        info: &ModelInfo,
        row: &Row,
        out: &mut Row,
        new_id: Option<&Value>,
        stat: &mut TableStat,
    ) -> CdResult<()> {
        for (column, target) in info.uuid_refs {
            let Some(old) = row
                .get(*column)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let mapped = self.uuid_map.get(*target).and_then(|m| m.get(old)).cloned();
            if let Some(m) = mapped {
                out.insert((*column).to_owned(), Value::String(m));
                continue;
            }
            let Some(new_id) = new_id else {
                return fail(
                    Code::ScopeConfig,
                    format!(
                        "{}.{column} menyimpan uuid model lain, tapi modelnya ber-primary key gabungan. Pass penambal uuid belum mendukung bentuk itu.",
                        info.name
                    ),
                );
            };
            out.insert((*column).to_owned(), Value::Null);
            self.pending.push(PendingUuidRef {
                model: info.name.clone(),
                new_id: new_id.clone(),
                column: (*column).to_owned(),
                old_uuid: old.to_owned(),
                target: (*target).to_owned(),
            });
            stat.deferred += 1;
        }
        Ok(())
    }

    /// `resolveUniques(info, pairs, stat)`.
    async fn resolve_uniques(
        &self,
        tx: &mut PgConnection,
        info: &ModelInfo,
        pairs: Vec<Pair>,
        stat: &mut TableStat,
    ) -> CdResult<Vec<Pair>> {
        let columns = &info.unique_columns;
        if columns.is_empty() || pairs.is_empty() {
            return Ok(pairs);
        }
        let mut taken: HashMap<&str, std::collections::HashSet<String>> = columns
            .iter()
            .map(|c| (c.as_str(), Default::default()))
            .collect();
        for col in columns {
            let values: Vec<String> = pairs
                .iter()
                .filter_map(|p| p.out.get(col).and_then(text_of))
                .collect();
            if values.is_empty() {
                continue;
            }
            let ty = self.catalog.sql_type(info, col)?;
            let sql = format!(
                "SELECT t.{c}::text FROM {tbl} t WHERE t.{c} = ANY($1::text[]::{ty}[])",
                c = q(col),
                tbl = q(&info.table)
            );
            let existing: Vec<Option<String>> = sqlx::query_scalar(&sql)
                .bind(values)
                .persistent(false)
                .fetch_all(&mut *tx)
                .await?;
            if let Some(set) = taken.get_mut(col.as_str()) {
                set.extend(existing.into_iter().flatten());
            }
        }

        let mut kept = Vec::with_capacity(pairs.len());
        for mut pair in pairs {
            let mut keep = true;
            for col in columns {
                let Some(value) = pair.out.get(col).filter(|v| !v.is_null()).cloned() else {
                    continue;
                };
                let text = text_of(&value).unwrap_or_default();
                let Some(set) = taken.get_mut(col.as_str()) else {
                    continue;
                };
                if !set.contains(&text) {
                    set.insert(text);
                    continue;
                }
                match policy(&info.name, col) {
                    Policy::Null => {
                        if info.field_meta.get(col).is_some_and(|m| m.is_required) {
                            return fail(
                                Code::UniqueConflict,
                                format!(
                                    "Kolom wajib {}.{col} tidak bisa dinolkan saat bentrok.",
                                    info.name
                                ),
                            );
                        }
                        pair.out.insert(col.clone(), Value::Null);
                        stat.unique_nulled += 1;
                    }
                    Policy::Regenerate => {
                        let mut candidate = regenerate(&info.name, col);
                        while set.contains(&candidate) {
                            candidate = regenerate(&info.name, col);
                        }
                        set.insert(candidate.clone());
                        pair.out.insert(col.clone(), Value::String(candidate));
                        stat.unique_regenerated += 1;
                    }
                    Policy::Suffix => {
                        let base = shown(Some(&value));
                        let mut n = 1;
                        let mut candidate = format!("{base}-r{n}");
                        while set.contains(&candidate) {
                            n += 1;
                            candidate = format!("{base}-r{n}");
                        }
                        set.insert(candidate.clone());
                        pair.out.insert(col.clone(), Value::String(candidate));
                        stat.unique_suffixed += 1;
                    }
                    Policy::Skip => {
                        keep = false;
                        stat.skipped += 1;
                        break;
                    }
                    Policy::Reuse => {
                        return fail(
                            Code::UniqueConflict,
                            format!(
                                "{}.{col} = {value} sudah ada di database tujuan. Pilih --username-suffix lain, atau jalankan dengan --users reuse.",
                                info.name
                            ),
                        );
                    }
                    Policy::Fail => {
                        return fail(
                            Code::UniqueConflict,
                            format!(
                                "Nilai unik {}.{col} = {value} sudah ada di database tujuan dan belum punya kebijakan penyelesaian. Tambahkan di UNIQUE_CONFLICT_POLICY (src/admin-jobs/company-data/company-scope.ts) atau kecualikan modelnya dengan --exclude.",
                                info.name
                            ),
                        );
                    }
                }
            }
            if keep {
                kept.push(pair);
            }
        }
        Ok(kept)
    }

    /// `resolvePartialUniques(info, pairs, stat)`. Semua spec di schema ini berkunci satu kolom.
    async fn resolve_partial_uniques(
        &self,
        tx: &mut PgConnection,
        info: &ModelInfo,
        pairs: &mut [Pair],
        stat: &mut TableStat,
    ) -> CdResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        for spec in PARTIAL_UNIQUE_INDEXES
            .iter()
            .filter(|s| s.model == info.name)
        {
            let idx: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter(|(_, p)| spec.applies(&p.out))
                .map(|(i, _)| i)
                .collect();
            if idx.is_empty() {
                continue;
            }
            let Some(col) = spec.keys.first() else {
                continue;
            };
            let mut values: Vec<String> = Vec::new();
            for i in &idx {
                if let Some(t) = pairs
                    .get(*i)
                    .and_then(|p| p.out.get(*col))
                    .and_then(text_of)
                    && !values.contains(&t)
                {
                    values.push(t);
                }
            }
            let ty = self.catalog.sql_type(info, col)?;
            let where_text = match spec.where_value {
                super::scope::Value0::Bool(b) => b.to_string(),
                super::scope::Value0::Text(t) => t.to_owned(),
            };
            let sql = format!(
                "SELECT t.{c}::text FROM {tbl} t WHERE t.{w}::text = $1 AND t.{c} = ANY($2::text[]::{ty}[])",
                c = q(col),
                w = q(spec.where_column),
                tbl = q(&info.table)
            );
            let existing: Vec<Option<String>> = sqlx::query_scalar(&sql)
                .bind(where_text)
                .bind(values)
                .persistent(false)
                .fetch_all(&mut *tx)
                .await?;
            let mut taken: std::collections::HashSet<String> =
                existing.into_iter().flatten().collect();
            for i in idx {
                let Some(pair) = pairs.get_mut(i) else {
                    continue;
                };
                let key = pair
                    .out
                    .get(*col)
                    .map_or_else(|| "undefined".to_owned(), |v| shown(Some(v)));
                if !taken.contains(&key) {
                    taken.insert(key);
                    continue;
                }
                match &spec.on_conflict {
                    PartialAction::Set { column, value } => {
                        pair.out.insert((*column).to_owned(), Value::Bool(*value));
                        stat.partial_resolved += 1;
                    }
                    PartialAction::Fail { message } => {
                        return fail(
                            Code::PartialUniqueConflict,
                            format!(
                                "Partial unique index \"{}\" ({}) bentrok.\n  {message}",
                                spec.name, info.name
                            ),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// `createMany({ data })` lewat `jsonb_to_recordset`, dikelompokkan per himpunan kunci.
    async fn insert_rows(
        &self,
        tx: &mut PgConnection,
        info: &ModelInfo,
        data: &[Row],
    ) -> CdResult<()> {
        let mut groups: IndexMap<Vec<String>, Vec<Value>> = IndexMap::new();
        for row in data {
            let cols: Vec<String> = row.keys().cloned().collect();
            let encoded: Map<String, Value> = row
                .iter()
                .map(|(k, v)| {
                    let v = match info.field_meta.get(k) {
                        Some(meta) => encode_for_insert(meta, v),
                        None => v.clone(),
                    };
                    (k.clone(), v)
                })
                .collect();
            groups.entry(cols).or_default().push(Value::Object(encoded));
        }
        for (cols, rows) in groups {
            let (defs, exprs) = self.catalog.insert_parts(info, &cols)?;
            let col_list = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "INSERT INTO {} ({col_list}) SELECT {exprs} FROM jsonb_to_recordset($1::jsonb) AS x({defs})",
                q(&info.table)
            );
            let res = sqlx::query(&sql)
                .bind(Value::Array(rows))
                .persistent(false)
                .execute(&mut *tx)
                .await;
            if let Err(err) = res {
                return Err(self.explain_write_error(err, &info.name).await);
            }
        }
        Ok(())
    }

    /// `explainWriteError(err, model)`.
    async fn explain_write_error(&self, err: sqlx::Error, model: &str) -> anyhow::Error {
        let constraint = match &err {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                db.constraint().map(str::to_owned)
            }
            _ => return err.into(),
        };
        // Prisma `meta.target` = kolom index unik; dibaca lewat pool (transaksi sudah gagal).
        let target = match &constraint {
            Some(name) => sqlx::query_scalar::<_, Option<String>>(
                r#"SELECT string_agg(a.attname::text, ', ' ORDER BY k.ord)
                     FROM pg_class c
                     JOIN pg_index i ON i.indexrelid = c.oid
                     JOIN unnest(i.indkey) WITH ORDINALITY k(attnum, ord) ON true
                     JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                    WHERE c.relname = $1"#,
            )
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| name.clone()),
            None => "undefined".to_owned(),
        };
        let hint = if user_scoped_risk(model) {
            format!(
                "Kunci ini memuat userId tapi tidak memuat companyId, jadi ia bentrok ketika user lama\n  dipakai ulang. Pilihan: --users insert --username-suffix \".copy\", atau --exclude {model}."
            )
        } else {
            format!(
                "Tambahkan kebijakan di UNIQUE_CONFLICT_POLICY (src/admin-jobs/company-data/company-scope.ts)\n  atau jalankan dengan --exclude {model}."
            )
        };
        super::errors::CompanyDataError::new(
            Code::UniqueConflict,
            format!("Tabrakan unique saat menulis {model} pada kolom [{target}]\n  {hint}"),
        )
        .into()
    }

    /// Audit `createMany` seperti middleware Node (pemicunya `createMany` pada model ber-audit).
    async fn audit_create_many(
        &self,
        name: &str,
        data: &[Row],
        phase_pre: Option<mz_audit::MembershipPre>,
    ) {
        let Some(model) = Model::from_name(name) else {
            return;
        };
        if let Some(pre) = phase_pre {
            self.audit.record_membership(pre).await;
            return;
        }
        let rows: Vec<mz_audit::Row> = data
            .iter()
            .filter_map(|r| mz_audit::Row::from_json(&Value::Object(audit_values(r))))
            .collect();
        self.audit
            .record_create_many(model, &rows, u64::try_from(data.len()).unwrap_or(0))
            .await;
    }

    async fn membership_pre(&self, name: &str, data: &[Row]) -> Option<mz_audit::MembershipPre> {
        // Tabel keanggotaan menurut spesifikasi audit (`mz_audit::Model::is_membership_companion`).
        let model = Model::from_name(name).filter(|m| m.is_membership_companion())?;
        let rows: Vec<mz_audit::Row> = data
            .iter()
            .filter_map(|r| mz_audit::Row::from_json(&Value::Object(audit_values(r))))
            .collect();
        Some(
            self.audit
                .membership_pre(model, MemberEffect::Add, Touched::Rows(rows))
                .await,
        )
    }

    /// `restoreModel(name, dir)`.
    async fn restore_model(&mut self, tx: &mut PgConnection, name: &str) -> CdResult<TableStat> {
        let models = self.models;
        let info = match models.get(name) {
            Some(i) => i,
            None => {
                return fail(
                    Code::SchemaMismatch,
                    format!("Model {name} ada di backup tapi tidak ada di schema."),
                );
            }
        };
        let mut stat = TableStat::default();
        self.id_map.entry(name.to_owned()).or_default();
        self.uuid_map.entry(name.to_owned()).or_default();
        let table = self.manifest.get("tables").and_then(|t| t.get(name));
        let rows_expected = table
            .and_then(|t| t.get("rows"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let Some(file) = table
            .filter(|_| rows_expected > 0)
            .and_then(|t| t.get("file"))
            .and_then(Value::as_str)
        else {
            self.stats.insert(name.to_owned(), stat.clone());
            return Ok(stat);
        };
        let path = resolve_backup_file(&self.params.dir, file)?;
        let regen_cols = regenerate_on_clone(name);
        let pk_is_fk = info
            .fks
            .iter()
            .find(|f| info.pk.as_deref() == Some(f.column.as_str()))
            .cloned();

        let mut batches = Batches::open(&path, RESTORE_BATCH)?;
        while let Some(batch) = batches.next_batch()? {
            let mut rows = batch;
            if name == "users" {
                let reused = self.premap_existing_users(tx, &rows).await?;
                stat.reused += u64::try_from(reused.len()).unwrap_or(0);
                rows.retain(|r| !reused.contains(&key_of(r.get("id").unwrap_or(&Value::Null))));
                if let Some(suffix) = &self.params.username_suffix
                    && !suffix.is_empty()
                {
                    for r in &mut rows {
                        let base = shown(r.get("username"));
                        r.insert("username".into(), Value::String(format!("{base}{suffix}")));
                    }
                }
            }
            for col in regen_cols {
                for row in &mut rows {
                    if row.get(*col).is_some_and(|v| !v.is_null()) {
                        row.insert(
                            (*col).to_owned(),
                            Value::String(uuid::Uuid::new_v4().to_string()),
                        );
                    }
                }
            }

            // 1. Petakan ulang setiap rujukan id.
            let mut pairs = Vec::new();
            for row in rows {
                if let Some(out) = self.transform_row(info, &row, &mut stat) {
                    pairs.push(Pair { row, out });
                }
            }
            // 2. Unique diperiksa sesudah id dipetakan.
            let mut pairs = self.resolve_uniques(tx, info, pairs, &mut stat).await?;
            self.resolve_partial_uniques(tx, info, &mut pairs, &mut stat)
                .await?;
            if pairs.is_empty() {
                continue;
            }

            // 3. Id baru dari sequence tabel tujuan.
            let new_ids: Option<Vec<Value>> = if info.pk.is_none() {
                None
            } else if let Some(fk) = &pk_is_fk {
                Some(
                    pairs
                        .iter()
                        .map(|p| p.out.get(&fk.column).cloned().unwrap_or(Value::Null))
                        .collect(),
                )
            } else {
                Some(self.allocate_ids(tx, info, pairs.len()).await?)
            };

            let mut data = Vec::with_capacity(pairs.len());
            for (i, mut pair) in pairs.into_iter().enumerate() {
                let new_id = new_ids.as_ref().and_then(|ids| ids.get(i)).cloned();
                if let (Some(pk), Some(id)) = (&info.pk, &new_id) {
                    pair.out.insert(pk.clone(), id.clone());
                    let old = pair.row.get(pk).cloned().unwrap_or(Value::Null);
                    self.id_map
                        .entry(name.to_owned())
                        .or_default()
                        .insert(key_of(&old), id.clone());
                }
                if info.uuid_col {
                    let fresh = uuid::Uuid::new_v4().to_string();
                    pair.out.insert("uuid".into(), Value::String(fresh.clone()));
                    let old = shown(pair.row.get("uuid"));
                    self.uuid_map
                        .entry(name.to_owned())
                        .or_default()
                        .insert(old, fresh);
                }
                self.remap_uuid_refs(info, &pair.row, &mut pair.out, new_id.as_ref(), &mut stat)?;
                data.push(pair.out);
            }

            if name == "company"
                && let Some(rename) = &self.params.rename
            {
                for d in &mut data {
                    d.insert("name".into(), Value::String(rename.clone()));
                }
            }

            let pre = self.membership_pre(name, &data).await;
            self.insert_rows(tx, info, &data).await?;
            self.audit_create_many(name, &data, pre).await;
            stat.inserted += u64::try_from(data.len()).unwrap_or(0);
            if name == "company" {
                self.new_company_id = new_ids.and_then(|ids| ids.into_iter().next());
            }
        }
        self.stats.insert(name.to_owned(), stat.clone());
        Ok(stat)
    }

    /// `flushPendingUuidRefs()`.
    async fn flush_pending_uuid_refs(&mut self, tx: &mut PgConnection) -> CdResult<u64> {
        let mut patched = 0;
        let pending = std::mem::take(&mut self.pending);
        for p in &pending {
            let Some(mapped) = self
                .uuid_map
                .get(&p.target)
                .and_then(|m| m.get(&p.old_uuid))
                .cloned()
            else {
                continue;
            };
            let info = self.info(&p.model)?;
            let pk = info.pk.clone().unwrap_or_default();
            let col_ty = self.catalog.sql_type(info, &p.column)?;
            let pk_ty = self.catalog.sql_type(info, &pk)?;
            let audited = Model::from_name(&p.model);
            let pre = match (
                audited,
                p.new_id.as_i64().and_then(|n| i32::try_from(n).ok()),
            ) {
                (Some(m), Some(id)) => Some((m, id, self.audit.pre_read(m, Lookup::Id(id)).await)),
                _ => None,
            };
            let sql = format!(
                "UPDATE {} SET {} = $1::text::{col_ty} WHERE {} = $2::text::{pk_ty}",
                q(&info.table),
                q(&p.column),
                q(&pk)
            );
            let res = sqlx::query(&sql)
                .bind(&mapped)
                .bind(text_of(&p.new_id))
                .persistent(false)
                .execute(&mut *tx)
                .await?;
            if res.rows_affected() == 0 {
                anyhow::bail!(
                    "An operation failed because it depends on one or more records that were required but not found. Record to update not found."
                );
            }
            if let Some((m, id, pre)) = pre
                && let Some(after) = self.audit.result_row(&mut *tx, m, Lookup::Id(id)).await
            {
                self.audit.record_update(m, pre, &after).await;
            }
            patched += 1;
        }
        Ok(patched)
    }
}

/// Nilai `data` `createMany` dalam bentuk yang dibaca middleware audit Node
/// (`BigInt` → string seperti `mz_audit::Row`).
fn audit_values(row: &Row) -> Map<String, Value> {
    row.iter()
        .map(|(k, v)| {
            let v = match v.get("$bigint").and_then(Value::as_str) {
                Some(s) => Value::String(s.to_owned()),
                None => v.clone(),
            };
            (k.clone(), v)
        })
        .collect()
}

/// `warnUnknownPartialIndexes(prisma, models, order, log)`.
async fn warn_unknown_partial_indexes(
    pool: &PgPool,
    models: &ModelGraph,
    order: &[String],
    log: &dyn Log,
) {
    let tables: Vec<String> = order
        .iter()
        .filter_map(|n| models.get(n).map(|i| i.table.clone()))
        .collect();
    let rows: Result<Vec<(String, String)>, _> = sqlx::query_as(
        r#"SELECT indexname::text, tablename::text
             FROM pg_indexes
            WHERE schemaname = current_schema()
              AND indexdef LIKE 'CREATE UNIQUE INDEX%'
              AND indexdef LIKE '%WHERE%'
              AND tablename = ANY($1)"#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await;
    let Ok(rows) = rows else {
        return;
    };
    let unknown: Vec<String> = rows
        .iter()
        .filter(|(i, _)| !KNOWN_PARTIAL_UNIQUE_INDEXES.contains(&i.as_str()))
        .map(|(i, t)| format!("{t}.{i}"))
        .collect();
    if !unknown.is_empty() {
        log.line(&format!(
            "PERINGATAN: {} partial unique index di database tujuan belum dikenal\nscript ini: {}.\nKalau kuncinya memuat userId tanpa companyId, restore bisa gagal di tengah jalan.\nDaftarkan di PARTIAL_UNIQUE_INDEXES (src/admin-jobs/company-data/company-scope.ts).\n",
            unknown.len(),
            unknown.join(", ")
        ));
    }
}

/// `restoreCompany(deps, params, log)` untuk admin job (`yes: true`). Hasil = objek
/// `{ status, dryRun, newCompanyId, reusedUsers, inserted, patchedUuidRefs, tables }`.
pub async fn restore_company(
    pool: &PgPool,
    params: &RestoreParams,
    log: &dyn Log,
) -> CdResult<Value> {
    validate(params)?;
    let manifest = load_manifest(&params.dir)?;
    let models = embedded_graph()?;

    let fingerprint = schema_fingerprint(&models);
    let backup_fp = manifest
        .get("schemaFingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if backup_fp != fingerprint {
        return fail(
            Code::SchemaMismatch,
            format!(
                "Schema sudah berubah sejak backup dibuat ({backup_fp} != {fingerprint}).\n  Restore dihentikan supaya tidak menulis kolom yang artinya sudah berbeda.\n  Restore dengan checkout kode di versi backup itu, atau perbaiki backupnya lebih dulu."
            ),
        );
    }

    let order: Vec<String> = manifest
        .get("order")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    for name in &order {
        if !models.contains_key(name) {
            return fail(
                Code::SchemaMismatch,
                format!("Model {name} ada di backup tapi tidak ada di schema."),
            );
        }
    }
    if !order.iter().any(|n| n == "company") {
        return fail(Code::ManifestInvalid, "Backup tidak memuat baris company.");
    }

    log.line("Memeriksa berkas backup...");
    for name in &order {
        let Some(table) = manifest.get("tables").and_then(|t| t.get(name)) else {
            continue;
        };
        if table.get("rows").and_then(Value::as_u64).unwrap_or(0) == 0 {
            continue;
        }
        let file_name = table
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let file = resolve_backup_file(&params.dir, file_name)?;
        if !file.exists() {
            return fail(
                Code::BackupFileMissing,
                format!("Berkas hilang: {file_name}"),
            );
        }
        let expected = table
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (actual, ok) = verify_checksum(&file, expected)?;
        if !ok {
            return fail(
                Code::ChecksumMismatch,
                format!(
                    "Checksum {file_name} tidak cocok (manifest {expected}, berkas {actual}). Backup rusak atau sudah diubah — restore dihentikan."
                ),
            );
        }
    }

    let catalog = {
        let mut conn = pool.acquire().await?;
        Catalog::load(&mut conn).await?
    };
    catalog.verify(&models, order.iter())?;

    let target_db = describe_database(params.database_url.as_deref());
    let source = manifest.get("source").cloned().unwrap_or(Value::Null);
    let source_db = source.get("database").and_then(Value::as_str);
    let same_db = target_db.is_some() && target_db.as_deref() == source_db;
    let total_rows = manifest
        .get("totals")
        .and_then(|t| t.get("rows"))
        .map(|v| shown(Some(v)))
        .unwrap_or_else(|| "undefined".into());

    log.line("");
    log.line(&format!(
        "  Sumber   : company #{} \"{}\"",
        shown(source.get("companyId")),
        shown(source.get("companyName"))
    ));
    log.line(&format!(
        "             {}",
        source_db
            .filter(|s| !s.is_empty())
            .unwrap_or("(tidak tercatat)")
    ));
    log.line(&format!(
        "  Tujuan   : {}",
        target_db
            .as_deref()
            .unwrap_or("(DATABASE_URL tidak terbaca)")
    ));
    log.line(&format!(
        "  Mode     : clone{}",
        if params.dry_run {
            " (DRY RUN — akan di-rollback)"
        } else {
            ""
        }
    ));
    log.line(&format!(
        "  User     : {}{}",
        params.users.as_str(),
        params
            .username_suffix
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!(" (suffix \"{s}\")"))
            .unwrap_or_default()
    ));
    log.line(&format!(
        "  Tabel    : {}, total {total_rows} baris",
        order.len()
    ));
    log.line("");

    if params.users == UsersMode::Reuse {
        let risky: Vec<&String> = order
            .iter()
            .filter(|n| {
                user_scoped_risk(n)
                    && manifest
                        .get("tables")
                        .and_then(|t| t.get(n.as_str()))
                        .and_then(|t| t.get("rows"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        > 0
            })
            .collect();
        if !risky.is_empty() && same_db {
            let list = risky.iter().map(|s| s.as_str()).collect::<Vec<_>>();
            log.line(&format!(
                "PERINGATAN: user yang sudah ada akan dipakai ulang, sementara tabel berikut punya\nunique yang memuat userId tanpa companyId: {}.\nRestore ke database yang sama besar kemungkinan akan bentrok di sana.\nPertimbangkan --users insert --username-suffix \".copy\", atau --exclude {}.\n",
                list.join(", "),
                list.join(",")
            ));
        }
    }

    warn_unknown_partial_indexes(pool, &models, &order, log).await;

    log.line("Menulis...\n");
    let mut tx = tokio::time::timeout(Duration::from_secs(30), pool.begin())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Transaction API error: Unable to start a transaction in the given time."
            )
        })??;

    let mut restorer = Restorer {
        models: &models,
        catalog: &catalog,
        manifest: &manifest,
        params,
        pool: pool.clone(),
        audit: AuditHandle::detached(pool.clone()),
        id_map: HashMap::new(),
        uuid_map: HashMap::new(),
        external_id_map: HashMap::new(),
        pending: Vec::new(),
        stats: IndexMap::new(),
        new_company_id: None,
        reused_users: 0,
    };

    let timeout_ms = RESTORE_TIMEOUT_MINUTES * 60_000;
    let started = std::time::Instant::now();
    let run = async {
        if let Some(key) = &params.lock_key {
            mz_core::db::advisory_xact_lock(&mut tx, key).await?;
        }
        restorer.resolve_externals(&mut tx).await?;
        for name in &order {
            let stat = restorer.restore_model(&mut tx, name).await?;
            let expected = manifest
                .get("tables")
                .and_then(|t| t.get(name.as_str()))
                .and_then(|t| t.get("rows"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if expected == 0 && stat.inserted == 0 {
                continue;
            }
            let mut notes = Vec::new();
            if stat.reused > 0 {
                notes.push(format!("{} dipakai ulang", stat.reused));
            }
            if stat.skipped > 0 {
                notes.push(format!("{} dilewati", stat.skipped));
            }
            if stat.severed > 0 {
                notes.push(format!("{} rujukan dinolkan", stat.severed));
            }
            if stat.dangling > 0 {
                notes.push(format!("{} rujukan polimorfik menggantung", stat.dangling));
            }
            if stat.unique_regenerated > 0 {
                notes.push(format!(
                    "{} nilai unik dibuat ulang",
                    stat.unique_regenerated
                ));
            }
            if stat.unique_suffixed > 0 {
                notes.push(format!(
                    "{} nilai unik diberi akhiran",
                    stat.unique_suffixed
                ));
            }
            if stat.unique_nulled > 0 {
                notes.push(format!("{} nilai unik dinolkan", stat.unique_nulled));
            }
            if stat.partial_resolved > 0 {
                notes.push(format!(
                    "{} bentrok partial index diredakan",
                    stat.partial_resolved
                ));
            }
            log.line(&format!(
                "  {} {}/{}{}",
                pad_end(name, 34),
                pad_start(&stat.inserted.to_string(), 8),
                pad_end(&expected.to_string(), 8),
                if notes.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", notes.join(", "))
                }
            ));
        }
        let patched = restorer.flush_pending_uuid_refs(&mut tx).await?;
        if patched > 0 {
            log.line(&format!(
                "\n  {patched} rujukan uuid ditambal di pass terakhir."
            ));
        }
        CdResult::Ok(patched)
    };
    let patched = match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
        Ok(r) => r?,
        Err(_) => {
            anyhow::bail!(
                "Transaction API error: Transaction already closed: A query cannot be executed on an expired transaction. The timeout for this transaction was {timeout_ms} ms, however {} ms passed since the start of the transaction. Consider increasing the interactive transaction timeout or doing less work in the transaction.",
                started.elapsed().as_millis()
            );
        }
    };

    let inserted: u64 = restorer.stats.values().map(|s| s.inserted).sum();
    let tables: Map<String, Value> = restorer
        .stats
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(Value::Null)))
        .collect();
    let mut result = json!({
        "status": "done",
        "dryRun": false,
        "newCompanyId": restorer.new_company_id.clone(),
        "reusedUsers": restorer.reused_users,
        "inserted": inserted,
        "patchedUuidRefs": patched,
        "tables": tables,
    });

    if params.dry_run {
        tx.rollback().await?;
        log.line("\nDRY RUN selesai — semua perubahan sudah di-rollback.");
        log.line(&format!(
            "  {inserted} baris akan ditulis, company baru akan dibuat."
        ));
        if let Some(m) = result.as_object_mut() {
            m.insert("status".into(), json!("dry-run"));
            m.insert("dryRun".into(), json!(true));
        }
        return Ok(result);
    }
    tx.commit().await?;

    log.line("\nSelesai.");
    log.line(&format!(
        "  Company baru : #{}",
        shown(restorer.new_company_id.as_ref())
    ));
    log.line(&format!("  Baris ditulis: {inserted}"));
    if restorer.reused_users > 0 {
        log.line(&format!(
            "  User dipakai ulang (username sudah ada): {}",
            restorer.reused_users
        ));
    }
    log.line("\nCatatan: berkas di object storage (screenshot, foto kunjungan, lampiran form)");
    log.line("tidak ikut dipindahkan — barisnya menunjuk ke key bucket yang lama.");
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_backup_file_menolak_keluar_folder() {
        assert_eq!(
            resolve_backup_file(Path::new("/b"), "data/x.ndjson").unwrap(),
            PathBuf::from("/b/data/x.ndjson")
        );
        assert!(resolve_backup_file(Path::new("/b"), "../etc/passwd").is_err());
        assert!(resolve_backup_file(Path::new("/b"), "/etc/passwd").is_err());
        assert!(resolve_backup_file(Path::new("/b"), "").is_err());
        assert!(resolve_backup_file(Path::new("/b"), "data/../..").is_err());
    }

    #[test]
    fn users_insert_butuh_suffix() {
        let p = RestoreParams {
            dir: PathBuf::from("/x"),
            dry_run: true,
            users: UsersMode::Insert,
            username_suffix: None,
            rename: None,
            database_url: None,
            lock_key: None,
        };
        let err = validate(&p).unwrap_err();
        assert_eq!(
            super::super::errors::code_of(&err),
            Some(Code::InvalidParams)
        );
        let p = RestoreParams {
            dir: PathBuf::new(),
            users: UsersMode::Reuse,
            ..p
        };
        assert!(validate(&p).is_err());
    }

    #[test]
    fn manifest_versi_lain_ditolak() {
        let dir = std::env::temp_dir().join(format!("mz-adm-manifest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = load_manifest(&dir).unwrap_err();
        assert_eq!(
            super::super::errors::code_of(&err),
            Some(Code::ManifestInvalid)
        );
        std::fs::write(dir.join("manifest.json"), r#"{"formatVersion":1}"#).unwrap();
        let err = load_manifest(&dir).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Format backup versi 1, script ini hanya membaca versi 2."
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
