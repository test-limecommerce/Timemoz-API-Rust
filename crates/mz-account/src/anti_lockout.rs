//! Port `permissions/anti-lockout.ts` (§9.6 + §9.8): satu company tidak boleh kehilangan seluruh
//! orang yang punya `roles:WRITE` DAN `users:WRITE`.
//!
//! Urutannya: kunci baris company -> hitung admin SEBELUM -> tulis -> hitung ulang -> tolak.
//! Karena semuanya satu transaksi, penolakan me-rollback tulisannya. Company yang sudah nol admin
//! sebelum perubahan TIDAK dibekukan (yang dilarang adalah MEMBUAT keadaan itu).

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use mz_core::{ApiError, ApiResult};
use serde_json::json;
use sqlx::PgConnection;

pub const LOCKOUT_FEATURE_ROLES: &str = "roles";
pub const LOCKOUT_FEATURE_USERS: &str = "users";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyAdmin {
    pub user_id: i32,
    pub name: String,
    pub username: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Holder {
    #[sqlx(rename = "accessLevelId")]
    pub access_level_id: i32,
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    pub name: String,
    pub username: String,
}

/// Murni: siapa yang berhak, dari pemegang role aktif + hibah `rolePermission` WRITE.
/// `grants` = pasangan (`accessLevelId`, `featureKey`).
pub fn qualified_admins(holders: &[Holder], grants: &[(i32, String)]) -> Vec<CompanyAdmin> {
    let mut by_level: HashMap<i32, HashSet<&str>> = HashMap::new();
    for (level, feature) in grants {
        by_level.entry(*level).or_default().insert(feature.as_str());
    }
    let qualified = |level: i32| {
        by_level.get(&level).is_some_and(|set| {
            set.contains(LOCKOUT_FEATURE_ROLES) && set.contains(LOCKOUT_FEATURE_USERS)
        })
    };
    let mut seen = HashSet::new();
    let mut admins = Vec::new();
    for h in holders {
        if !qualified(h.access_level_id) || !seen.insert(h.user_id) {
            continue;
        }
        admins.push(CompanyAdmin {
            user_id: h.user_id,
            name: h.name.clone(),
            username: h.username.clone(),
        });
    }
    admins
}

/// `LastAdminException` — 422 yang menyebut siapa admin terakhirnya.
pub fn last_admin_error(admins: &[CompanyAdmin]) -> ApiError {
    // `a.name ?? a.username`: `users.name` NOT NULL, jadi selalu `name`.
    let names: Vec<&str> = admins.iter().map(|a| a.name.as_str()).collect();
    let joined = names.join(", ");
    let message = format!(
        "This change would leave the company with nobody who can manage roles and users. \
         Grant another active user 'roles' and 'users' write access first{}.",
        if joined.is_empty() {
            String::new()
        } else {
            format!(" (currently only: {joined})")
        }
    );
    ApiError::body(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "code": "LAST_COMPANY_ADMIN", "lastAdmins": names, "message": message }),
    )
}

/// `lockCompanyRow`: `SELECT id FROM company WHERE id = $1 FOR UPDATE`. Wajib di dalam transaksi.
pub async fn lock_company_row(conn: &mut PgConnection, company_id: i32) -> ApiResult<()> {
    sqlx::query("SELECT id FROM company WHERE id = $1 FOR UPDATE")
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(())
}

/// `companyAdmins(client, companyId)`.
pub async fn company_admins(
    conn: &mut PgConnection,
    company_id: i32,
) -> ApiResult<Vec<CompanyAdmin>> {
    // `users.status = 1`: user yang baru diundang belum bisa login, jadi belum bisa menyelamatkan siapa pun.
    let holders = sqlx::query_as::<_, Holder>(
        r#"SELECT ual."accessLevelId", u.id AS "userId", u.name, u.username
           FROM "userAccessLevel" ual JOIN users u ON u.id = ual."userId"
           WHERE ual."companyId" = $1 AND ual.status = 1 AND u.status = 1
           ORDER BY ual.id"#,
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    if holders.is_empty() {
        return Ok(Vec::new());
    }
    let mut level_ids: Vec<i32> = holders.iter().map(|h| h.access_level_id).collect();
    level_ids.sort_unstable();
    level_ids.dedup();
    let grants: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT "accessLevelId", "featureKey" FROM "rolePermission"
           WHERE "accessLevelId" = ANY($1) AND "featureKey" = ANY($2)
             AND action = 'WRITE'::"PermissionAction""#,
    )
    .bind(&level_ids)
    .bind(vec![LOCKOUT_FEATURE_ROLES, LOCKOUT_FEATURE_USERS])
    .fetch_all(&mut *conn)
    .await?;
    Ok(qualified_admins(&holders, &grants))
}

/// Awal `guardCompanyAdmins`: kunci + hitung admin sebelum perubahan.
pub async fn begin_guard(conn: &mut PgConnection, company_id: i32) -> ApiResult<Vec<CompanyAdmin>> {
    lock_company_row(conn, company_id).await?;
    company_admins(conn, company_id).await
}

/// Akhir `guardCompanyAdmins`: panggil SESUDAH menulis, di transaksi yang sama.
pub async fn finish_guard(
    conn: &mut PgConnection,
    company_id: i32,
    before: &[CompanyAdmin],
) -> ApiResult<()> {
    if before.is_empty() {
        return Ok(());
    }
    if company_admins(conn, company_id).await?.is_empty() {
        return Err(last_admin_error(before));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn holder(level: i32, user: i32, name: &str) -> Holder {
        Holder {
            access_level_id: level,
            user_id: user,
            name: name.into(),
            username: format!("{name}@x.co"),
        }
    }

    fn grant(level: i32, feature: &str) -> (i32, String) {
        (level, feature.to_owned())
    }

    #[test]
    fn needs_both_roles_and_users_write() {
        let holders = vec![holder(1, 10, "a"), holder(2, 20, "b")];
        let grants = vec![
            grant(1, "roles"),
            grant(1, "users"),
            grant(2, "roles"), // hanya satu
        ];
        let admins = qualified_admins(&holders, &grants);
        assert_eq!(admins.len(), 1);
        assert_eq!(admins.first().map(|a| a.user_id), Some(10));
    }

    #[test]
    fn same_user_with_two_rows_counts_once() {
        let holders = vec![holder(1, 10, "a"), holder(1, 10, "a")];
        let grants = vec![grant(1, "roles"), grant(1, "users")];
        assert_eq!(qualified_admins(&holders, &grants).len(), 1);
    }

    #[test]
    fn no_grants_no_admins() {
        assert!(qualified_admins(&[holder(1, 10, "a")], &[]).is_empty());
    }

    #[test]
    fn last_admin_message_names_the_admins() {
        let admins = vec![
            CompanyAdmin {
                user_id: 1,
                name: "Budi".into(),
                username: "b@x.co".into(),
            },
            CompanyAdmin {
                user_id: 2,
                name: "Citra".into(),
                username: "c@x.co".into(),
            },
        ];
        let ApiError::Body { status, body } = last_admin_error(&admins) else {
            return assert_eq!(0, 1);
        };
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "LAST_COMPANY_ADMIN");
        assert_eq!(body["lastAdmins"], json!(["Budi", "Citra"]));
        assert_eq!(
            body["message"],
            "This change would leave the company with nobody who can manage roles and users. \
             Grant another active user 'roles' and 'users' write access first (currently only: Budi, Citra)."
        );
    }

    #[test]
    fn last_admin_message_without_names() {
        let ApiError::Body { body, .. } = last_admin_error(&[]) else {
            return assert_eq!(0, 1);
        };
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.ends_with("write access first."))
        );
    }
}
