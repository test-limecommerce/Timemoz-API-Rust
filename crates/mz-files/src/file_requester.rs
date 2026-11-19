//! Port `src/files/file-requester.ts`: identitas pemanggil proxy file ber-JWT.

use mz_core::company::find_company_id;
use mz_core::enums::{BaseRole, effective_role_code};
use sqlx::PgPool;

/// Company aktif + `effectiveRoleCode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRequester {
    pub user_id: i32,
    pub company_id: Option<i32>,
    pub role_code: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RoleRow {
    code: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
    #[sqlx(rename = "baseRole")]
    base_role: Option<String>,
}

/// `loadFileRequester`: company = aturan `nestedUserCompanyScope`
/// (`mz_core::company::find_company_id`), role = baris `userAccessLevel`
/// terbaru (`id desc`, tanpa filter status — sama dengan Node).
pub async fn load_file_requester(db: &PgPool, user_id: i32) -> sqlx::Result<FileRequester> {
    let role_fut = sqlx::query_as::<_, RoleRow>(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text AS "baseRole"
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1
           ORDER BY ual.id DESC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db);
    let (company_id, role) = tokio::try_join!(find_company_id(db, user_id), role_fut)?;

    let role_code = role.map(|r| {
        let base = r
            .base_role
            .as_deref()
            .and_then(|b| b.parse::<BaseRole>().ok());
        effective_role_code(&r.code, r.is_system, base)
    });
    Ok(FileRequester {
        user_id,
        company_id,
        role_code,
    })
}
