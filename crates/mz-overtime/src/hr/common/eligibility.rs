//! Port `src/overtime/overtime-eligibility.ts`: kelayakan karyawan lembur (user aktif,
//! keanggotaan aktif di company, penugasan aktif ber-role efektif selain GUEST). Dipakai
//! pengajuan SELF dan perintah ORDER, supaya keduanya tak bisa berbeda.

use mz_core::enums::{BaseRole, effective_role_code, rank_of};
use sqlx::PgExecutor;

use super::constants::ACTIVE;

/// Role penugasan aktif tertua (`userAccessLevel[0].accessLevel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmployeeRole {
    pub code: String,
    pub is_system: bool,
    pub base_role: Option<BaseRole>,
    /// `accessLevel.status`.
    pub status: i32,
}

/// `OvertimeEmployeeRow` (`overtimeEmployeeSelect` + `username`/`uuid` untuk email/respons).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OvertimeEmployeeRow {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    /// Email login (`users.username`).
    pub username: String,
    /// `None` = tak ada penugasan aktif di company ini (atau relasi `accessLevel` hilang).
    pub role: Option<EmployeeRole>,
}

#[derive(sqlx::FromRow)]
struct DbRow {
    id: i32,
    uuid: String,
    name: String,
    username: String,
    code: Option<String>,
    is_system: Option<bool>,
    base_role: Option<BaseRole>,
    al_status: Option<i32>,
}

/// `users.findMany({ where: overtimeEmployeeWhere(companyId, ids), select:
/// overtimeEmployeeSelect(companyId) })`: user aktif + keanggotaan aktif di company, dengan
/// penugasan aktif tertua (`orderBy id asc, take 1`) di company itu. Hasil urut `users.id`;
/// id yang tak memenuhi `where` tidak muncul.
pub async fn load_overtime_employees<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<Vec<OvertimeEmployeeRow>> {
    let rows: Vec<DbRow> = sqlx::query_as(
        r#"SELECT u.id, u.uuid::text AS uuid, u.name, u.username,
                  a.code, a.is_system, a.base_role, a.al_status
           FROM users u
           LEFT JOIN LATERAL (
             SELECT al.code, al."isSystem" AS is_system, al."baseRole"::text AS base_role,
                    al.status AS al_status
             FROM "userAccessLevel" ual
             LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = u.id AND ual."companyId" = $1 AND ual.status = $3
             ORDER BY ual.id ASC
             LIMIT 1
           ) a ON true
           WHERE u.id = ANY($2) AND u.status = $3
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = $3)
           ORDER BY u.id"#,
    )
    .bind(company_id)
    .bind(user_ids)
    .bind(ACTIVE)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| OvertimeEmployeeRow {
            id: r.id,
            uuid: r.uuid,
            name: r.name,
            username: r.username,
            role: match (r.code, r.is_system, r.al_status) {
                (Some(code), Some(is_system), Some(status)) => Some(EmployeeRole {
                    code,
                    is_system,
                    base_role: r.base_role,
                    status,
                }),
                _ => None,
            },
        })
        .collect())
}

/// Versi satu karyawan (`users.findFirst`). `None` = tidak memenuhi `where`.
pub async fn load_overtime_employee<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Option<OvertimeEmployeeRow>> {
    Ok(load_overtime_employees(exec, company_id, &[user_id])
        .await?
        .into_iter()
        .next())
}

/// `overtimeEmployeeRank(row)`: peringkat role efektif (`rank_of`), atau `None` bila tak
/// dikenal / tanpa penugasan. Role custom nonaktif tak memberi plafon (sama dengan
/// RolesGuard); role custom dibandingkan lewat `baseRole`-nya.
pub fn overtime_employee_rank(row: Option<&OvertimeEmployeeRow>) -> Option<u8> {
    let role = row?.role.as_ref()?;
    if !(role.is_system || role.status == ACTIVE) {
        return None;
    }
    rank_of(&effective_role_code(
        &role.code,
        role.is_system,
        role.base_role,
    ))
}

/// `isEligibleOvertimeEmployee(row)`: role tak dikenal = tolak; GUEST (peringkat 0) tidak layak.
pub fn is_eligible_overtime_employee(row: Option<&OvertimeEmployeeRow>) -> bool {
    overtime_employee_rank(row).is_some_and(|r| rank_of("STAFF").is_some_and(|staff| r >= staff))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn row(
        code: &str,
        is_system: bool,
        base: Option<BaseRole>,
        status: i32,
    ) -> OvertimeEmployeeRow {
        OvertimeEmployeeRow {
            id: 1,
            uuid: String::new(),
            name: "Budi".into(),
            username: "b@x.com".into(),
            role: Some(EmployeeRole {
                code: code.into(),
                is_system,
                base_role: base,
                status,
            }),
        }
    }

    #[test]
    fn rank_and_eligibility() {
        assert!(!is_eligible_overtime_employee(None));
        let no_role = OvertimeEmployeeRow {
            role: None,
            ..row("STAFF", true, None, 1)
        };
        assert!(!is_eligible_overtime_employee(Some(&no_role)));
        assert!(is_eligible_overtime_employee(Some(&row(
            "STAFF", true, None, 1
        ))));
        assert!(!is_eligible_overtime_employee(Some(&row(
            "GUEST", true, None, 1
        ))));
        // Role bawaan nonaktif tetap dihitung (isSystem); custom nonaktif tidak.
        assert!(is_eligible_overtime_employee(Some(&row(
            "ADMIN", true, None, 0
        ))));
        assert!(!is_eligible_overtime_employee(Some(&row(
            "c1_x",
            false,
            Some(BaseRole::MANAGER),
            0
        ))));
        assert_eq!(
            overtime_employee_rank(Some(&row("c1_x", false, Some(BaseRole::MANAGER), 1))),
            Some(2)
        );
        // Custom tanpa baseRole → kode opaque → tak dikenal.
        assert!(!is_eligible_overtime_employee(Some(&row(
            "c1_x", false, None, 1
        ))));
    }
}
