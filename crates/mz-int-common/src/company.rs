//! `resolveCompanyId(userId)` integrasi (ClickUp/Jira/Discord; teks Node identik).

use mz_core::AppState;

/// Node (`clickup.service.ts:77`, sama di `jira.service.ts`, `discord.service.ts`):
/// `userAccessLevel` aktif dengan `roleCodeFilter([SYSTEM_ADMIN, SUPER_ADMIN, ADMIN])`
/// (role sistem ber-code itu ATAU role custom ber-`baseRole` ADMIN). Nol → `None`, satu →
/// company itu, lebih → `findCompanyIdAmong` atas id terurut, cadangan id terkecil.
pub async fn admin_company_id(state: &AppState, user_id: i32) -> sqlx::Result<Option<i32>> {
    let mut ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT ual."companyId" FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual.status = 1
             AND ((al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN'))
                  OR (NOT al."isSystem" AND al."baseRole"::text = 'ADMIN'))"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    match ids.as_slice() {
        [] => return Ok(None),
        [one] => return Ok(Some(*one)),
        _ => {}
    }
    ids.sort_unstable();
    let among = mz_core::company::find_company_id_among(&state.db, user_id, &ids).await?;
    Ok(among.or_else(|| ids.first().copied()))
}
