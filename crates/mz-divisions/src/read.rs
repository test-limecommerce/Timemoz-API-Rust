//! Route baca `/v1/divisions*`: list, employees, my-scope, `:uuid`, `:uuid/tracking`,
//! `:uuid/report` (`divisions.service.ts`).

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::State;
use mz_core::time::JsDate;
use mz_core::uuid_rules::is_uuid;
use mz_core::{AppState, RequestCtx};
use mz_trackings::division::{leader_may_read_feature, role_code_in_company};
use sqlx::PgPool;

use crate::consts::*;
use crate::division_time::{
    Tracked, clamp_to_history, latest_activity_by_user, member_status, round_average,
    sum_tracked_seconds_by_user, utc_date_key,
};
use crate::dto::{
    EmployeesQueryRaw, ListQuery, ReportQuery, parse_employees_query, parse_list_query,
    parse_report_query,
};
use crate::entities::*;
use crate::errors::*;
use crate::extract::UuidParam;
use crate::queries::*;
use crate::scope::{Scope, can_lead_division, resolve_viewer_scope};

/// `viewerContext`: company → pilot switch (FR-030) → jangkauan (dibaca per request, tanpa cache).
async fn viewer_context(state: &AppState, user_id: i32) -> DivResult<(i32, Scope)> {
    let company = open_company(state, user_id).await?;
    let scope = resolve_viewer_scope(&state.db, user_id, company).await?;
    Ok((company, scope))
}

/// Divisi hidup yang boleh dibaca pemanggil (`readableDivision`).
pub struct ReadableDivision {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub description: Option<String>,
    pub version: i32,
}

type DivisionRow = (i32, String, String, Option<String>, i32);

/// Aturan akses baca SATU divisi (detail, tracking, report):
///   - COMPANY: uuid bukan UUID / tak ada / company lain / terhapus → 404;
///   - DIVISION: hanya divisi yang dipimpin; uuid lain → 403 DIVISION_SCOPE_FORBIDDEN TANPA cek
///     keberadaan (tidak pernah 404);
///   - SELF: 403 DIVISION_NOT_LEADER.
async fn readable_division(
    state: &AppState,
    user_id: i32,
    division_uuid: &str,
) -> DivResult<(i32, Scope, ReadableDivision)> {
    let (company, scope) = viewer_context(state, user_id).await?;
    let sql = r#"SELECT id, uuid::text, name, description, version FROM division
                 WHERE "companyId" = $1 AND "deletedAt" IS NULL AND "#;
    let row: Option<DivisionRow> = match &scope {
        Scope::Company => {
            if !is_uuid(division_uuid) {
                return Err(division_not_found());
            }
            sqlx::query_as(&format!("{sql}uuid = $2::uuid LIMIT 1"))
                .bind(company)
                .bind(division_uuid.to_lowercase())
                .fetch_optional(&state.db)
                .await?
        }
        Scope::Division { divisions, .. } => {
            let wanted = division_uuid.to_lowercase();
            let Some(led) = divisions.iter().find(|d| d.uuid.to_lowercase() == wanted) else {
                return Err(scope_forbidden("You can only open a division you lead"));
            };
            sqlx::query_as(&format!("{sql}id = $2 LIMIT 1"))
                .bind(company)
                .bind(led.id)
                .fetch_optional(&state.db)
                .await?
        }
        Scope::SelfOnly => return Err(not_leader()),
    };
    match row {
        Some((id, uuid, name, description, version)) => Ok((
            company,
            scope,
            ReadableDivision {
                id,
                uuid,
                name,
                description,
                version,
            },
        )),
        // T6: divisi Leader terhapus di antara resolver dan pembacaan baris → bukan 404.
        None if matches!(scope, Scope::Company) => Err(division_not_found()),
        None => Err(not_leader()),
    }
}

/// Leader yang ditampilkan: baris LEADER hidup yang role efektifnya boleh memimpin.
fn leaders_of(rows: &[LiveMember], codes: &HashMap<i32, Option<String>>) -> Vec<Person> {
    rows.iter()
        .filter(|r| r.is_leader() && can_lead_division(code_of(codes, r.user_id)))
        .map(|r| Person::new(&r.uuid, &r.name))
        .collect()
}

fn group_members(rows: Vec<LiveMember>) -> HashMap<i32, Vec<LiveMember>> {
    let mut grouped: HashMap<i32, Vec<LiveMember>> = HashMap::new();
    for r in rows {
        grouped.entry(r.division_id).or_default().push(r);
    }
    grouped
}

struct TimeTotals {
    today: HashMap<i32, i64>,
    week: HashMap<i32, i64>,
}

fn totals(m: HashMap<i32, Tracked>) -> HashMap<i32, i64> {
    m.into_iter().map(|(k, v)| (k, v.total)).collect()
}

/// `trackedTodayAndWeek`: satu query per periode (satu bila hari ini = awal pekan).
async fn tracked_today_and_week(
    db: &PgPool,
    company: i32,
    user_ids: &[i32],
    clock: &Clock,
) -> sqlx::Result<TimeTotals> {
    if clock.today == clock.week_start {
        let same = totals(
            sum_tracked_seconds_by_user(db, company, user_ids, &clock.today, &clock.today).await?,
        );
        return Ok(TimeTotals {
            today: same.clone(),
            week: same,
        });
    }
    let today =
        sum_tracked_seconds_by_user(db, company, user_ids, &clock.today, &clock.today).await?;
    let week =
        sum_tracked_seconds_by_user(db, company, user_ids, &clock.week_start, &clock.today).await?;
    Ok(TimeTotals {
        today: totals(today),
        week: totals(week),
    })
}

fn sum_for(rows: &[LiveMember], t: &TimeTotals) -> (i64, i64) {
    rows.iter().fold((0, 0), |(a, b), r| {
        (
            a + t.today.get(&r.user_id).copied().unwrap_or(0),
            b + t.week.get(&r.user_id).copied().unwrap_or(0),
        )
    })
}

/// `companySummary` (TIDAK dipengaruhi `search`) + role efektif setiap Leader hidup.
async fn company_summary(
    db: &PgPool,
    company: i32,
    known_division_count: Option<i64>,
) -> sqlx::Result<(Summary, HashMap<i32, Option<String>>)> {
    let divisions =
        match known_division_count {
            Some(n) => n,
            None => sqlx::query_scalar(
                r#"SELECT count(*) FROM division WHERE "companyId" = $1 AND "deletedAt" IS NULL"#,
            )
            .bind(company)
            .fetch_one(db)
            .await?,
        };
    let leader_rows: Vec<(i32, i32)> = sqlx::query_as(&format!(
        r#"SELECT dm."divisionId", dm."userId"
           FROM "divisionMember" dm
           JOIN division d ON d.id = dm."divisionId" AND d."deletedAt" IS NULL
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm.role::text = 'LEADER' AND dm."deletedAt" IS NULL
             AND {ACTIVE_EMPLOYEE_SQL}"#
    ))
    .bind(company)
    .fetch_all(db)
    .await?;
    let active: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM users u WHERE {ACTIVE_EMPLOYEE_SQL}"
    ))
    .bind(company)
    .fetch_one(db)
    .await?;
    let assigned: i64 = sqlx::query_scalar(&format!(
        r#"SELECT count(*) FROM users u WHERE {ACTIVE_EMPLOYEE_SQL}
             AND EXISTS (SELECT 1 FROM "divisionMember" dm
                         JOIN division d ON d.id = dm."divisionId" AND d."deletedAt" IS NULL
                         WHERE dm."userId" = u.id AND dm."companyId" = $1 AND dm."deletedAt" IS NULL)"#
    ))
    .bind(company)
    .fetch_one(db)
    .await?;

    let user_ids: Vec<i32> = leader_rows.iter().map(|(_, u)| *u).collect();
    let codes = role_codes_in_company(db, company, &user_ids).await?;
    let with_leader: HashSet<i32> = leader_rows
        .iter()
        .filter(|(_, u)| can_lead_division(code_of(&codes, *u)))
        .map(|(d, _)| *d)
        .collect();
    Ok((
        Summary {
            divisions,
            divisions_with_leader: with_leader.len(),
            assigned_employees: assigned,
            active_employees: active,
            unassigned_employees: (active - assigned).max(0),
        },
        codes,
    ))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ───────────────────────── service ─────────────────────────

/// Node: src/divisions/divisions.service.ts:271 `list`.
pub async fn list(state: &AppState, user_id: i32, query: &ListQuery) -> DivResult<ListResponse> {
    let (company, scope) = viewer_context(state, user_id).await?;
    let only_ids: Option<Vec<i32>> = match &scope {
        Scope::Company => None,
        Scope::Division { divisions, .. } => Some(divisions.iter().map(|d| d.id).collect()),
        Scope::SelfOnly => return Err(not_leader()),
    };
    // Validasi sudah dilakukan extractor; `search` kosong sesudah trim = tanpa filter.
    let search = parse_list_query(&query.0).ok().flatten();

    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT id, uuid::text, name, description, version FROM division WHERE "companyId" = "#,
    );
    qb.push_bind(company).push(r#" AND "deletedAt" IS NULL"#);
    if let Some(ids) = &only_ids {
        qb.push(" AND id = ANY(").push_bind(ids.clone()).push(")");
    }
    if let Some(s) = &search {
        qb.push(" AND name ILIKE ")
            .push_bind(format!("%{}%", escape_like_pattern(s)));
    }
    qb.push(" ORDER BY name ASC, id ASC");
    let divisions: Vec<DivisionRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let clock = company_clock(&state.db, company).await?;

    let division_ids: Vec<i32> = divisions.iter().map(|d| d.0).collect();
    let members = live_members_of(&state.db, company, &division_ids).await?;
    let company_part = if only_ids.is_none() {
        let known = if search.is_some() {
            None
        } else {
            i64::try_from(divisions.len()).ok()
        };
        Some(company_summary(&state.db, company, known).await?)
    } else {
        None
    };

    let all_users: Vec<i32> = {
        let mut v: Vec<i32> = members.iter().map(|m| m.user_id).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let time = tracked_today_and_week(&state.db, company, &all_users, &clock).await?;
    let (summary, leader_codes) = match company_part {
        Some((s, codes)) => (Some(s), codes),
        None => {
            let leader_ids: Vec<i32> = members
                .iter()
                .filter(|m| m.is_leader())
                .map(|m| m.user_id)
                .collect();
            (
                None,
                role_codes_in_company(&state.db, company, &leader_ids).await?,
            )
        }
    };

    let mut by_division = group_members(members);
    let data = divisions
        .into_iter()
        .map(|(id, uuid, name, description, version)| {
            let rows = by_division.remove(&id).unwrap_or_default();
            let (today, week) = sum_for(&rows, &time);
            let leaders = leaders_of(&rows, &leader_codes);
            ListItem {
                uuid,
                name,
                description,
                version,
                // D3: `leader` (deprecated) = Leader pertama.
                leader: leaders.first().cloned(),
                leaders,
                member_count: rows.len(),
                members_preview: rows
                    .iter()
                    .take(MEMBERS_PREVIEW_MAX)
                    .map(|r| Person::new(&r.uuid, &r.name))
                    .collect(),
                tracked_today_seconds: today,
                tracked_week_seconds: week,
            }
        })
        .collect();

    Ok(ListResponse {
        data,
        summary,
        meta: ListMeta {
            time_zone: clock.time_zone,
            today: clock.today,
            week_start: clock.week_start,
        },
    })
}

/// Node: src/divisions/divisions.service.ts:352 `getDetail`.
pub async fn get_detail(
    state: &AppState,
    user_id: i32,
    division_uuid: &str,
) -> DivResult<DetailResponse> {
    let (company, scope, division) = readable_division(state, user_id, division_uuid).await?;
    let rows = live_members_of(&state.db, company, &[division.id]).await?;
    let clock = company_clock(&state.db, company).await?;
    // Hanya COMPANY yang bisa memegang role WRITE_ROLES (semuanya company-wide).
    let viewer_role = if matches!(scope, Scope::Company) {
        role_code_in_company(&state.db, user_id, company).await?
    } else {
        None
    };
    let user_ids: Vec<i32> = rows.iter().map(|r| r.user_id).collect();
    let leader_ids: Vec<i32> = rows
        .iter()
        .filter(|r| r.is_leader())
        .map(|r| r.user_id)
        .collect();
    let time = tracked_today_and_week(&state.db, company, &user_ids, &clock).await?;
    let codes = role_codes_in_company(&state.db, company, &leader_ids).await?;
    let (today, week) = sum_for(&rows, &time);
    let leaders = leaders_of(&rows, &codes);

    Ok(DetailResponse {
        data: DetailEntity {
            uuid: division.uuid,
            name: division.name,
            description: division.description,
            version: division.version,
            leader: leaders.first().cloned(),
            leaders,
            members: rows
                .iter()
                .map(|r| MemberEntity {
                    uuid: r.uuid.clone(),
                    name: r.name.clone(),
                    email: r.username.clone(),
                    role: r.role.clone(),
                    joined_at: r.created_at,
                })
                .collect(),
            tracked_today_seconds: today,
            tracked_week_seconds: week,
            viewer: Viewer {
                can_manage: viewer_role.as_deref().is_some_and(is_write_role),
            },
        },
    })
}

/// Node: src/divisions/divisions.service.ts:472 `myScope`.
pub async fn my_scope(state: &AppState, user_id: i32) -> DivResult<MyScopeResponse> {
    let (company, scope) = viewer_context(state, user_id).await?;
    let Scope::Division {
        divisions,
        user_ids,
    } = scope
    else {
        return Ok(MyScopeResponse {
            data: MyScopeEntity {
                kind: scope.kind(),
                division: None,
                divisions: None,
                employees: None,
            },
        });
    };
    // `employees` = PERSIS `scope.userIds` (dibaca dari `users`, bukan dari baris anggota).
    let users: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT id, uuid::text, name FROM users WHERE id = ANY($1) ORDER BY name ASC, id ASC"#,
    )
    .bind(&user_ids)
    .fetch_all(&state.db)
    .await?;
    let division_ids: Vec<i32> = divisions.iter().map(|d| d.id).collect();
    let leader_rows: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "divisionMember"
           WHERE "companyId" = $1 AND "divisionId" = ANY($2) AND role::text = 'LEADER'
             AND "deletedAt" IS NULL AND "userId" = ANY($3)"#,
    )
    .bind(company)
    .bind(&division_ids)
    .bind(&user_ids)
    .fetch_all(&state.db)
    .await?;
    // Pemanggil selalu Leader (resolver menemukan baris LEADER hidupnya).
    let mut leader_ids: HashSet<i32> = leader_rows.into_iter().collect();
    leader_ids.insert(user_id);
    let employees: Vec<ScopeEmployee> = users
        .into_iter()
        .map(|(id, uuid, name)| ScopeEmployee {
            id,
            uuid,
            name,
            role: if leader_ids.contains(&id) {
                "LEADER"
            } else {
                "MEMBER"
            }
            .to_owned(),
        })
        .collect();
    let divs: Vec<ScopeDivisionEntity> = divisions
        .iter()
        .map(|d| ScopeDivisionEntity {
            uuid: d.uuid.clone(),
            name: d.name.clone(),
        })
        .collect();
    // Urutan: pemanggil, Leader lain, Member (masing-masing urut nama).
    let mut ordered = Vec::with_capacity(employees.len());
    let (me, others): (Vec<_>, Vec<_>) = employees.into_iter().partition(|e| e.id == user_id);
    let (leaders, members): (Vec<_>, Vec<_>) = others.into_iter().partition(|e| e.role == "LEADER");
    ordered.extend(me);
    ordered.extend(leaders);
    ordered.extend(members);
    Ok(MyScopeResponse {
        data: MyScopeEntity {
            kind: "DIVISION",
            division: divs.first().cloned(),
            divisions: Some(divs),
            employees: Some(ordered),
        },
    })
}

/// Node: src/divisions/divisions.service.ts:560 `tracking`.
pub async fn tracking(
    state: &AppState,
    user_id: i32,
    division_uuid: &str,
) -> DivResult<TrackingResponse> {
    let (company, _scope, division) = readable_division(state, user_id, division_uuid).await?;
    // FR-024: tab ini menuntut `screenshots:READ` untuk SETIAP pemanggil.
    let access = mz_core::permissions::effective_access(state, user_id, company).await?;
    if !leader_may_read_feature(access.as_deref(), "screenshots") {
        return Err(scope_forbidden(
            "Your role cannot open the Screenshots feature",
        ));
    }
    let rows = live_members_of(&state.db, company, &[division.id]).await?;
    let clock = company_clock(&state.db, company).await?;
    let user_ids: Vec<i32> = rows.iter().map(|r| r.user_id).collect();
    let latest = latest_activity_by_user(&state.db, company, &user_ids, &clock.today).await?;
    let today =
        sum_tracked_seconds_by_user(&state.db, company, &user_ids, &clock.today, &clock.today)
            .await?;

    // Satu `now` untuk semua anggota supaya ambang status konsisten di satu respons.
    let now = now_ms();
    struct Live<'a> {
        row: &'a LiveMember,
        status: crate::division_time::MemberStatus,
        last_end: Option<JsDate>,
        project_id: Option<i32>,
    }
    let lives: Vec<Live<'_>> = rows
        .iter()
        .map(|row| {
            let activity = latest.get(&row.user_id);
            let last_end = activity.and_then(|a| a.last_end());
            let status = member_status(last_end.map(|d| d.timestamp_millis()), now);
            Live {
                row,
                status,
                last_end,
                // `currentProject` hanya untuk ONLINE/IDLE.
                project_id: if status == crate::division_time::MemberStatus::Offline {
                    None
                } else {
                    activity.and_then(|a| a.project_id)
                },
            }
        })
        .collect();
    let mut project_ids: Vec<i32> = lives.iter().filter_map(|l| l.project_id).collect();
    project_ids.sort_unstable();
    project_ids.dedup();
    let projects: HashMap<i32, (String, String)> = if project_ids.is_empty() {
        HashMap::new()
    } else {
        // Pagar tenant: proyek harus milik company ini.
        let rows: Vec<(i32, String, String)> = sqlx::query_as(
            r#"SELECT p.id, p.uuid::text, p.name FROM project p
               WHERE p.id = ANY($1)
                 AND EXISTS (SELECT 1 FROM "projectCompany" pc
                             WHERE pc."projectId" = p.id AND pc."companyId" = $2)"#,
        )
        .bind(&project_ids)
        .bind(company)
        .fetch_all(&state.db)
        .await?;
        rows.into_iter().map(|(id, u, n)| (id, (u, n))).collect()
    };

    let data = lives
        .iter()
        .map(|l| TrackingItem {
            uuid: l.row.uuid.clone(),
            name: l.row.name.clone(),
            role: l.row.role.clone(),
            status: l.status.as_str(),
            current_project: l
                .project_id
                .and_then(|id| projects.get(&id))
                .map(|(uuid, name)| TrackingProject {
                    uuid: uuid.clone(),
                    name: name.clone(),
                }),
            last_activity_at: l.last_end,
            tracked_today_seconds: today.get(&l.row.user_id).map_or(0, |t| t.total),
        })
        .collect();
    Ok(TrackingResponse {
        data,
        meta: TrackingMeta {
            time_zone: clock.time_zone,
            today: clock.today,
            online_window_minutes: ONLINE_WINDOW_MINUTES,
            idle_window_minutes: IDLE_WINDOW_MINUTES,
        },
    })
}

/// Node: src/divisions/divisions.service.ts:663 `report`.
pub async fn report(
    state: &AppState,
    user_id: i32,
    division_uuid: &str,
    query: &ReportQuery,
) -> DivResult<ReportResponse> {
    let (company, _scope, division) = readable_division(state, user_id, division_uuid).await?;
    let access = mz_core::permissions::effective_access(state, user_id, company).await?;
    if !leader_may_read_feature(access.as_deref(), "reports") {
        return Err(scope_forbidden("Your role cannot open the Reports feature"));
    }
    // Pertahanan berlapis: DTO sudah menolak rentang tidak sah.
    if let Some(msg) = crate::dto::report_range_error(query.0.get("startAt"), query.0.get("endAt"))
    {
        return Err(coded(
            axum::http::StatusCode::BAD_REQUEST,
            DIVISION_REPORT_RANGE_INVALID,
            msg,
        ));
    }
    let range = parse_report_query(&query.0).ok().flatten();

    let rows = live_members_of(&state.db, company, &[division.id]).await?;
    let clock = company_clock(&state.db, company).await?;

    // Batas riwayat yang sama dengan `ReportsService.find` (-1 = tanpa batas).
    let week_start_midnight = chrono::NaiveDate::parse_from_str(&clock.week_start, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or_default();
    let requested_start = range.map_or(week_start_midnight, |(s, _)| s);
    let (start_date, clamped) =
        clamp_to_history(requested_start, clock.data_history_months, now_ms());
    let from_key = utc_date_key(start_date);
    let to_key = range.map_or_else(|| clock.today.clone(), |(_, e)| utc_date_key(e));

    // FR-028: riwayat penuh (tidak difilter `createdAt` anggota). Seluruh rentang lebih tua dari
    // batas riwayat: tidak ada yang boleh dibaca dan rentang dipersempit ke satu hari.
    let entirely_before_limit = to_key < from_key;
    let user_ids: Vec<i32> = rows.iter().map(|r| r.user_id).collect();
    let sums = if entirely_before_limit {
        HashMap::new()
    } else {
        sum_tracked_seconds_by_user(&state.db, company, &user_ids, &from_key, &to_key).await?
    };

    let members: Vec<ReportMember> = rows
        .iter()
        .map(|r| {
            let s = sums.get(&r.user_id).copied().unwrap_or_default();
            ReportMember {
                uuid: r.uuid.clone(),
                name: r.name.clone(),
                total_seconds: s.total,
                billable_seconds: s.billable,
                non_billable_seconds: s.non_billable,
            }
        })
        .collect();
    let total: i64 = members.iter().map(|m| m.total_seconds).sum();
    let billable: i64 = members.iter().map(|m| m.billable_seconds).sum();
    let non_billable: i64 = members.iter().map(|m| m.non_billable_seconds).sum();
    let n = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    Ok(ReportResponse {
        data: ReportEntity {
            range: ReportRangeEntity {
                start_at: from_key.clone(),
                end_at: if entirely_before_limit {
                    from_key
                } else {
                    to_key
                },
                clamped_by_history_limit: clamped,
            },
            totals: ReportTotals {
                total_seconds: total,
                billable_seconds: billable,
                non_billable_seconds: non_billable,
                average_per_member_seconds: round_average(total, n),
            },
            members,
        },
    })
}

/// Node: src/divisions/divisions.service.ts:766 `listEmployees`.
pub async fn list_employees(
    state: &AppState,
    user_id: i32,
    query: &EmployeesQueryRaw,
) -> DivResult<EmployeesResponse> {
    let (company, scope) = viewer_context(state, user_id).await?;
    // Route-nya VIEW_ROLES; pagar kedua: daftar memuat email semua karyawan.
    if !matches!(scope, Scope::Company) {
        return Err(not_leader());
    }
    let q = parse_employees_query(&query.0).map_err(|_| internal("invalid query"))?;
    // Prisma `skip`/`take`: nilai di luar rentang bigint gagal (500).
    let offset = q
        .page_number
        .saturating_sub(1)
        .checked_mul(q.items_per_page)
        .ok_or_else(|| internal("skip out of range"))?;

    let like = q
        .search
        .as_ref()
        .map(|s| format!("%{}%", escape_like_pattern(s)));
    // SQL tetap (offset/limit/pencarian di-bind) supaya cache prepared statement tidak membengkak.
    let users: Vec<(i32, String, String, String)> = sqlx::query_as(&format!(
        r#"SELECT u.id, u.uuid::text, u.name, u.username FROM users u
           WHERE {ACTIVE_EMPLOYEE_SQL} AND ($2::text IS NULL OR u.name ILIKE $2)
           ORDER BY u.name ASC, u.id ASC
           OFFSET $3 LIMIT $4"#
    ))
    .bind(company)
    .bind(&like)
    .bind(offset)
    .bind(q.items_per_page)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM users u WHERE {ACTIVE_EMPLOYEE_SQL} AND ($2::text IS NULL OR u.name ILIKE $2)"
    ))
    .bind(company)
    .bind(&like)
    .fetch_one(&state.db)
    .await?;

    let ids: Vec<i32> = users.iter().map(|u| u.0).collect();
    // A1: satu baris hidup per divisi, banyak divisi per orang; baris kembar disaring.
    let memberships: Vec<(i32, String, String, String)> = sqlx::query_as(
        r#"SELECT dm."userId", dm.role::text, d.uuid::text, d.name
           FROM "divisionMember" dm
           JOIN division d ON d.id = dm."divisionId" AND d."deletedAt" IS NULL
           WHERE dm."companyId" = $1 AND dm."userId" = ANY($2) AND dm."deletedAt" IS NULL
           ORDER BY d.name ASC, dm."divisionId" ASC, dm.id DESC"#,
    )
    .bind(company)
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let mut per_user: HashMap<i32, Vec<EmployeeDivision>> = HashMap::new();
    let mut seen: HashSet<(i32, String)> = HashSet::new();
    for (uid, role, uuid, name) in memberships {
        if !seen.insert((uid, uuid.clone())) {
            continue;
        }
        per_user
            .entry(uid)
            .or_default()
            .push(EmployeeDivision { uuid, name, role });
    }

    let data = users
        .into_iter()
        .map(|(id, uuid, name, email)| {
            let divisions = per_user.remove(&id).unwrap_or_default();
            Employee {
                uuid,
                name,
                email,
                division: divisions.first().map(|d| EmployeeDivision {
                    uuid: d.uuid.clone(),
                    name: d.name.clone(),
                    role: d.role.clone(),
                }),
                divisions,
            }
        })
        .collect();
    Ok(EmployeesResponse {
        data,
        meta: EmployeesMeta {
            total,
            page_number: q.page_number,
            items_per_page: q.items_per_page,
        },
    })
}

// ───────────────────────── handler ─────────────────────────

use mz_core::extract::ValidatedQuery;

/// Node: src/divisions/divisions.controller.ts:62 `list`
pub async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<ListQuery>,
) -> DivResult<Json<ListResponse>> {
    Ok(Json(list(&state, ctx.require_user_id()?, &q).await?))
}

/// Node: src/divisions/divisions.controller.ts:73 `listEmployees`
pub async fn h_employees(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<EmployeesQueryRaw>,
) -> DivResult<Json<EmployeesResponse>> {
    Ok(Json(
        list_employees(&state, ctx.require_user_id()?, &q).await?,
    ))
}

/// Node: src/divisions/divisions.controller.ts:83 `myScope`
pub async fn h_my_scope(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> DivResult<Json<MyScopeResponse>> {
    Ok(Json(my_scope(&state, ctx.require_user_id()?).await?))
}

/// Node: src/divisions/divisions.controller.ts:91 `getDetail`
pub async fn h_detail(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> DivResult<Json<DetailResponse>> {
    Ok(Json(
        get_detail(&state, ctx.require_user_id()?, &uuid).await?,
    ))
}

/// Node: src/divisions/divisions.controller.ts:100 `tracking`
pub async fn h_tracking(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> DivResult<Json<TrackingResponse>> {
    Ok(Json(tracking(&state, ctx.require_user_id()?, &uuid).await?))
}

/// Node: src/divisions/divisions.controller.ts:109 `report`
pub async fn h_report(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
    ValidatedQuery(q): ValidatedQuery<ReportQuery>,
) -> DivResult<Json<ReportResponse>> {
    Ok(Json(
        report(&state, ctx.require_user_id()?, &uuid, &q).await?,
    ))
}
