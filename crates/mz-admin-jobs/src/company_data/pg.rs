//! Pengganti delegate Prisma dinamis (`prisma-delegate.ts`) untuk backup/restore:
//! katalog kolom Postgres + penerjemah nilai antara baris database dan bentuk JSON Prisma.
//!
//! - Baca: kolom dipilih dengan ekspresi yang menghasilkan bentuk `JSON.stringify` hasil
//!   Prisma (`DateTime` → `2026-11-16T08:00:00.000Z`, `Decimal` → string, `BigInt` →
//!   `{"$bigint":"…"}`, list NULL → `[]`), lalu `row_to_json` (urutan kunci = urutan field DMMF).
//! - Tulis: satu parameter `jsonb` + `jsonb_to_recordset` yang meng-cast tiap kolom ke tipenya
//!   (setara `createMany`: string ISO → `timestamp`/`date`, string → `numeric`, dst).

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::Value;
use sqlx::PgConnection;

use super::errors::{CdResult, Code, fail};
use super::scope::{FieldMeta, ModelGraph, ModelInfo, Row};

/// Jenis kolom fisik yang butuh perlakuan khusus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgKind {
    Timestamp,
    Timestamptz,
    Date,
    Time,
    Numeric,
    BigInt,
    Jsonb,
    Other,
}

#[derive(Debug, Clone)]
pub struct PgCol {
    /// `format_type(atttypid, atttypmod)`, mis. `timestamp(3) without time zone`, `"HolidayType"`.
    pub sql_type: String,
    pub kind: PgKind,
}

fn kind_of(sql_type: &str) -> PgKind {
    if sql_type.ends_with("[]") {
        return PgKind::Other;
    }
    if sql_type.starts_with("timestamp") {
        if sql_type.ends_with("with time zone") && !sql_type.ends_with("without time zone") {
            PgKind::Timestamptz
        } else {
            PgKind::Timestamp
        }
    } else if sql_type == "date" {
        PgKind::Date
    } else if sql_type.starts_with("time") {
        PgKind::Time
    } else if sql_type.starts_with("numeric") {
        PgKind::Numeric
    } else if sql_type == "bigint" {
        PgKind::BigInt
    } else if sql_type == "jsonb" || sql_type == "json" {
        PgKind::Jsonb
    } else {
        PgKind::Other
    }
}

/// Kolom setiap tabel di schema aktif.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: HashMap<String, IndexMap<String, PgCol>>,
}

/// Identifier ber-kutip ganda (nama dari DMMF yang sudah divalidasi, bukan input pengguna).
pub fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

impl Catalog {
    pub async fn load(conn: &mut PgConnection) -> sqlx::Result<Self> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod)
                 FROM pg_attribute a
                 JOIN pg_class c ON c.oid = a.attrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p')
                  AND a.attnum > 0 AND NOT a.attisdropped
                ORDER BY c.relname, a.attnum"#,
        )
        .persistent(false)
        .fetch_all(conn)
        .await?;
        let mut tables: HashMap<String, IndexMap<String, PgCol>> = HashMap::new();
        for (table, col, sql_type) in rows {
            let kind = kind_of(&sql_type);
            tables
                .entry(table)
                .or_default()
                .insert(col, PgCol { sql_type, kind });
        }
        Ok(Self { tables })
    }

    pub fn col(&self, table: &str, col: &str) -> Option<&PgCol> {
        self.tables.get(table)?.get(col)
    }

    /// Kolom setiap model `names` harus sama persis dengan field skalar DMMF tertanam. Selisih
    /// berarti `dmmf.json` basi (atau database belum dimigrasi): menulis/membaca dengan peta
    /// kolom yang salah akan diam-diam membuang data, jadi berhenti.
    pub fn verify<'a>(
        &self,
        models: &ModelGraph,
        names: impl IntoIterator<Item = &'a String>,
    ) -> CdResult<()> {
        let mut problems = Vec::new();
        for name in names {
            let Some(info) = models.get(name) else {
                continue;
            };
            let Some(cols) = self.tables.get(&info.table) else {
                problems.push(format!("tabel {} tidak ada", info.table));
                continue;
            };
            for f in &info.scalar_fields {
                if !cols.contains_key(f) {
                    problems.push(format!("{}.{f} tidak ada di database", info.table));
                }
            }
            for c in cols.keys() {
                if !info.field_meta.contains_key(c) {
                    problems.push(format!("{}.{c} tidak dikenal dmmf.json", info.table));
                }
            }
            for (f, meta) in &info.field_meta {
                if meta.is_list
                    && matches!(meta.ty.as_str(), "DateTime" | "Decimal" | "BigInt" | "Json")
                {
                    problems.push(format!(
                        "{}.{f}: list {} belum didukung",
                        info.table, meta.ty
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            fail(
                Code::SchemaMismatch,
                format!(
                    "Kolom database tidak cocok dengan DMMF yang tertanam di mz-admin-jobs ({}).\n  Perbarui crates/mz-admin-jobs/src/company_data/dmmf.json (tools/dump-dmmf.cjs) dari schema.prisma yang sedang dipakai.",
                    problems.join("; ")
                ),
            )
        }
    }

    fn require(&self, info: &ModelInfo, field: &str) -> CdResult<&PgCol> {
        match self.col(&info.table, field) {
            Some(c) => Ok(c),
            None => fail(
                Code::SchemaMismatch,
                format!("{}.{field} tidak ada di database", info.table),
            ),
        }
    }

    /// Ekspresi SELECT satu field dalam bentuk JSON Prisma, beralias nama field.
    pub fn read_expr(&self, info: &ModelInfo, field: &str, alias: &str) -> CdResult<String> {
        let col = self.require(info, field)?;
        let meta = info.field_meta.get(field);
        let c = format!("{alias}.{}", q(field));
        const ISO: &str = r#"'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'"#;
        let expr = match (meta.map(|m| m.ty.as_str()), col.kind) {
            (Some("DateTime"), PgKind::Timestamp) => format!("to_char({c}, {ISO})"),
            (Some("DateTime"), PgKind::Timestamptz) => {
                format!("to_char({c} AT TIME ZONE 'UTC', {ISO})")
            }
            (Some("DateTime"), PgKind::Date) => {
                format!(r#"to_char({c}, 'YYYY-MM-DD"T00:00:00.000Z"')"#)
            }
            (Some("DateTime"), PgKind::Time) => {
                format!("to_char(DATE '1970-01-01' + {c}, {ISO})")
            }
            (Some("Decimal" | "BigInt"), _) => format!("{c}::text"),
            _ if meta.is_some_and(|m| m.is_list) => format!("COALESCE({c}, '{{}}')"),
            _ => c,
        };
        Ok(format!("{expr} AS {}", q(field)))
    }

    /// `SELECT row_to_json(x)::text FROM (SELECT <field...> FROM "t" t WHERE …) x` — bagian
    /// daftar kolom untuk `fields` (urutan dipertahankan).
    pub fn select_list(&self, info: &ModelInfo, fields: &[&str], alias: &str) -> CdResult<String> {
        let mut parts = Vec::with_capacity(fields.len());
        for f in fields {
            parts.push(self.read_expr(info, f, alias)?);
        }
        Ok(parts.join(", "))
    }

    /// Definisi kolom `jsonb_to_recordset(...) AS x(<def>)` dan ekspresi SELECT-nya untuk
    /// menyisipkan `fields`. Kolom `time` dibaca sebagai `timestamp(3)` lalu di-cast (bentuk
    /// Prisma `1970-01-01T08:00:00.000Z` tidak diterima input `time`).
    pub fn insert_parts(&self, info: &ModelInfo, fields: &[String]) -> CdResult<(String, String)> {
        let mut defs = Vec::with_capacity(fields.len());
        let mut exprs = Vec::with_capacity(fields.len());
        for f in fields {
            let col = self.require(info, f)?;
            if col.kind == PgKind::Time {
                defs.push(format!("{} timestamp(3)", q(f)));
                exprs.push(format!("(x.{})::{}", q(f), col.sql_type));
            } else {
                defs.push(format!("{} {}", q(f), col.sql_type));
                exprs.push(format!("x.{}", q(f)));
            }
        }
        Ok((defs.join(", "), exprs.join(", ")))
    }

    /// Tipe SQL kolom (untuk `$1::text[]::<tipe>[]`).
    pub fn sql_type(&self, info: &ModelInfo, field: &str) -> CdResult<String> {
        Ok(self.require(info, field)?.sql_type.clone())
    }
}

/// `Decimal.toJSON()` (decimal.js): tanpa nol di belakang koma.
pub fn normalize_decimal(s: &str) -> String {
    if !s.contains('.') {
        return s.to_owned();
    }
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() || t == "-" {
        "0".to_owned()
    } else {
        t.to_owned()
    }
}

/// Bentuk akhir baris hasil `row_to_json`: `Decimal` dinormalkan, `BigInt` jadi
/// `{"$bigint": "…"}` (`jsonReplacer` backup Node).
pub fn finish_row(info: &ModelInfo, row: &mut Row) {
    for (field, meta) in &info.field_meta {
        let Some(v) = row.get_mut(field) else {
            continue;
        };
        match (meta.ty.as_str(), &v) {
            ("Decimal", Value::String(s)) => *v = Value::String(normalize_decimal(s)),
            ("BigInt", Value::String(s)) => {
                let s = s.clone();
                *v = serde_json::json!({ "$bigint": s });
            }
            _ => {}
        }
    }
}

/// `decodeValue(meta, value)` + kebalikan `jsonReplacer` untuk dikirim ke Postgres lewat
/// `jsonb_to_recordset`: `{"$bigint": "5"}` → `"5"`. Nilai lain sudah dalam bentuk teks
/// yang diterima input Postgres.
pub fn encode_for_insert(meta: &FieldMeta, value: &Value) -> Value {
    if meta.ty == "BigInt"
        && let Some(s) = value.get("$bigint").and_then(Value::as_str)
    {
        return Value::String(s.to_owned());
    }
    value.clone()
}

/// Teks kanonik nilai JSON untuk dibandingkan/diikat sebagai `text` (kunci peta id, nilai unik).
pub fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Object(o) => match o.get("$bigint") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => Some(v.to_string()),
        },
        Value::Array(_) => Some(v.to_string()),
    }
}

/// Kunci identitas nilai seperti `Set`/`Map` JS (`1` dan `"1"` berbeda).
pub fn key_of(v: &Value) -> String {
    v.to_string()
}

/// Ambil baris `row_to_json` (teks) lalu parse.
pub fn parse_rows(info: &ModelInfo, raw: Vec<String>) -> CdResult<Vec<Row>> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let mut row: Row = serde_json::from_str(&r)?;
        finish_row(info, &mut row);
        out.push(row);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decimal_seperti_decimal_js() {
        assert_eq!(normalize_decimal("12.50"), "12.5");
        assert_eq!(normalize_decimal("12.00"), "12");
        assert_eq!(normalize_decimal("0.00"), "0");
        assert_eq!(normalize_decimal("-0.50"), "-0.5");
        assert_eq!(normalize_decimal("100"), "100");
        assert_eq!(normalize_decimal("0.000001"), "0.000001");
    }

    #[test]
    fn jenis_kolom() {
        assert_eq!(kind_of("timestamp(3) without time zone"), PgKind::Timestamp);
        assert_eq!(kind_of("timestamp with time zone"), PgKind::Timestamptz);
        assert_eq!(kind_of("date"), PgKind::Date);
        assert_eq!(kind_of("time without time zone"), PgKind::Time);
        assert_eq!(kind_of("numeric(18,2)"), PgKind::Numeric);
        assert_eq!(kind_of("bigint"), PgKind::BigInt);
        assert_eq!(kind_of("jsonb"), PgKind::Jsonb);
        assert_eq!(kind_of("text[]"), PgKind::Other);
        assert_eq!(kind_of("\"HolidayType\""), PgKind::Other);
    }

    #[test]
    fn teks_kanonik() {
        assert_eq!(text_of(&serde_json::json!(5)), Some("5".into()));
        assert_eq!(
            text_of(&serde_json::json!({"$bigint": "9"})),
            Some("9".into())
        );
        assert_eq!(text_of(&Value::Null), None);
        assert_ne!(
            key_of(&serde_json::json!(1)),
            key_of(&serde_json::json!("1"))
        );
    }
}
