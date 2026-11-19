//! Baris Postgres → objek JSON berbentuk hasil Prisma (semua kolom skalar,
//! nama kolom apa adanya). Konversi berdasarkan TIPE kolom, bukan nama:
//! `timestamp` → `2026-11-16T08:00:00.000Z`, `date`/`time` → `Date` JS,
//! `numeric` → string (Prisma `Decimal`), `float8` → angka JS, enum → string.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use mz_core::time::{JsDate, to_js_iso};
use serde_json::{Map, Value};
use sqlx::postgres::{PgRow, PgTypeKind, PgValueRef};
use sqlx::{Column, Decode, Postgres, Row, TypeInfo, ValueRef};

use crate::js;

type DecodeResult = Result<Value, sqlx::error::BoxDynError>;

fn dec<'r, T: Decode<'r, Postgres>>(raw: PgValueRef<'r>) -> Result<T, sqlx::error::BoxDynError> {
    T::decode(raw)
}

fn date_value(d: NaiveDate) -> Value {
    Value::String(format!("{}T00:00:00.000Z", d.format("%Y-%m-%d")))
}

fn time_value(t: NaiveTime) -> Value {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .map(|d| Value::String(JsDate(d.and_time(t)).to_js_string()))
        .unwrap_or(Value::Null)
}

fn scalar(type_name: &str, raw: PgValueRef<'_>) -> DecodeResult {
    Ok(match type_name {
        "INT2" => Value::from(dec::<i16>(raw)?),
        "INT4" => Value::from(dec::<i32>(raw)?),
        "INT8" => Value::from(dec::<i64>(raw)?),
        "FLOAT4" => js::num(f64::from(dec::<f32>(raw)?)),
        "FLOAT8" => js::num(dec::<f64>(raw)?),
        "NUMERIC" => Value::String(dec::<rust_decimal::Decimal>(raw)?.normalize().to_string()),
        "BOOL" => Value::Bool(dec::<bool>(raw)?),
        "UUID" => Value::String(dec::<uuid::Uuid>(raw)?.hyphenated().to_string()),
        "TIMESTAMP" => Value::String(JsDate(dec::<NaiveDateTime>(raw)?).to_js_string()),
        "TIMESTAMPTZ" => Value::String(to_js_iso(&dec::<DateTime<Utc>>(raw)?)),
        "DATE" => date_value(dec::<NaiveDate>(raw)?),
        "TIME" => time_value(dec::<NaiveTime>(raw)?),
        "JSON" | "JSONB" => dec::<Value>(raw)?,
        "TEXT[]" | "VARCHAR[]" | "BPCHAR[]" => Value::from(
            dec::<Vec<Option<String>>>(raw)?
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>(),
        ),
        "INT4[]" => Value::from(
            dec::<Vec<Option<i32>>>(raw)?
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>(),
        ),
        "INT8[]" => Value::from(
            dec::<Vec<Option<i64>>>(raw)?
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>(),
        ),
        "BOOL[]" => Value::from(
            dec::<Vec<Option<bool>>>(raw)?
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>(),
        ),
        // TEXT, VARCHAR, BPCHAR, NAME, enum Prisma (biner = label teks), dan sisanya.
        _ => Value::String(raw.as_str()?.to_owned()),
    })
}

/// Nilai NULL per tipe kolom: kolom daftar (`text[]`, `int4[]`, ...) NULL dibaca Prisma sebagai `[]`.
fn null_value(type_name: &str) -> Value {
    if type_name.ends_with("[]") {
        Value::Array(Vec::new())
    } else {
        Value::Null
    }
}

/// Satu baris → objek JSON (semua kolom). Gagal decode = 500.
pub fn row_to_map(row: &PgRow) -> Result<Map<String, Value>, sqlx::Error> {
    let mut map = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(i)?;
        let value = if raw.is_null() {
            null_value(col.type_info().name())
        } else {
            let info = col.type_info();
            let name = match info.kind() {
                PgTypeKind::Enum(_) => "ENUM",
                _ => info.name(),
            };
            scalar(name, raw).map_err(|source| sqlx::Error::ColumnDecode {
                index: col.name().to_owned(),
                source,
            })?
        };
        map.insert(col.name().to_owned(), value);
    }
    Ok(map)
}

pub fn row_to_value(row: &PgRow) -> Result<Value, sqlx::Error> {
    row_to_map(row).map(Value::Object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_list_columns_become_empty_array() {
        assert_eq!(null_value("TEXT[]"), Value::Array(vec![]));
        assert_eq!(null_value("INT4[]"), Value::Array(vec![]));
        assert_eq!(null_value("TEXT"), Value::Null);
    }

    #[test]
    fn date_and_time_shapes() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap_or_default();
        assert_eq!(
            date_value(d),
            Value::String("2026-11-16T00:00:00.000Z".into())
        );
        let t = NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default();
        assert_eq!(
            time_value(t),
            Value::String("1970-01-01T00:00:00.000Z".into())
        );
    }
}
