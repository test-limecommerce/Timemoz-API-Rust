//! Port `src/admin-jobs/company-data/backup.ts`: backup seluruh data satu company ke folder
//! yang bisa direstore ulang tanpa bentrok id (`manifest.json` + `data/<model>.ndjson[.gz]`).
//!
//! Format berkas sama dengan Node (format versi 2, sidik jari schema sama), jadi backup Rust
//! bisa direstore Node dan sebaliknya. Yang berbeda hanya teks per baris yang tidak semantik
//! (urutan kunci = urutan field DMMF, angka float ditulis `serde_json`); sha256 di manifest
//! dihitung dari baris yang benar-benar ditulis, sama seperti Node.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;

use super::errors::{CdResult, Code, Log, fail};
use super::pg::{Catalog, key_of, parse_rows, q, text_of};
use super::scope::{
    ANCHORS, EXTERNAL_MODELS, ModelGraph, ModelInfo, Owner, PlanOptions, Row, ScopePlan,
    build_scope_plan, describe_database, embedded_graph, hybrid, schema_fingerprint,
    secret_columns,
};

pub const BACKUP_FORMAT_VERSION: i64 = 2;
/// Jumlah id maksimum per klausa IN.
const ID_CHUNK: usize = 5000;
pub const BACKUP_BATCH_DEFAULT: i64 = 2000;

/// `BackupParams` (yang dipakai admin job; opsi CLI lain tidak di-port).
#[derive(Debug, Clone)]
pub struct BackupParams {
    pub company_id: i32,
    pub out_dir: PathBuf,
    pub gzip: bool,
    pub skip_heavy: bool,
    pub redact_secrets: bool,
    /// URL database sumber, hanya dicatat host + nama db di manifest.
    pub database_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Rencana
// ---------------------------------------------------------------------------

/// `prepareBackupPlan(opts, log)`.
pub fn prepare_backup_plan(opts: &PlanOptions, log: &dyn Log) -> CdResult<(ModelGraph, ScopePlan)> {
    let models = embedded_graph()?;
    let plan = build_scope_plan(&models, opts)?;
    if !plan.uncovered.is_empty() {
        log.line(&format!(
            "PERINGATAN: {} model tidak punya jalur ke company dan TIDAK ikut dibackup: {}\nKalau salah satunya menyimpan data tenant, daftarkan relasinya di src/admin-jobs/company-data/company-scope.ts.",
            plan.uncovered.len(),
            plan.uncovered.join(", ")
        ));
    }
    Ok((models, plan))
}

fn describe_owner(owner: &Owner) -> String {
    match owner {
        Owner::AnchorSelf => "company.id".to_owned(),
        Owner::AnchorVia { model, column } => format!("{model}.{column}"),
        Owner::Scan => "(pindai penuh)".to_owned(),
        Owner::Fk { column, target } => format!("{column} -> {target}"),
    }
}

/// `String.prototype.padEnd` / `padStart` (unit UTF-16 ≈ char untuk nama model ASCII).
pub fn pad_end(s: &str, width: usize) -> String {
    let n = s.encode_utf16().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

pub fn pad_start(s: &str, width: usize) -> String {
    let n = s.encode_utf16().count();
    format!("{}{s}", " ".repeat(width.saturating_sub(n)))
}

/// `formatScopePlan(plan)`.
pub fn format_scope_plan(plan: &ScopePlan) -> Vec<String> {
    let mut lines = vec![format!("Model yang ikut ({}):", plan.order.len())];
    for name in &plan.order {
        let via = plan
            .owners
            .get(name)
            .map(describe_owner)
            .unwrap_or_default();
        lines.push(format!("  {} via {via}", pad_end(name, 34)));
    }
    let mut external: Vec<&String> = plan.external.iter().collect();
    external.sort();
    lines.push(format!(
        "\nGlobal (tidak disalin, dicocokkan ulang saat restore): {}",
        external
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if !plan.hybrid.is_empty() {
        let mut hybrid: Vec<&String> = plan.hybrid.iter().collect();
        hybrid.sort();
        lines.push(format!(
            "Separuh global (baris ber-companyId ikut disalin, baris global dicocokkan ulang): {}",
            hybrid
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Penulis NDJSON
// ---------------------------------------------------------------------------

enum Sink {
    Plain(BufWriter<File>),
    // Box: state zlib besar (clippy `large_enum_variant` saat fitur flate2 workspace digabung).
    Gzip(Box<GzEncoder<BufWriter<File>>>),
}

/// `NdjsonWriter`: satu baris JSON per row; sha256 dihitung dari teks SEBELUM gzip.
pub struct NdjsonWriter {
    sink: Sink,
    hash: Sha256,
    count: u64,
}

impl NdjsonWriter {
    pub fn create(path: &Path, gzip: bool) -> std::io::Result<Self> {
        let file = BufWriter::new(File::create(path)?);
        let sink = if gzip {
            Sink::Gzip(Box::new(GzEncoder::new(file, Compression::new(6))))
        } else {
            Sink::Plain(file)
        };
        Ok(Self {
            sink,
            hash: Sha256::new(),
            count: 0,
        })
    }

    pub fn write(&mut self, row: &Row) -> CdResult<()> {
        let mut line = serde_json::to_string(row)?;
        line.push('\n');
        self.hash.update(line.as_bytes());
        self.count += 1;
        match &mut self.sink {
            Sink::Plain(w) => w.write_all(line.as_bytes())?,
            Sink::Gzip(w) => w.write_all(line.as_bytes())?,
        }
        Ok(())
    }

    /// `{ count, sha256 }`.
    pub fn end(self) -> CdResult<(u64, String)> {
        match self.sink {
            Sink::Plain(mut w) => w.flush()?,
            Sink::Gzip(w) => w.finish()?.flush()?,
        }
        Ok((self.count, hex::encode(self.hash.finalize())))
    }
}

// ---------------------------------------------------------------------------
// Pengambilan data
// ---------------------------------------------------------------------------

/// Satu klausa `where` (`buildWhereClauses`).
#[derive(Debug, Clone)]
pub enum Where {
    /// `{}` (pindai penuh).
    All,
    /// `{ <col>: companyId }`.
    EqInt(String, i32),
    /// `{ <col>: { in: chunk } }`: nilai teks + tipe SQL kolom.
    In(String, String, Vec<String>),
}

fn chunk<T: Clone>(values: &[T], size: usize) -> Vec<Vec<T>> {
    values.chunks(size).map(<[T]>::to_vec).collect()
}

/// `buildWhereClauses(info, owner, companyId, idSets, anchorIds)`.
pub fn build_where_clauses(
    catalog: &Catalog,
    info: &ModelInfo,
    owner: &Owner,
    company_id: i32,
    id_sets: &HashMap<String, IndexMap<String, Value>>,
    anchor_ids: &HashMap<String, Vec<Value>>,
) -> CdResult<Vec<Where>> {
    match owner {
        Owner::AnchorSelf => Ok(vec![Where::EqInt("id".into(), company_id)]),
        Owner::AnchorVia { .. } => {
            let ids = anchor_ids.get(&info.name).cloned().unwrap_or_default();
            if ids.is_empty() {
                return Ok(vec![]);
            }
            let ty = catalog.sql_type(info, "id")?;
            let texts: Vec<String> = ids.iter().filter_map(text_of).collect();
            Ok(chunk(&texts, ID_CHUNK)
                .into_iter()
                .map(|c| Where::In("id".into(), ty.clone(), c))
                .collect())
        }
        Owner::Scan => Ok(vec![Where::All]),
        Owner::Fk { column, target } => {
            if column == "companyId" && target == "company" {
                return Ok(vec![Where::EqInt("companyId".into(), company_id)]);
            }
            let Some(parent) = id_sets.get(target).filter(|s| !s.is_empty()) else {
                return Ok(vec![]);
            };
            let ty = catalog.sql_type(info, column)?;
            let texts: Vec<String> = parent.values().filter_map(text_of).collect();
            Ok(chunk(&texts, ID_CHUNK)
                .into_iter()
                .map(|c| Where::In(column.clone(), ty.clone(), c))
                .collect())
        }
    }
}

/// Satu halaman `findMany` (cursor = pk, atau offset untuk pk gabungan).
async fn fetch_page(
    conn: &mut PgConnection,
    catalog: &Catalog,
    info: &ModelInfo,
    clause: &Where,
    cursor: Option<&Value>,
    offset: i64,
    batch: i64,
) -> CdResult<Vec<Row>> {
    let fields: Vec<&str> = info.scalar_fields.iter().map(String::as_str).collect();
    let select = catalog.select_list(info, &fields, "t")?;
    let mut conds = Vec::new();
    let mut n = 0;
    let mut next = || {
        n += 1;
        format!("${n}")
    };
    match clause {
        Where::All => {}
        Where::EqInt(col, _) => conds.push(format!("t.{} = {}", q(col), next())),
        Where::In(col, ty, _) => {
            conds.push(format!("t.{} = ANY({}::text[]::{ty}[])", q(col), next()))
        }
    }
    let order_cols: Vec<String> = match (&info.pk, &info.composite_pk) {
        (Some(pk), _) => vec![pk.clone()],
        (None, Some(cols)) if !cols.is_empty() => cols.clone(),
        _ => {
            return fail(
                Code::ScopeConfig,
                format!(
                    "Model {} tidak punya primary key yang bisa diurutkan.",
                    info.name
                ),
            );
        }
    };
    let mut cursor_text = None;
    if let (Some(pk), Some(c)) = (&info.pk, cursor) {
        let ty = catalog.sql_type(info, pk)?;
        conds.push(format!("t.{} > {}::text::{ty}", q(pk), next()));
        cursor_text = text_of(c);
    }
    let limit = next();
    let offset_ph = next();
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let inner_order = order_cols
        .iter()
        .map(|c| format!("t.{} ASC", q(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let outer_order = order_cols
        .iter()
        .map(|c| format!("x.{}", q(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT row_to_json(x)::text FROM (SELECT {select} FROM {} t {where_sql} ORDER BY {inner_order} LIMIT {limit} OFFSET {offset_ph}) x ORDER BY {outer_order}",
        q(&info.table)
    );
    let mut query = sqlx::query_scalar::<_, String>(&sql).persistent(false);
    match clause {
        Where::All => {}
        Where::EqInt(_, v) => query = query.bind(*v),
        Where::In(_, _, vals) => query = query.bind(vals.clone()),
    }
    if info.pk.is_some() && cursor.is_some() {
        query = query.bind(cursor_text);
    }
    let raw = query.bind(batch).bind(offset).fetch_all(&mut *conn).await?;
    parse_rows(info, raw)
}

/// Statistik rujukan per tabel (`RowStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowStats {
    pub dropped: u64,
    pub severed: u64,
    pub dangling: u64,
}

/// `reconcileRow(row, info, plan, idSets, stats, hybridRefs)`. `None` = baris dibuang.
pub fn reconcile_row(
    mut row: Row,
    info: &ModelInfo,
    plan: &ScopePlan,
    id_sets: &HashMap<String, IndexMap<String, Value>>,
    stats: &mut RowStats,
    hybrid_refs: &mut IndexMap<String, IndexMap<String, Value>>,
) -> Option<Row> {
    for fk in &info.fks {
        if !plan.selected.contains(&fk.target) {
            continue;
        }
        let value = match row.get(&fk.column) {
            None | Some(Value::Null) => continue,
            Some(v) => v.clone(),
        };
        if plan.hybrid.contains(&fk.target) {
            hybrid_refs
                .entry(fk.target.clone())
                .or_default()
                .insert(key_of(&value), value);
            continue;
        }
        if id_sets
            .get(&fk.target)
            .is_some_and(|s| s.contains_key(&key_of(&value)))
        {
            continue;
        }
        if fk.nullable {
            row.insert(fk.column.clone(), Value::Null);
            stats.severed += 1;
        } else {
            stats.dropped += 1;
            return None;
        }
    }

    for lf in &info.list_fks {
        let Some(Value::Array(values)) = row.get(&lf.column) else {
            continue;
        };
        if values.is_empty() || !(lf.when)(&row) {
            continue;
        }
        let Some(set) = id_sets.get(&lf.target) else {
            continue;
        };
        let kept: Vec<Value> = values
            .iter()
            .filter(|v| set.contains_key(&key_of(v)))
            .cloned()
            .collect();
        if kept.len() != values.len() {
            stats.severed += 1;
        }
        row.insert(lf.column.clone(), Value::Array(kept));
    }

    if let Some(poly) = info.polymorphic {
        let target = row
            .get(poly.discriminator)
            .and_then(Value::as_str)
            .and_then(|d| poly.target(d));
        let set = target.and_then(|t| id_sets.get(t));
        match row.get(poly.id_column) {
            None | Some(Value::Null) => {}
            Some(v) => {
                if !set.is_some_and(|s| s.contains_key(&key_of(v))) {
                    stats.dangling += 1;
                }
            }
        }
    }
    Some(row)
}

/// `redact(row, columns, info)`: kolom wajib → `[REDACTED]`, opsional → null.
pub fn redact(row: &mut Row, columns: &[&str], info: &ModelInfo) {
    for col in columns {
        match row.get(*col) {
            None | Some(Value::Null) => continue,
            Some(_) => {}
        }
        let required = info.field_meta.get(*col).is_some_and(|m| m.is_required);
        row.insert(
            (*col).to_owned(),
            if required {
                Value::String("[REDACTED]".into())
            } else {
                Value::Null
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// `backupCompany(deps, params, log)`. Mengembalikan manifest (juga ditulis ke
/// `<outDir>/manifest.json`).
pub async fn backup_company(
    conn: &mut PgConnection,
    params: &BackupParams,
    log: &dyn Log,
) -> CdResult<Value> {
    if params.company_id < 1 {
        return fail(
            Code::InvalidParams,
            format!("companyId tidak valid: {}", params.company_id),
        );
    }
    let opts = PlanOptions {
        skip_heavy: params.skip_heavy,
        ..PlanOptions::default()
    };
    let (models, plan) = prepare_backup_plan(&opts, log)?;
    let catalog = Catalog::load(conn).await?;
    catalog.verify(&models, plan.order.iter())?;

    let started_at = mz_core::time::JsDate::now().to_js_string();
    let out_dir = &params.out_dir;

    let company: Option<(i32, String, uuid::Uuid)> =
        sqlx::query_as(r#"SELECT id, name, uuid FROM company WHERE id = $1"#)
            .bind(params.company_id)
            .persistent(false)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((company_id, company_name, company_uuid)) = company else {
        return fail(
            Code::CompanyNotFound,
            format!("Company id {} tidak ditemukan.", params.company_id),
        );
    };

    std::fs::create_dir_all(out_dir.join("data"))?;
    log.line(&format!("Company #{company_id} — {company_name}"));
    log.line(&format!("Tujuan: {}\n", out_dir.display()));

    // Jangkar: himpunan id yang tidak bisa ditemukan lewat kolom companyId.
    let mut anchor_ids: HashMap<String, Vec<Value>> = HashMap::new();
    for a in ANCHORS {
        let Some((via_model, via_column)) = a.via else {
            continue;
        };
        if !plan.selected.contains(a.name) {
            continue;
        }
        let Some(via) = models.get(via_model) else {
            continue;
        };
        let sql = format!(
            "SELECT to_jsonb(t.{col}) FROM {tbl} t WHERE t.\"companyId\" = $1",
            col = q(via_column),
            tbl = q(&via.table)
        );
        let rows: Vec<Option<Value>> = sqlx::query_scalar(&sql)
            .bind(params.company_id)
            .persistent(false)
            .fetch_all(&mut *conn)
            .await?;
        let mut seen: IndexMap<String, Value> = IndexMap::new();
        for v in rows.into_iter().flatten() {
            if !v.is_null() {
                seen.insert(key_of(&v), v);
            }
        }
        let ids: Vec<Value> = seen.into_values().collect();
        log.line(&format!(
            "  jangkar {}: {} baris via {via_model}.{via_column}",
            a.name,
            ids.len()
        ));
        anchor_ids.insert(a.name.to_owned(), ids);
    }
    log.line("");

    // Hanya simpan himpunan id untuk model yang benar-benar dirujuk model lain.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &plan.selected {
        let Some(info) = models.get(name) else {
            continue;
        };
        for f in &info.fks {
            if plan.selected.contains(&f.target) {
                referenced.insert(f.target.clone());
            }
        }
        for f in &info.list_fks {
            if plan.selected.contains(&f.target) {
                referenced.insert(f.target.clone());
            }
        }
        if let Some(p) = info.polymorphic {
            for (_, t) in p.targets {
                if plan.selected.contains(*t) {
                    referenced.insert((*t).to_owned());
                }
            }
        }
    }

    let mut id_sets: HashMap<String, IndexMap<String, Value>> = HashMap::new();
    let mut external_refs: IndexMap<String, IndexMap<String, Value>> = IndexMap::new();
    let mut hybrid_refs: IndexMap<String, IndexMap<String, Value>> = IndexMap::new();
    let mut tables = serde_json::Map::new();
    let mut totals = (0u64, 0u64, 0u64, 0u64);
    let ext = if params.gzip { ".gz" } else { "" };

    for name in &plan.order {
        let Some(info) = models.get(name) else {
            continue;
        };
        let Some(owner) = plan.owners.get(name) else {
            continue;
        };
        if *owner == Owner::Scan {
            log.line(&format!(
                "  {name}: tidak punya FK penyaring, dipindai penuh lalu disaring di memori."
            ));
        }
        let keep_ids = referenced.contains(name) || plan.hybrid.contains(name);
        if keep_ids {
            id_sets.insert(name.clone(), IndexMap::new());
        }

        let file_name = format!("{name}.ndjson{ext}");
        let mut writer = NdjsonWriter::create(&out_dir.join("data").join(&file_name), params.gzip)?;
        let mut stats = RowStats::default();
        let secrets = if params.redact_secrets {
            secret_columns(name)
        } else {
            &[]
        };

        let clauses = build_where_clauses(
            &catalog,
            info,
            owner,
            params.company_id,
            &id_sets,
            &anchor_ids,
        )?;
        for clause in &clauses {
            let mut cursor: Option<Value> = None;
            let mut offset = 0i64;
            loop {
                let rows = fetch_page(
                    conn,
                    &catalog,
                    info,
                    clause,
                    cursor.as_ref(),
                    if info.pk.is_some() { 0 } else { offset },
                    BACKUP_BATCH_DEFAULT,
                )
                .await?;
                let n = rows.len();
                if n == 0 {
                    break;
                }
                if let Some(pk) = &info.pk {
                    cursor = rows.last().and_then(|r| r.get(pk)).cloned();
                }
                offset += i64::try_from(n).unwrap_or(i64::MAX);
                for raw in rows {
                    let Some(mut row) =
                        reconcile_row(raw, info, &plan, &id_sets, &mut stats, &mut hybrid_refs)
                    else {
                        continue;
                    };
                    if !secrets.is_empty() {
                        redact(&mut row, secrets, info);
                    }
                    for fk in &info.fks {
                        if plan.external.contains(&fk.target)
                            && let Some(v) = row.get(&fk.column).filter(|v| !v.is_null())
                        {
                            external_refs
                                .entry(fk.target.clone())
                                .or_default()
                                .insert(key_of(v), v.clone());
                        }
                    }
                    if keep_ids && let Some(set) = id_sets.get_mut(name) {
                        let pk_value = info
                            .pk
                            .as_ref()
                            .and_then(|pk| row.get(pk))
                            .cloned()
                            .unwrap_or(Value::Null);
                        set.insert(key_of(&pk_value), pk_value);
                    }
                    writer.write(&row)?;
                }
                if i64::try_from(n).unwrap_or(0) < BACKUP_BATCH_DEFAULT {
                    break;
                }
            }
        }

        let (count, sha) = writer.end()?;
        tables.insert(
            name.clone(),
            json!({
                "file": format!("data/{file_name}"),
                "rows": count,
                "sha256": sha,
                "droppedOutOfScope": stats.dropped,
                "severedOptionalRefs": stats.severed,
                "danglingPolymorphicRefs": stats.dangling,
            }),
        );
        totals.0 += count;
        totals.1 += stats.dropped;
        totals.2 += stats.severed;
        totals.3 += stats.dangling;

        let mut notes = Vec::new();
        if stats.dropped > 0 {
            notes.push(format!("{} dibuang (di luar irisan)", stats.dropped));
        }
        if stats.severed > 0 {
            notes.push(format!("{} rujukan opsional dinolkan", stats.severed));
        }
        if stats.dangling > 0 {
            notes.push(format!("{} rujukan polimorfik menggantung", stats.dangling));
        }
        log.line(&format!(
            "  {} {} baris{}",
            pad_end(name, 34),
            pad_start(&count.to_string(), 8),
            if notes.is_empty() {
                String::new()
            } else {
                format!("   [{}]", notes.join(", "))
            }
        ));
    }

    // Natural key model global.
    let mut externals = serde_json::Map::new();
    for (name, ids) in &external_refs {
        let Some((_, Some(keys))) = EXTERNAL_MODELS.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let Some(info) = models.get(name) else {
            continue;
        };
        let mut fields = vec!["id"];
        fields.extend(keys.iter());
        let rows = select_by_ids(conn, &catalog, info, &fields, ids).await?;
        externals.insert(name.clone(), json!({ "naturalKey": keys, "rows": rows }));
    }

    // Model separuh global.
    for (name, ids) in &hybrid_refs {
        let Some(cfg) = hybrid(name) else {
            continue;
        };
        let Some(info) = models.get(name) else {
            continue;
        };
        let own = id_sets.get(name);
        let foreign: IndexMap<String, Value> = ids
            .iter()
            .filter(|(k, _)| !own.is_some_and(|o| o.contains_key(*k)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let match_obj: serde_json::Map<String, Value> = cfg
            .match_null
            .iter()
            .map(|c| ((*c).to_owned(), Value::Null))
            .collect();
        if foreign.is_empty() {
            externals.insert(
                name.clone(),
                json!({ "naturalKey": cfg.natural_key, "match": match_obj, "rows": [] }),
            );
            continue;
        }
        let mut fields = vec!["id", cfg.scope_column];
        fields.extend(cfg.natural_key.iter());
        let rows = select_by_ids(conn, &catalog, info, &fields, &foreign).await?;
        let alien: Vec<&Row> = rows
            .iter()
            .filter(|r| r.get(cfg.scope_column).is_some_and(|v| !v.is_null()))
            .collect();
        if !alien.is_empty() {
            let sample: Vec<String> = alien
                .iter()
                .take(5)
                .map(|r| r.get("id").map(js_string).unwrap_or_default())
                .collect();
            return fail(
                Code::CrossCompanyLeak,
                format!(
                    "{} baris {name} yang dirujuk company ini ternyata milik company lain (id {}). Data sumbernya sudah bocor antar company — bereskan dulu sebelum dibackup.",
                    alien.len(),
                    sample.join(", ")
                ),
            );
        }
        let projected: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut out = serde_json::Map::new();
                out.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
                for k in cfg.natural_key {
                    out.insert((*k).to_owned(), r.get(*k).cloned().unwrap_or(Value::Null));
                }
                Value::Object(out)
            })
            .collect();
        externals.insert(
            name.clone(),
            json!({ "naturalKey": cfg.natural_key, "match": match_obj, "rows": projected }),
        );
        log.line(&format!(
            "  rujukan global {name}: {} baris dicatat untuk dicocokkan ulang",
            rows.len()
        ));
    }

    let manifest = json!({
        "formatVersion": BACKUP_FORMAT_VERSION,
        "generatedAt": started_at,
        "finishedAt": mz_core::time::JsDate::now().to_js_string(),
        "schemaFingerprint": schema_fingerprint(&models),
        "source": {
            "companyId": company_id,
            "companyUuid": company_uuid.to_string(),
            "companyName": company_name,
            "database": describe_database(params.database_url.as_deref()),
        },
        "options": {
            "skipHeavy": params.skip_heavy,
            "includeEphemeral": false,
            "include": [],
            "exclude": [],
            "redactSecrets": params.redact_secrets,
            "gzip": params.gzip,
        },
        "order": plan.order,
        "tables": tables,
        "externals": externals,
        "uncoveredModels": plan.uncovered,
        "totals": { "rows": totals.0, "dropped": totals.1, "severed": totals.2, "dangling": totals.3 },
    });
    std::fs::write(
        out_dir.join("manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;

    log.line(&format!(
        "\nSelesai. {} baris di {} tabel.",
        totals.0,
        plan.order.len()
    ));
    if totals.1 > 0 {
        log.line(&format!(
            "  {} baris dibuang karena menunjuk ke luar irisan company ini.",
            totals.1
        ));
    }
    if totals.2 > 0 {
        log.line(&format!(
            "  {} rujukan opsional lintas-company dinolkan.",
            totals.2
        ));
    }
    if totals.3 > 0 {
        log.line(&format!(
            "  {} rujukan polimorfik (auditLog) menggantung — entitasnya memang sudah dihapus.",
            totals.3
        ));
    }
    Ok(manifest)
}

/// `String(value)` untuk pesan.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `findMany({ where: { id: { in: ids } }, select })`.
async fn select_by_ids(
    conn: &mut PgConnection,
    catalog: &Catalog,
    info: &ModelInfo,
    fields: &[&str],
    ids: &IndexMap<String, Value>,
) -> CdResult<Vec<Row>> {
    let select = catalog.select_list(info, fields, "t")?;
    let ty = catalog.sql_type(info, "id")?;
    let texts: Vec<String> = ids.values().filter_map(text_of).collect();
    let sql = format!(
        "SELECT row_to_json(x)::text FROM (SELECT {select} FROM {} t WHERE t.\"id\" = ANY($1::text[]::{ty}[]) ORDER BY t.\"id\") x",
        q(&info.table)
    );
    let raw: Vec<String> = sqlx::query_scalar(&sql)
        .bind(texts)
        .persistent(false)
        .fetch_all(&mut *conn)
        .await?;
    parse_rows(info, raw)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::company_data::scope::{build_scope_plan, embedded_graph};

    #[test]
    fn reconcile_row_seperti_node() {
        let models = embedded_graph().unwrap();
        let plan = build_scope_plan(&models, &PlanOptions::default()).unwrap();
        let info = &models["userAccessLevel"];
        let mut id_sets: HashMap<String, IndexMap<String, Value>> = HashMap::new();
        id_sets.insert(
            "users".into(),
            [("1".to_owned(), json!(1))].into_iter().collect(),
        );
        id_sets.insert(
            "company".into(),
            [("7".to_owned(), json!(7))].into_iter().collect(),
        );
        let mut stats = RowStats::default();
        let mut hybrid_refs = IndexMap::new();
        let row = |v: Value| v.as_object().unwrap().clone();

        let kept = reconcile_row(
            row(json!({ "id": 1, "userId": 1, "companyId": 7, "accessLevelId": 3 })),
            info,
            &plan,
            &id_sets,
            &mut stats,
            &mut hybrid_refs,
        );
        assert!(kept.is_some());
        assert_eq!(
            hybrid_refs["accessLevel"]
                .values()
                .cloned()
                .collect::<Vec<_>>(),
            vec![json!(3)]
        );
        let dropped = reconcile_row(
            row(json!({ "id": 2, "userId": 99, "companyId": 7, "accessLevelId": 3 })),
            info,
            &plan,
            &id_sets,
            &mut stats,
            &mut hybrid_refs,
        );
        assert!(dropped.is_none());
        assert_eq!(stats.dropped, 1);
    }

    #[test]
    fn redact_wajib_penanda_opsional_null() {
        let models = embedded_graph().unwrap();
        let info = &models["users"];
        let mut row = json!({ "password": "hash", "googleCalendarAccessToken": "tok" })
            .as_object()
            .unwrap()
            .clone();
        redact(&mut row, &["password", "googleCalendarAccessToken"], info);
        let want = if info.field_meta["password"].is_required {
            json!("[REDACTED]")
        } else {
            Value::Null
        };
        assert_eq!(row["password"], want);
        assert!(row["googleCalendarAccessToken"].is_null());
    }

    #[test]
    fn format_plan_baris_pertama_dan_company() {
        let models = embedded_graph().unwrap();
        let plan = build_scope_plan(&models, &PlanOptions::default()).unwrap();
        let lines = format_scope_plan(&plan);
        assert_eq!(lines[0], format!("Model yang ikut ({}):", plan.order.len()));
        assert!(lines.contains(&format!("  {} via company.id", pad_end("company", 34))));
    }

    #[test]
    fn ndjson_sha_sebelum_gzip() {
        let dir = std::env::temp_dir().join(format!("mz-adm-ndjson-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.ndjson.gz");
        let mut w = NdjsonWriter::create(&path, true).unwrap();
        let row = json!({ "id": 7, "big": { "$bigint": "5" } })
            .as_object()
            .unwrap()
            .clone();
        w.write(&row).unwrap();
        let (n, sha) = w.end().unwrap();
        let line = "{\"id\":7,\"big\":{\"$bigint\":\"5\"}}\n";
        assert_eq!(n, 1);
        assert_eq!(sha, hex::encode(Sha256::digest(line.as_bytes())));
        let mut gz = flate2::read::GzDecoder::new(File::open(&path).unwrap());
        let mut s = String::new();
        std::io::Read::read_to_string(&mut gz, &mut s).unwrap();
        assert_eq!(s, line);
        let empty = NdjsonWriter::create(&dir.join("e.ndjson"), false).unwrap();
        assert_eq!(empty.end().unwrap().1, hex::encode(Sha256::digest(b"")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
