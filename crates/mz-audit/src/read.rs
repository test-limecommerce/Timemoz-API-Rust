//! Pembaca snapshot: padanan `findFirst`/`findMany` Prisma untuk model ber-audit.
//!
//! Setiap kolom dibaca lewat `to_jsonb` lalu dinormalkan per jenis kolom
//! ([`crate::row::Kind`]) supaya nilainya sama persis dengan yang ditulis Node ke kolom
//! `Json` (`DateTime` → ISO ms `Z`, `Decimal`/`BigInt` → string). Urutan kunci = urutan
//! kolom di `schema.prisma` ([`crate::columns`]).

use std::sync::Arc;

use serde_json::Value;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::columns::{COLUMNS, Column};
use crate::policy::snapshot_select;
use crate::row::{Kind, Row, normalize_db_value};
use crate::scope::Model;

type FilterFn = dyn for<'a> Fn(&mut QueryBuilder<'a, Postgres>) + Send + Sync;

/// Klausa `where` pembacaan. Kolom ditulis tanpa alias tabel (`"userId" = ...`); tabel
/// luarnya bisa dirujuk dengan namanya sendiri (`"users".id`) di dalam subquery.
#[derive(Clone)]
pub enum Lookup {
    Id(i32),
    Ids(Vec<i32>),
    Uuid(String),
    /// SQL bebas yang dipasang ke `QueryBuilder` (nilai WAJIB lewat `push_bind`).
    Filter(Arc<FilterFn>),
}

impl Lookup {
    /// `Lookup::filter(|qb| { qb.push(r#""userId" = "#).push_bind(5); })`.
    pub fn filter(
        f: impl for<'a> Fn(&mut QueryBuilder<'a, Postgres>) + Send + Sync + 'static,
    ) -> Self {
        Lookup::Filter(Arc::new(f))
    }

    pub(crate) fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        match self {
            Lookup::Id(id) => {
                qb.push("id = ").push_bind(*id);
            }
            Lookup::Ids(ids) => {
                qb.push("id = ANY(").push_bind(ids.clone()).push(")");
            }
            Lookup::Uuid(uuid) => {
                qb.push("uuid = ").push_bind(uuid.clone()).push("::uuid");
            }
            Lookup::Filter(f) => {
                qb.push("(");
                f(qb);
                qb.push(")");
            }
        }
    }
}

impl std::fmt::Debug for Lookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lookup::Id(id) => write!(f, "Id({id})"),
            Lookup::Ids(ids) => write!(f, "Ids({ids:?})"),
            Lookup::Uuid(u) => write!(f, "Uuid({u})"),
            Lookup::Filter(_) => f.write_str("Filter(..)"),
        }
    }
}

/// Kolom yang dibaca.
#[derive(Debug, Clone, Copy)]
pub enum Columns<'a> {
    /// Semua kolom skalar = hasil Prisma tanpa `select`.
    All,
    /// `selectArg(model)` pra-baca: allowlist + kolom internal, atau semua kolom (denylist).
    Snapshot,
    Only(&'a [&'a str]),
}

pub(crate) fn columns_of(model: Model) -> Option<&'static [Column]> {
    COLUMNS
        .iter()
        .find(|(m, _)| *m == model.as_str())
        .map(|(_, c)| *c)
}

fn selected(model: Model, cols: Columns<'_>) -> Vec<(&'static str, Kind, bool)> {
    let all = columns_of(model).unwrap_or(&[]);
    let wanted: Option<Vec<&str>> = match cols {
        Columns::All => None,
        Columns::Snapshot => snapshot_select(model),
        Columns::Only(only) => Some(only.to_vec()),
    };
    all.iter()
        .copied()
        .filter(|(name, _, _)| wanted.as_ref().is_none_or(|w| w.contains(name)))
        .collect()
}

/// `SELECT ... FROM "model" WHERE <lookup> [LIMIT n]` → baris ternormalisasi.
pub async fn read_rows<'e>(
    exec: impl PgExecutor<'e>,
    model: Model,
    cols: Columns<'_>,
    lookup: &Lookup,
    extra: Option<&str>,
    limit: Option<i64>,
) -> sqlx::Result<Vec<Row>> {
    let spec = selected(model, cols);
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb = QueryBuilder::<Postgres>::new("SELECT to_jsonb(t) FROM (SELECT ");
    for (i, (name, kind, list)) in spec.iter().enumerate() {
        if i > 0 {
            qb.push(", ");
        }
        // Decimal/BigInt sebagai teks: presisinya utuh, dan Node menulisnya sebagai string.
        if matches!(kind, Kind::Decimal | Kind::BigInt) && !list {
            qb.push(format!(r#""{name}"::text AS "{name}""#));
        } else {
            qb.push(format!(r#""{name}""#));
        }
    }
    qb.push(format!(r#" FROM "{}" WHERE "#, model.as_str()));
    lookup.push(&mut qb);
    if let Some(extra) = extra {
        qb.push(" AND ").push(extra);
    }
    if let Some(limit) = limit {
        qb.push(" LIMIT ").push_bind(limit);
    }
    qb.push(") t");
    let raw: Vec<sqlx::types::Json<Value>> = qb.build_query_scalar().fetch_all(exec).await?;
    Ok(raw.into_iter().map(|j| to_row(&spec, j.0)).collect())
}

fn to_row(spec: &[(&'static str, Kind, bool)], mut value: Value) -> Row {
    let mut row = Row::new();
    let Some(obj) = value.as_object_mut() else {
        return row;
    };
    for (name, kind, list) in spec {
        if row.contains(name) {
            continue;
        }
        let v = obj.remove(*name).unwrap_or(Value::Null);
        row.set(name, normalize_db_value(*kind, *list, v));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_columns_follow_schema_order() {
        let names: Vec<&str> = selected(Model::Users, Columns::Snapshot)
            .iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(names, vec!["id", "uuid", "username", "name", "status"]);
        let all = selected(Model::Company, Columns::Snapshot);
        assert!(all.len() > 50);
    }

    #[test]
    fn to_row_orders_and_normalizes() {
        let spec = selected(Model::UserAccessLevel, Columns::All);
        let row = to_row(
            &spec,
            json!({"status": 1, "id": 2, "userId": 5, "accessLevelId": 3, "companyId": 7,
                   "uuid": "u", "createdAt": "2026-01-01T00:00:00", "updatedAt": null}),
        );
        assert_eq!(
            row.keys().collect::<Vec<_>>(),
            vec![
                "id",
                "uuid",
                "userId",
                "accessLevelId",
                "companyId",
                "createdAt",
                "updatedAt",
                "status"
            ]
        );
        assert_eq!(
            row.get("createdAt"),
            Some(&json!("2026-01-01T00:00:00.000Z"))
        );
    }
}
