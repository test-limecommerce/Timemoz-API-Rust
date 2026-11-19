//! Resolusi + cache kebijakan visibilitas kolom (`table-field-policy.{service,cache}.ts`).
//!
//! Tiga tingkat, yang bawah menimpa yang atas: registry default (`visible`) ← default company
//! (`accessLevelId IS NULL`) ← override role. Cache 30 dtk per `(companyId, accessLevelId)`,
//! satu instans per proses, dengan penjaga epoch (`EpochCache`).
//!
//! Invalidasi lintas proses (`plan/cache-invalidation-protocol.md`): [`invalidate_company`] /
//! [`invalidate_all`] membersihkan cache lokal LALU mem-publish `table-field-policy`
//! (`mz_core::invalidation::Message::TableFieldPolicy`). Pesan MASUK (dari Node atau proses Rust
//! lain) diterapkan subscriber core lewat hook yang didaftarkan saat cache ini pertama dipakai
//! ([`mz_core::invalidation::register_external`]); reconnect subscriber ikut mengosongkannya.
//! Jalur reveal tetap memakai [`resolve_uncached`].

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use mz_core::cache::{EpochCache, Keep};
use mz_core::invalidation::{self, Message};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};

use crate::registry::{FieldVisibilityMode, TableDefinition, table_by_key};

/// §5.2 — 30 dtk: yang basi adalah data yang terlihat, bukan hak yang dicabut.
pub const TABLE_FIELD_POLICY_TTL: Duration = Duration::from_secs(30);

/// Kolom tersensor satu tabel: `columnKey -> mode` (tidak pernah memuat `visible`).
pub type CensoredColumns = HashMap<String, FieldVisibilityMode>;

/// Kebijakan seluruh tabel untuk satu pembaca. Hanya kolom yang BUKAN `visible`.
#[derive(Debug, Default, Clone)]
pub struct TableFieldPolicy {
    pub by_table: HashMap<String, Arc<CensoredColumns>>,
}

impl TableFieldPolicy {
    /// `modeFor`: `visible` untuk apa pun yang tidak disebut kebijakan.
    pub fn mode_for(&self, table_key: &str, column_key: &str) -> FieldVisibilityMode {
        self.by_table
            .get(table_key)
            .and_then(|m| m.get(column_key))
            .copied()
            .unwrap_or(FieldVisibilityMode::Visible)
    }

    /// `policyForTable`: peta kosong = tidak ada yang tersensor.
    pub fn for_table(&self, table_key: &str) -> Arc<CensoredColumns> {
        self.by_table
            .get(table_key)
            .cloned()
            .unwrap_or_else(|| Arc::new(CensoredColumns::new()))
    }
}

type PolicyCache = EpochCache<(i32, Option<i32>), Arc<TableFieldPolicy>>;

/// Satu instans per proses (modul Node di-cache). Hook invalidasi lintas proses didaftarkan
/// bersamaan: cache yang belum pernah dipakai tidak punya isi untuk dibuang.
static CACHE: LazyLock<PolicyCache> = LazyLock::new(|| {
    invalidation::register_external(apply_external);
    EpochCache::new(50_000)
});

/// Hook subscriber core: pesan `table-field-policy` dari proses lain, atau `None` = reconnect.
fn apply_external(msg: Option<&Message>) {
    match msg {
        None | Some(Message::TableFieldPolicy { company_id: None }) => invalidate_all_local(),
        Some(Message::TableFieldPolicy {
            company_id: Some(id),
        }) => invalidate_company_local(*id),
        Some(_) => {}
    }
}

/// Hanya lokal: seluruh entri satu company (semua role — baris default menjadi dasar semua role).
pub fn invalidate_company_local(company_id: i32) {
    CACHE.invalidate_where(|(c, _), _| *c == company_id);
}

/// Hanya lokal.
pub fn invalidate_all_local() {
    CACHE.invalidate_all();
}

/// `invalidateCompany`: lokal, lalu publish `table-field-policy` (fire-and-forget).
pub fn invalidate_company(state: &AppState, company_id: i32) {
    invalidate_company_local(company_id);
    publish(state, Some(company_id));
}

/// `invalidateAll`: lokal, lalu publish `table-field-policy` tanpa `companyId` (= semua).
pub fn invalidate_all(state: &AppState) {
    invalidate_all_local();
    publish(state, None);
}

fn publish(state: &AppState, company_id: Option<i32>) {
    invalidation::publish(state, Message::TableFieldPolicy { company_id });
}

#[derive(sqlx::FromRow)]
struct Row {
    #[sqlx(rename = "accessLevelId")]
    access_level_id: Option<i32>,
    #[sqlx(rename = "tableKey")]
    table_key: String,
    #[sqlx(rename = "columnKey")]
    column_key: String,
    mode: String,
}

/// Satu baris DB → entri kebijakan, atau dibuang (tabel/kolom di luar registry, kolom `locked`,
/// mode tak dikenal): baris nakal dari skrip/restore tidak boleh menyensor kolom lain.
fn apply(merged: &mut HashMap<String, CensoredColumns>, row: &Row) {
    let Some(table) = table_by_key(&row.table_key) else {
        return;
    };
    let Some(column) = table.columns.iter().find(|c| c.key == row.column_key) else {
        return;
    };
    if column.locked {
        return;
    }
    let Some(mode) = FieldVisibilityMode::parse(&row.mode) else {
        return;
    };
    merged
        .entry(row.table_key.clone())
        .or_default()
        .insert(row.column_key.clone(), mode);
}

/// Gabungkan baris DB menjadi kebijakan (default company dulu, lalu override role).
fn merge(rows: &[Row]) -> TableFieldPolicy {
    let mut merged: HashMap<String, CensoredColumns> = HashMap::new();
    for row in rows.iter().filter(|r| r.access_level_id.is_none()) {
        apply(&mut merged, row);
    }
    for row in rows.iter().filter(|r| r.access_level_id.is_some()) {
        apply(&mut merged, row);
    }
    let mut by_table = HashMap::new();
    for (table, columns) in merged {
        // Override role boleh `visible` (membatalkan default company); dibuang di sini.
        let censored: CensoredColumns = columns
            .into_iter()
            .filter(|(_, m)| *m != FieldVisibilityMode::Visible)
            .collect();
        if !censored.is_empty() {
            by_table.insert(table, Arc::new(censored));
        }
    }
    TableFieldPolicy { by_table }
}

async fn fetch(
    db: &sqlx::PgPool,
    company_id: i32,
    access_level_id: Option<i32>,
) -> sqlx::Result<TableFieldPolicy> {
    // `companyId` SELALU di klausa yang sama (§1.6): role bawaan dipakai semua company.
    let rows: Vec<Row> = sqlx::query_as(
        r#"SELECT "accessLevelId", "tableKey", "columnKey", mode::text AS mode
             FROM "tableFieldVisibility"
            WHERE "companyId" = $1
              AND ("accessLevelId" IS NULL OR "accessLevelId" = $2)
            ORDER BY id"#,
    )
    .bind(company_id)
    .bind(access_level_id)
    .fetch_all(db)
    .await?;
    Ok(merge(&rows))
}

/// `resolve(companyId, accessLevelId)`. `company_id` 0 → kebijakan kosong tanpa menyentuh DB
/// (menolak menebak company, §1.6).
pub async fn resolve(
    state: &AppState,
    company_id: i32,
    access_level_id: Option<i32>,
) -> ApiResult<Arc<TableFieldPolicy>> {
    if company_id == 0 {
        return Ok(Arc::new(TableFieldPolicy::default()));
    }
    let db = state.db.clone();
    CACHE
        .get_or_load(
            (company_id, access_level_id),
            |_| Some(Keep::ttl(TABLE_FIELD_POLICY_TTL)),
            move || async move {
                fetch(&db, company_id, access_level_id)
                    .await
                    .map(Arc::new)
                    .map_err(|e| Arc::new(anyhow::Error::from(e)))
            },
        )
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))
}

/// Seperti [`resolve`] tetapi selalu membaca DB (tidak membaca/menulis cache). Dipakai jalur
/// reveal supaya perubahan dari Node langsung berlaku (lihat catatan modul).
pub async fn resolve_uncached(
    state: &AppState,
    company_id: i32,
    access_level_id: Option<i32>,
) -> ApiResult<Arc<TableFieldPolicy>> {
    if company_id == 0 {
        return Ok(Arc::new(TableFieldPolicy::default()));
    }
    Ok(Arc::new(
        fetch(&state.db, company_id, access_level_id).await?,
    ))
}

/// `policyForRequest`: company dari `req.access` bila ada, selain itu `resolveRequestCompanyId`.
/// Company tak ketemu = kebijakan kosong (satu-satunya jawaban aman). `fresh` = lewati cache.
pub async fn policy_for_request(
    state: &AppState,
    ctx: &RequestCtx,
    fresh: bool,
) -> ApiResult<Arc<TableFieldPolicy>> {
    let access = ctx.access();
    let company_id = match access {
        Some(a) => Some(a.company_id),
        None => ctx.company_id(state).await?,
    };
    let Some(company_id) = company_id else {
        return Ok(Arc::new(TableFieldPolicy::default()));
    };
    let role = access.map(|a| a.role_id);
    if fresh {
        resolve_uncached(state, company_id, role).await
    } else {
        resolve(state, company_id, role).await
    }
}

/// `censoredColumns(req, tableKey)`: bentuk yang dipakai `apply_field_policy`, klausa search dan
/// validasi sort. Peta kosong = tanpa penyimpangan.
pub async fn censored_columns(
    state: &AppState,
    ctx: &RequestCtx,
    table: &TableDefinition,
) -> ApiResult<Arc<CensoredColumns>> {
    Ok(policy_for_request(state, ctx, false)
        .await?
        .for_table(table.key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(role: Option<i32>, table: &str, col: &str, mode: &str) -> Row {
        Row {
            access_level_id: role,
            table_key: table.into(),
            column_key: col.into(),
            mode: mode.into(),
        }
    }

    #[test]
    fn role_overrides_company_default_and_visible_is_dropped() {
        let rows = vec![
            row(Some(5), "stores", "phone", "visible"),
            row(None, "stores", "phone", "masked"),
            row(None, "stores", "address", "hidden"),
        ];
        let p = merge(&rows);
        assert_eq!(p.mode_for("stores", "phone"), FieldVisibilityMode::Visible);
        assert_eq!(p.mode_for("stores", "address"), FieldVisibilityMode::Hidden);
    }

    #[test]
    fn junk_rows_are_ignored() {
        let rows = vec![
            row(None, "stores", "lat", "hidden"),
            row(None, "ghost", "x", "hidden"),
            row(None, "stores", "nope", "hidden"),
            row(None, "stores", "phone", "weird"),
        ];
        assert!(merge(&rows).by_table.is_empty());
    }

    #[test]
    fn payload_matches_protocol() {
        let v = Message::TableFieldPolicy {
            company_id: Some(7),
        }
        .to_value("rust-x");
        assert_eq!(
            v,
            serde_json::json!({"v":1,"src":"rust-x","kind":"table-field-policy","companyId":7})
        );
        let all = Message::TableFieldPolicy { company_id: None }.to_value("rust-x");
        assert!(all.get("companyId").is_none());
    }

    #[test]
    fn external_hook_applies_inbound_messages() {
        // Mendaftarkan hook (lewat `LazyLock`) lalu memastikan dispatch tidak panik untuk
        // semua bentuk pesan, termasuk reconnect (`None`).
        LazyLock::force(&CACHE);
        apply_external(Some(&Message::TableFieldPolicy {
            company_id: Some(1),
        }));
        apply_external(Some(&Message::TableFieldPolicy { company_id: None }));
        apply_external(Some(&Message::AccessAll));
        apply_external(None);
    }
}
