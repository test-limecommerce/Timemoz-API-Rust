//! Query baca bersama (`divisions.service.ts` bagian "Read helpers").

use std::collections::{HashMap, HashSet};

use mz_core::AppState;
use mz_core::time::JsDate;
use mz_trackings::division::effective_code;
use sqlx::{PgExecutor, PgPool};

use crate::consts::ACTIVE;
use crate::division_time::company_day_and_week;
use crate::errors::{DivResult, module_disabled};

/// "Karyawan aktif company": `users.status 1` + `userCompany.status 1` (alias `u`, `$1` = company).
pub const ACTIVE_EMPLOYEE_SQL: &str = r#"u.status = 1 AND EXISTS (
    SELECT 1 FROM "userCompany" uc
    WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)"#;

/// `escapeLikePattern`: escape `\`, `%`, `_` (ILIKE memakai backslash).
pub fn escape_like_pattern(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `DivisionGateService.assertOpen`: hanya saklar pilot `company.divisionsEnabled`
/// (gerbang modul dan paket sudah ditegakkan `RouteSpec`). Tanpa cache 60 dtk (lihat deviasi).
pub async fn assert_open(db: &PgPool, company_id: i32) -> DivResult<()> {
    let enabled: Option<bool> =
        sqlx::query_scalar(r#"SELECT "divisionsEnabled" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    if enabled == Some(true) {
        Ok(())
    } else {
        Err(module_disabled())
    }
}

/// `requireCompanyId` + `assertOpen`.
pub async fn open_company(state: &AppState, user_id: i32) -> DivResult<i32> {
    let company = mz_core::company::require_company_id(&state.db, user_id).await?;
    assert_open(&state.db, company).await?;
    Ok(company)
}

/// `roleCodesInCompany`: role efektif per user (baris `userAccessLevel` aktif terbaru di company
/// itu; role custom nonaktif → `None`). User tanpa baris tidak ada di peta.
pub async fn role_codes_in_company<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, Option<String>>> {
    let mut out = HashMap::new();
    let mut ids: Vec<i32> = user_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(i32, String, bool, Option<String>, i32)> = sqlx::query_as(
        r#"SELECT ual."userId", al.code, al."isSystem", al."baseRole"::text, al.status
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = ANY($1) AND ual."companyId" = $2 AND ual.status = $3
           ORDER BY ual.id DESC"#,
    )
    .bind(&ids)
    .bind(company_id)
    .bind(ACTIVE)
    .fetch_all(exec)
    .await?;
    for (user_id, code, is_system, base, status) in rows {
        // Baris terbaru sudah diambil.
        out.entry(user_id).or_insert_with(|| {
            if !is_system && status != ACTIVE {
                None
            } else {
                Some(effective_code(&code, is_system, base.as_deref()))
            }
        });
    }
    Ok(out)
}

/// `Map.get(id) ?? null`.
pub fn code_of(codes: &HashMap<i32, Option<String>>, user: i32) -> Option<&str> {
    codes.get(&user).and_then(|c| c.as_deref())
}

/// Zona waktu + kunci tanggal hari ini/awal pekan company (`companyClock`).
#[derive(Debug, Clone)]
pub struct Clock {
    pub time_zone: String,
    pub today: String,
    pub week_start: String,
    pub data_history_months: Option<i32>,
}

pub async fn company_clock(db: &PgPool, company_id: i32) -> sqlx::Result<Clock> {
    let row: Option<(String, i32, i32)> = sqlx::query_as(
        r#"SELECT "timeZone", "workweekStartDay", "dataHistoryMonths" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    // `company?.timeZone || 'UTC'`, `workweekStartDay ?? 1`.
    let (zone, wsd, months) = match row {
        Some((z, w, m)) => (z, w, Some(m)),
        None => (String::new(), 1, None),
    };
    let time_zone = if zone.is_empty() {
        "UTC".to_owned()
    } else {
        zone
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (today, week_start) = company_day_and_week(now_ms, Some(&time_zone), wsd);
    Ok(Clock {
        time_zone,
        today,
        week_start,
        data_history_months: months,
    })
}

/// Anggota hidup dan aktif satu/lebih divisi.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LiveMember {
    pub division_id: i32,
    pub user_id: i32,
    pub role: String,
    pub created_at: JsDate,
    pub uuid: String,
    pub name: String,
    pub username: String,
}

impl LiveMember {
    pub fn is_leader(&self) -> bool {
        self.role == "LEADER"
    }
}

/// `liveMembersOf`: Leader dulu lalu nama; baris kembar user+divisi dibuang.
pub async fn live_members_of(
    db: &PgPool,
    company_id: i32,
    division_ids: &[i32],
) -> sqlx::Result<Vec<LiveMember>> {
    if division_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<LiveMember> = sqlx::query_as(&format!(
        r#"SELECT dm."divisionId" AS division_id, dm."userId" AS user_id, dm.role::text AS role,
                  dm."createdAt" AS created_at, u.uuid::text AS uuid, u.name AS name,
                  u.username AS username
           FROM "divisionMember" dm
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."divisionId" = ANY($2) AND dm."deletedAt" IS NULL
             AND {ACTIVE_EMPLOYEE_SQL}
           ORDER BY u.name ASC, dm."userId" ASC, dm.id ASC"#
    ))
    .bind(company_id)
    .bind(division_ids)
    .fetch_all(db)
    .await?;
    let mut seen: HashSet<(i32, i32)> = HashSet::new();
    let unique: Vec<LiveMember> = rows
        .into_iter()
        .filter(|r| seen.insert((r.division_id, r.user_id)))
        .collect();
    let (leaders, rest): (Vec<_>, Vec<_>) = unique.into_iter().partition(LiveMember::is_leader);
    Ok(leaders.into_iter().chain(rest).collect())
}

/// `ICU localeCompare` yang disederhanakan — implementasi bersama di `mz_core::jsfmt::collate_id`.
pub use mz_core::jsfmt::collate_id as locale_compare;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escape_covers_wildcards_and_backslash() {
        assert_eq!(escape_like_pattern("a_c"), r"a\_c");
        assert_eq!(escape_like_pattern("100%"), r"100\%");
        assert_eq!(escape_like_pattern(r"a\b"), r"a\\b");
        assert_eq!(escape_like_pattern("plain"), "plain");
    }

    #[test]
    fn locale_compare_ignores_case_first() {
        use std::cmp::Ordering::*;
        assert_eq!(locale_compare("andi", "Budi"), Less);
        assert_eq!(locale_compare("Budi", "andi"), Greater);
        assert_eq!(locale_compare("a", "A"), Less);
        assert_eq!(locale_compare("x", "x"), Equal);
    }
}
