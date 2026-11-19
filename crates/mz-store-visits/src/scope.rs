//! Port `src/store-visits/store-scope.service.ts` (§3) — pembatasan wilayah seluruh jalur
//! mobile, **satu sumber dua bentuk**: klausa daftar ([`push_scope_clause`]) dan predikat
//! satu-toko ([`is_in_scope`]) lahir dari tri-state yang sama. Dipakai juga `mz-orders`.
//!
//! Tri-state ([`StoreScope`] = `Option<Scope>`):
//! - `Some(Scope { group_ids: non-kosong, .. })` → terikat wilayah itu;
//! - `Some(Scope { group_ids: [], .. })` → dibatasi tanpa wilayah (§0.23);
//! - `None` → TIDAK dibatasi (§0.5).
//!
//! `strict` = `company.storeVisitEnforceGroupScope`: toko tanpa region lolos untuk siapa pun
//! bila `false`, tidak untuk siapa pun bila `true` (§0.6).

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use mz_core::{ApiError, ApiResult};
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::geo_util::get_distance;

/// Wilayah caller yang dibatasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub group_ids: Vec<i32>,
    pub strict: bool,
}

/// `StoreScope` Node (`null` = tidak dibatasi).
pub type StoreScope = Option<Scope>;

/// `ScopePosition`: posisi yang dilaporkan caller (rute tanpa koordinat → `None`, audit memakai 0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopePosition {
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: f64,
    pub is_mock: Option<bool>,
}

/// Toko yang ditolak (cukup untuk memutuskan dan mengauditnya).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeStore {
    pub id: i32,
    pub group_id: Option<i32>,
    pub lat: f64,
    pub lng: f64,
}

/// `ScopeDenial`.
#[derive(Debug, Clone)]
pub struct ScopeDenial<'a> {
    pub scope: &'a StoreScope,
    pub company_id: i32,
    pub user_id: i32,
    pub store: ScopeStore,
    pub position: Option<ScopePosition>,
    pub client_request_id: Option<&'a str>,
}

/// `storeVisitAuditLog.reason` — VarChar(255).
const REASON_MAX_LENGTH: usize = 255;

/// `company.storeVisitEnforceGroupScope` (baris tak terbaca → `false`).
pub async fn read_enforce_flag(db: &PgPool, company_id: i32) -> ApiResult<bool> {
    let v: Option<bool> =
        sqlx::query_scalar(r#"SELECT "storeVisitEnforceGroupScope" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    Ok(v.unwrap_or(false))
}

/// `assignedGroupIds`: assignment hidup (`deletedAt IS NULL`) ke region AKTIF company caller.
pub async fn assigned_group_ids(db: &PgPool, user_id: i32, company_id: i32) -> ApiResult<Vec<i32>> {
    Ok(sqlx::query_scalar(
        r#"SELECT sgu."groupId" FROM "storeGroupUser" sgu
             JOIN "storeGroup" g ON g.id = sgu."groupId"
            WHERE sgu."userId" = $1 AND sgu."deletedAt" IS NULL
              AND g."companyId" = $2 AND g."isActive" = true
            ORDER BY sgu.id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(db)
    .await?)
}

/// `foldScope`: satu-satunya tempat "daftar kosong" diberi arti (§0.23).
pub fn fold_scope(group_ids: Vec<i32>, enforce: bool) -> StoreScope {
    if group_ids.is_empty() && !enforce {
        return None;
    }
    Some(Scope {
        group_ids,
        strict: enforce,
    })
}

/// §3.1 `resolveStoreScope(userId, companyId, enforce?)`. `enforce = None` → bendera dibaca
/// berbarengan dengan assignment.
pub async fn resolve_store_scope(
    db: &PgPool,
    user_id: i32,
    company_id: i32,
    enforce: Option<bool>,
) -> ApiResult<StoreScope> {
    if let Some(enforce) = enforce {
        return Ok(fold_scope(
            assigned_group_ids(db, user_id, company_id).await?,
            enforce,
        ));
    }
    let (flag, ids) = tokio::try_join!(
        read_enforce_flag(db, company_id),
        assigned_group_ids(db, user_id, company_id)
    )?;
    Ok(fold_scope(ids, flag))
}

/// §3.2b `isInScope(scope, storeGroupId)`.
pub fn is_in_scope(scope: &StoreScope, store_group_id: Option<i32>) -> bool {
    match (scope, store_group_id) {
        (None, _) => true,
        (Some(s), None) => !s.strict,
        (Some(s), Some(g)) => s.group_ids.contains(&g),
    }
}

/// §3.2c `usersInScopeFor(companyId, storeGroupId, candidateUserIds)`: tepat himpunan user yang
/// `is_in_scope`-nya lolos untuk toko itu.
pub async fn users_in_scope_for(
    db: &PgPool,
    company_id: i32,
    store_group_id: Option<i32>,
    candidate_user_ids: &[i32],
) -> ApiResult<HashSet<i32>> {
    if candidate_user_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows_fut = async {
        if store_group_id.is_none() {
            return Ok::<Vec<(i32, i32)>, ApiError>(Vec::new());
        }
        Ok(sqlx::query_as(
            r#"SELECT sgu."userId", sgu."groupId" FROM "storeGroupUser" sgu
                 JOIN "storeGroup" g ON g.id = sgu."groupId"
                WHERE sgu."userId" = ANY($1) AND sgu."deletedAt" IS NULL
                  AND g."companyId" = $2 AND g."isActive" = true
                ORDER BY sgu.id"#,
        )
        .bind(candidate_user_ids)
        .bind(company_id)
        .fetch_all(db)
        .await?)
    };
    let (enforce, rows) = tokio::try_join!(read_enforce_flag(db, company_id), rows_fut)?;
    Ok(users_in_scope_from_rows(
        enforce,
        store_group_id,
        candidate_user_ids,
        &rows,
    ))
}

/// Bagian murni [`users_in_scope_for`].
pub fn users_in_scope_from_rows(
    enforce: bool,
    store_group_id: Option<i32>,
    candidate_user_ids: &[i32],
    rows: &[(i32, i32)],
) -> HashSet<i32> {
    if store_group_id.is_none() {
        return if enforce {
            HashSet::new()
        } else {
            candidate_user_ids.iter().copied().collect()
        };
    }
    let mut by_user: HashMap<i32, Vec<i32>> = HashMap::new();
    for (uid, gid) in rows {
        by_user.entry(*uid).or_default().push(*gid);
    }
    candidate_user_ids
        .iter()
        .copied()
        .filter(|uid| {
            is_in_scope(
                &fold_scope(by_user.get(uid).cloned().unwrap_or_default(), enforce),
                store_group_id,
            )
        })
        .collect()
}

/// §3.2a `scopeWhereClause(scope)` sebagai SQL: tulis ` AND (...)` atas kolom `group_col`
/// (mis. `s."groupId"`). `None` → TIDAK menulis apa pun (query identik dengan sebelum fitur ada).
/// Strict → `col = ANY(ids)`; longgar → `(col = ANY(ids) OR col IS NULL)`.
pub fn push_scope_clause(qb: &mut QueryBuilder<'_, Postgres>, scope: &StoreScope, group_col: &str) {
    let Some(s) = scope else {
        return;
    };
    qb.push(" AND (");
    qb.push(group_col);
    qb.push(" = ANY(");
    qb.push_bind(s.group_ids.clone());
    qb.push(")");
    if !s.strict {
        qb.push(" OR ");
        qb.push(group_col);
        qb.push(" IS NULL");
    }
    qb.push(")");
}

/// Bentuk klausa Prisma (`scopeWhereClause`) sebagai JSON — untuk uji kesetaraan dua bentuk.
pub fn scope_where_json(scope: &StoreScope) -> Option<serde_json::Value> {
    let s = scope.as_ref()?;
    if s.strict {
        return Some(json!({ "groupId": { "in": s.group_ids } }));
    }
    Some(json!({ "OR": [{ "groupId": { "in": s.group_ids } }, { "groupId": null }] }))
}

/// 403 `STORE_OUT_OF_SCOPE`.
pub fn out_of_scope_error() -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "STORE_OUT_OF_SCOPE",
        "This store is outside the region assigned to you",
    )
}

/// Jalur satu-toko `assertInScope`: lolos diam-diam, atau 403 yang SELALU meninggalkan satu baris
/// audit `outOfScope` (§0.19).
pub async fn assert_in_scope(db: &PgPool, denial: ScopeDenial<'_>) -> ApiResult<()> {
    if is_in_scope(denial.scope, denial.store.group_id) {
        return Ok(());
    }
    Err(deny_out_of_scope(db, denial).await)
}

/// `denyOutOfScope`: tulis audit (best-effort), lalu kembalikan galatnya untuk dilempar pemanggil.
pub async fn deny_out_of_scope(db: &PgPool, denial: ScopeDenial<'_>) -> ApiError {
    audit_out_of_scope(db, &denial).await;
    out_of_scope_error()
}

async fn audit_out_of_scope(db: &PgPool, denial: &ScopeDenial<'_>) {
    let pos = denial.position;
    let distance = pos.map_or(0, |p| {
        get_distance(p.lat, p.lng, denial.store.lat, denial.store.lng)
    });
    let res: ApiResult<()> = async {
        let reason = scope_reason(db, denial.scope).await?;
        sqlx::query(
            r#"INSERT INTO "storeVisitAuditLog"
                 ("companyId", "storeId", "userId", "visitId", event, lat, lng, "accuracyMeters",
                  "distanceMeters", "isMock", reason, "clientRequestId")
               VALUES ($1, $2, $3, NULL, 'outOfScope'::"StoreVisitAuditEvent", $4, $5, $6, $7, $8, $9,
                       $10::uuid)"#,
        )
        .bind(denial.company_id)
        .bind(denial.store.id)
        .bind(denial.user_id)
        .bind(pos.map_or(0.0, |p| p.lat))
        .bind(pos.map_or(0.0, |p| p.lng))
        .bind(pos.map_or(0.0, |p| p.accuracy_meters))
        .bind(i32::try_from(distance).unwrap_or(i32::MAX))
        .bind(pos.and_then(|p| p.is_mock).unwrap_or(false))
        .bind(reason)
        .bind(denial.client_request_id)
        .execute(db)
        .await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(
            company_id = denial.company_id,
            store_id = denial.store.id,
            user_id = denial.user_id,
            scope = ?denial.scope,
            "storeVisitAuditLog outOfScope write failed: {e:?}"
        );
    }
}

/// §2.5 `scopeReason`: kode region caller (urut `code`), dipangkas 255; kosong → `None`.
async fn scope_reason(db: &PgPool, scope: &StoreScope) -> ApiResult<Option<String>> {
    let Some(s) = scope.as_ref().filter(|s| !s.group_ids.is_empty()) else {
        return Ok(None);
    };
    let codes: Vec<String> =
        sqlx::query_scalar(r#"SELECT code FROM "storeGroup" WHERE id = ANY($1) ORDER BY code ASC"#)
            .bind(&s.group_ids)
            .fetch_all(db)
            .await?;
    Ok(reason_from_codes(&codes))
}

/// Bagian murni `scopeReason` (`codes.join(',').slice(0, 255) || null`, panjang UTF-16).
pub fn reason_from_codes(codes: &[String]) -> Option<String> {
    let joined = codes.join(",");
    let units: Vec<u16> = joined.encode_utf16().take(REASON_MAX_LENGTH).collect();
    let s = String::from_utf16_lossy(&units);
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(ids: &[i32], strict: bool) -> StoreScope {
        Some(Scope {
            group_ids: ids.to_vec(),
            strict,
        })
    }

    #[test]
    fn fold_tri_state() {
        assert_eq!(fold_scope(vec![], false), None);
        assert_eq!(fold_scope(vec![], true), scope(&[], true));
        assert_eq!(fold_scope(vec![1, 2], false), scope(&[1, 2], false));
        assert_eq!(fold_scope(vec![3], true), scope(&[3], true));
    }

    /// "the two forms agree (§3.2)": predikat vs klausa Prisma, untuk tiap kombinasi.
    #[test]
    fn two_forms_agree() {
        let clause_matches = |s: &StoreScope, gid: Option<i32>| -> bool {
            match scope_where_json(s) {
                None => true,
                Some(_) => {
                    let sc = s.as_ref().unwrap();
                    gid.is_some_and(|g| sc.group_ids.contains(&g)) || (!sc.strict && gid.is_none())
                }
            }
        };
        let scopes = [
            None,
            scope(&[], false),
            scope(&[1], false),
            scope(&[], true),
            scope(&[1], true),
        ];
        for s in &scopes {
            for gid in [None, Some(1), Some(2)] {
                assert_eq!(is_in_scope(s, gid), clause_matches(s, gid), "{s:?} {gid:?}");
            }
        }
        assert_eq!(scope_where_json(&None), None);
        assert_eq!(
            scope_where_json(&scope(&[], false)),
            Some(json!({ "OR": [{ "groupId": { "in": [] } }, { "groupId": null }] }))
        );
    }

    #[test]
    fn strict_drops_region_less_pass() {
        assert!(!is_in_scope(&scope(&[1], true), None));
        assert!(is_in_scope(&scope(&[1], false), None));
        assert!(!is_in_scope(&scope(&[], true), Some(1)));
        assert!(is_in_scope(&None, None));
    }

    #[test]
    fn users_in_scope_pure() {
        let rows = vec![(10, 1), (11, 2)];
        let got = users_in_scope_from_rows(false, Some(1), &[10, 11, 12], &rows);
        assert_eq!(got, HashSet::from([10, 12]));
        let got = users_in_scope_from_rows(true, Some(1), &[10, 11, 12], &rows);
        assert_eq!(got, HashSet::from([10]));
        assert!(users_in_scope_from_rows(true, None, &[10], &rows).is_empty());
        assert_eq!(
            users_in_scope_from_rows(false, None, &[10, 11], &rows),
            HashSet::from([10, 11])
        );
    }

    #[test]
    fn reason_codes() {
        assert_eq!(reason_from_codes(&[]), None);
        assert_eq!(
            reason_from_codes(&["A".into(), "B".into()]).as_deref(),
            Some("A,B")
        );
        let long: Vec<String> = (0..100).map(|i| format!("CODE{i:03}")).collect();
        assert_eq!(reason_from_codes(&long).unwrap().len(), 255);
    }

    #[test]
    fn sql_clause_shape() {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM store s WHERE true");
        push_scope_clause(&mut qb, &None, r#"s."groupId""#);
        assert_eq!(qb.sql(), "SELECT 1 FROM store s WHERE true");
        push_scope_clause(&mut qb, &scope(&[1], false), r#"s."groupId""#);
        assert!(
            qb.sql()
                .ends_with(r#" AND (s."groupId" = ANY($1) OR s."groupId" IS NULL)"#)
        );
    }
}
