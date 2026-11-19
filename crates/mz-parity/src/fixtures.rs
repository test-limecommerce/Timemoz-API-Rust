//! `tools/parity/fixtures.json`: data uji diambil dari dump (bukan dikarang),
//! supaya kasus berjalan di atas data produksi yang sebenarnya.
//!
//! ```json
//! {
//!   "fixtures": [
//!     { "name": "admin", "sql": "SELECT u.id::text AS id, u.uuid::text AS uuid, ... LIMIT 1" },
//!     { "name": "staff", "sql": "SELECT ... WHERE uc.\"companyId\" = {{admin.company_id}} ..." }
//!   ],
//!   "actors": { "admin": "admin.id", "staff": "staff.id" }
//! }
//! ```
//!
//! Setiap SQL WAJIB mengembalikan tepat satu baris dengan semua kolom bertipe teks
//! (`::text`). Fixture boleh merujuk fixture sebelumnya (`{{nama.kolom}}`).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;
use sqlx::{Column, PgPool, Row};

use crate::case::Vars;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFile {
    pub fixtures: Vec<Fixture>,
    /// Nama aktor → variabel yang berisi `users.id`.
    #[serde(default)]
    pub actors: BTreeMap<String, String>,
    /// Aktor OPERATOR Timemoz (`JwtAdminAuthGuard`): nama aktor → nama fixture yang punya
    /// kolom `id` & `username` dari tabel `operators`. Token = `{ username, sub, iat }`.
    #[serde(default)]
    pub operators: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub name: String,
    pub sql: String,
    /// Opsional: fixture yang boleh kosong (kasus yang memakainya akan error jelas).
    #[serde(default)]
    pub optional: bool,
}

pub fn load(path: &Path) -> anyhow::Result<FixtureFile> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("membaca {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("format {}", path.display()))
}

/// `fixtures.json` + semua `fixtures.d/*.json` (urut nama berkas). Berkas tambahan
/// boleh merujuk fixture di berkas sebelumnya. Nama fixture/aktor ganda = error.
pub fn load_all(dir: &Path) -> anyhow::Result<FixtureFile> {
    let mut all = load(&dir.join("fixtures.json"))?;
    let extra_dir = dir.join("fixtures.d");
    if extra_dir.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(&extra_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let more = load(&f)?;
            for fx in more.fixtures {
                if all.fixtures.iter().any(|x| x.name == fx.name) {
                    bail!(
                        "fixture `{}` di {} sudah didefinisikan",
                        fx.name,
                        f.display()
                    );
                }
                all.fixtures.push(fx);
            }
            for (k, v) in more.actors {
                if all.actors.insert(k.clone(), v).is_some() {
                    bail!("aktor `{k}` di {} sudah didefinisikan", f.display());
                }
            }
            for (k, v) in more.operators {
                if all.actors.contains_key(&k) || all.operators.insert(k.clone(), v).is_some() {
                    bail!(
                        "aktor operator `{k}` di {} sudah didefinisikan",
                        f.display()
                    );
                }
            }
        }
    }
    Ok(all)
}

/// Nilai yang boleh disisipkan ke SQL fixture lain (berasal dari DB, tapi tetap
/// dibatasi supaya tidak ada injeksi tak sengaja).
fn safe_sql_literal(v: &str) -> bool {
    v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:@ +".contains(c))
}

fn substitute(sql: &str, vars: &Vars, fixture: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(start) = rest.find("{{") {
        let (before, after) = rest.split_at(start);
        let Some(end) = after.find("}}") else {
            bail!("placeholder tidak ditutup di fixture `{fixture}`")
        };
        let key = after.get(2..end).unwrap_or_default();
        let value = vars
            .get(key)
            .with_context(|| format!("fixture `{fixture}` merujuk `{key}` yang belum ada"))?;
        if !safe_sql_literal(value) {
            bail!("nilai `{key}` tidak aman untuk disisipkan ke SQL: {value:?}");
        }
        out.push_str(before);
        out.push_str(value);
        rest = after.get(end + 2..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(out)
}

pub async fn resolve(
    db: &PgPool,
    file: &FixtureFile,
    vars: &mut Vars,
) -> anyhow::Result<Vec<String>> {
    let mut missing = Vec::new();
    for f in &file.fixtures {
        let sql = substitute(&f.sql, vars, &f.name)?;
        let row = sqlx::query(&sql)
            .fetch_optional(db)
            .await
            .with_context(|| format!("menjalankan fixture `{}`", f.name))?;
        let Some(row) = row else {
            if f.optional {
                missing.push(f.name.clone());
                continue;
            }
            bail!(
                "fixture `{}` tidak menemukan baris di dump — sesuaikan SQL-nya",
                f.name
            );
        };
        for col in row.columns() {
            let v: Option<String> = row.try_get(col.ordinal()).with_context(|| {
                format!(
                    "kolom `{}` di fixture `{}` harus `::text`",
                    col.name(),
                    f.name
                )
            })?;
            vars.insert(format!("{}.{}", f.name, col.name()), v.unwrap_or_default());
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_rejects_unsafe_values() {
        let mut vars = Vars::new();
        vars.insert("a.id".into(), "7".into());
        assert_eq!(
            substitute("x = {{a.id}}", &vars, "f").ok().as_deref(),
            Some("x = 7")
        );
        vars.insert("a.bad".into(), "1; DROP TABLE users".into());
        assert!(substitute("x = {{a.bad}}", &vars, "f").is_err());
        assert!(substitute("x = {{a.none}}", &vars, "f").is_err());
    }
}
