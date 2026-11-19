//! `GET /health` + pemeriksaan matriks role bawaan saat boot.
//!
//! Node: `src/health.controller.ts` (`{ status: 'ok', permissions }`) dan
//! `src/permissions/built-in-matrix-check.ts` (`BuiltInMatrixCheck`, dijalankan sekali di
//! `onApplicationBootstrap`, tidak pernah menggagalkan start).
//!
//! Matriks bawaan kosong membuat penegakan izin diam-diam lolos untuk role itu (R12);
//! pemeriksaan ini membuatnya terlihat: log error + `permissions: "degraded"`.
//!
//! DEVIASI: Rust menambah `runtime: "rust"` supaya terlihat proses mana yang menjawab
//! sesudah cutover/rollback (`plan/deviations/mz-core.md` #18).

use std::sync::atomic::{AtomicU8, Ordering};

use mz_core::AppState;

const UNKNOWN: u8 = 0;
const OK: u8 = 1;
const DEGRADED: u8 = 2;

static STATUS: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Node `CHECKED_ROLES`: semua `Role` kecuali `SYSTEM_ADMIN` (ia melewati matriks).
const CHECKED_ROLES: [&str; 5] = ["SUPER_ADMIN", "ADMIN", "MANAGER", "STAFF", "GUEST"];

fn label(v: u8) -> &'static str {
    match v {
        OK => "ok",
        DEGRADED => "degraded",
        _ => "unknown",
    }
}

/// Status terakhir: `ok` | `degraded` | `unknown` (belum/ gagal diperiksa).
pub fn permissions_status() -> &'static str {
    label(STATUS.load(Ordering::Relaxed))
}

/// Roles bawaan (`companyId IS NULL`) yang tidak punya satu pun `rolePermission`.
fn empty_roles(counts: &[(String, i64)]) -> Vec<&'static str> {
    CHECKED_ROLES
        .iter()
        .copied()
        .filter(|code| {
            counts
                .iter()
                .filter(|(c, _)| c == code)
                .map(|(_, n)| *n)
                .sum::<i64>()
                == 0
        })
        .collect()
}

/// Node `BuiltInMatrixCheck.run`. Tidak pernah gagal; hasil disimpan untuk `/health`.
pub async fn check_builtin_matrix(state: &AppState) -> &'static str {
    let rows = sqlx::query_as::<_, (String, i64)>(
        r#"SELECT a.code, COUNT(rp.id)
             FROM "accessLevel" a
             LEFT JOIN "rolePermission" rp ON rp."accessLevelId" = a.id
            WHERE a."companyId" IS NULL AND a.code = ANY($1)
            GROUP BY a.id, a.code"#,
    )
    .bind(CHECKED_ROLES.as_slice())
    .fetch_all(&state.db)
    .await;
    let next = match rows {
        Ok(counts) => {
            let empty = empty_roles(&counts);
            if empty.is_empty() {
                OK
            } else {
                tracing::error!(
                    "BUILT_IN_MATRIX_EMPTY roles={} — penegakan izin tidak berjalan untuk role ini; jalankan prisma/backfill-role-permissions.js",
                    empty.join(",")
                );
                DEGRADED
            }
        }
        Err(err) => {
            tracing::error!("Pemeriksaan matriks role bawaan gagal: {err}");
            UNKNOWN
        }
    };
    STATUS.store(next, Ordering::Relaxed);
    label(next)
}

/// Node: `src/health.controller.ts:15`.
pub async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "permissions": permissions_status(),
        "runtime": "rust",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roles_lists_missing_and_zero() {
        let counts = vec![
            ("SUPER_ADMIN".to_owned(), 10),
            ("ADMIN".to_owned(), 0),
            ("MANAGER".to_owned(), 3),
            ("STAFF".to_owned(), 1),
        ];
        assert_eq!(empty_roles(&counts), vec!["ADMIN", "GUEST"]);
    }

    #[test]
    fn empty_roles_sums_duplicate_codes() {
        let counts: Vec<(String, i64)> = CHECKED_ROLES
            .iter()
            .flat_map(|c| [((*c).to_owned(), 0), ((*c).to_owned(), 2)])
            .collect();
        assert!(empty_roles(&counts).is_empty());
    }

    #[test]
    fn default_status_unknown() {
        assert_eq!(label(UNKNOWN), "unknown");
        assert_eq!(label(9), "unknown");
    }
}
