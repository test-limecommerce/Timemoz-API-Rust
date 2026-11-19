//! Port `src/projects/projects.service.ts`.

use std::collections::HashMap;
use std::sync::OnceLock;

use fred::prelude::*;
use mz_core::company::find_company_id;
use mz_core::db::expect_one;
use mz_core::mail::{OwnedMail, spawn_send};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use tokio::sync::Semaphore;

use crate::dto::{PaginatorQuery, ProjectInput};
use crate::jsfmt::{as_i32, num};
use crate::model::{self, STATUS_ACTIVE, STATUS_DELETED, Scope};
use crate::{approvers, budget, rowjson};

/// Satu-satunya tipe project yang memakai `total_used` di klien desktop.
const FIXED_HOURS_TYPE_CODE: &str = "FIXED_HOURS";
/// Desktop meminta detail project tiap 60 dtk; total boleh basi selama itu.
const DEVICE_TOTAL_USED_TTL_MS: i64 = 60_000;

// Objek yang dibangun di SQL memakai `json_build_object`/`json_agg` (tipe `json` menjaga urutan
// argumen = urutan `select` Prisma), BUKAN jsonb (jsonb mengurutkan kunci menurut panjang lalu
// abjad). Baris tabel utuh (`to_jsonb(row)`) disusun ulang di Rust lewat `rowjson::order`.
const REL_CLIENT: &str = r#"COALESCE((SELECT json_agg(json_build_object('client',
    json_build_object('uuid', c.uuid::text, 'code', c.code, 'name', c.name)) ORDER BY x.id)
    FROM "projectClient" x JOIN client c ON c.id = x."clientId" WHERE x."projectId" = p.id), '[]'::json)"#;
const REL_USER: &str = r#"COALESCE((SELECT json_agg(json_build_object('user',
    json_build_object('uuid', u.uuid::text, 'username', u.username, 'name', u.name)) ORDER BY x.id)
    FROM "projectUser" x JOIN users u ON u.id = x."userId" WHERE x."projectId" = p.id), '[]'::json)"#;
const REL_COMPANY: &str = r#"COALESCE((SELECT json_agg(json_build_object('company',
    json_build_object('uuid', c.uuid::text, 'name', c.name)) ORDER BY x.id)
    FROM "projectCompany" x JOIN company c ON c.id = x."companyId" WHERE x."projectId" = p.id), '[]'::json)"#;
/// Baris `projectType` utuh; kunci disusun ulang di Rust (`PROJECT_TYPE_KEYS`).
const REL_TYPE: &str =
    r#"(SELECT to_jsonb(t) FROM "projectType" t WHERE t.id = p."projectTypeId")"#;

fn internal(msg: &'static str) -> ApiError {
    ApiError::internal(anyhow::anyhow!(msg))
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

fn not_found() -> Value {
    json!({ "success": false, "message": "Data project not found", "data": null })
}

// ───────────────────────── GET /projects ─────────────────────────

/// Batas jumlah `GET /projects` yang memegang koneksi DB bersamaan, supaya
/// lonjakan daftar project tidak menghabiskan pool dan membuat endpoint lain
/// (mis. `POST /trackings`) ikut mengantre. `RS_PROJECTS_LIST_CONCURRENCY`,
/// bawaan sepertiga `RS_DB_MAX_CONNECTIONS` (minimal 1). Dibaca sekali per proses.
static LIST_PERMITS: OnceLock<Semaphore> = OnceLock::new();

fn list_permits(state: &AppState) -> &'static Semaphore {
    LIST_PERMITS.get_or_init(|| {
        let n = state
            .config
            .var("RS_PROJECTS_LIST_CONCURRENCY")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                usize::try_from(state.config.db_max_connections / 3)
                    .unwrap_or(1)
                    .max(1)
            });
        Semaphore::new(n)
    })
}

/// `RS_PROJECTS_DEVICE_LITE` — bawaan aktif; `0`/`false`/`off` mematikan
/// respons ringan untuk desktop (kembali ke respons lengkap).
fn device_lite_enabled(state: &AppState) -> bool {
    !state
        .config
        .var("RS_PROJECTS_DEVICE_LITE")
        .is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            )
        })
}

/// `findAll(query, userId)`.
///
/// `company_id` = `resolveRequestCompanyId` (sudah dihitung guard, di-memo per
/// request). `device` = token desktop tanpa `Origin` (lihat `find_one`): klien
/// desktop hanya membaca `id/uuid/code/name/projectType`, jadi relasi anggota,
/// klien, company, `budgetSummary` dan `lastTracked` tidak dihitung (key tetap
/// ada: `[]`/`null`). Deviasi dari Node, lihat `plan/deviations/mz-projects.md`.
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    company_id: Option<i32>,
    q: &PaginatorQuery,
    device: bool,
) -> ApiResult<Value> {
    let lazy = model::resolve_paging_lazy(q).map_err(internal)?;
    let lite = device && device_lite_enabled(state);
    let search = q.search.as_deref();

    let permit = list_permits(state)
        .acquire()
        .await
        .map_err(|_| internal("project list semaphore closed"))?;

    let elevated = model::is_elevated(&state.db, user_id).await?;
    let scope = Scope::resolve(elevated, company_id, user_id);

    let offset = lazy.per_page.map_or(0, |n| (lazy.page - 1) * n);
    let (total, rows) = if lazy.per_page.is_none() && lazy.page > 1 {
        // itemsPerPage = total dan halaman > 1: OFFSET >= total, selalu kosong.
        (count_projects(state, scope, search).await?, Vec::new())
    } else {
        let rows = fetch_page(state, scope, search, &lazy, offset, lite).await?;
        let total = match rows.first() {
            Some((t, _)) => *t,
            None if offset == 0 => 0,
            None => count_projects(state, scope, search).await?,
        };
        (total, rows)
    };
    let paging = lazy.with_total(total);
    let mut docs: Vec<Value> = rows.into_iter().map(|r| r.1).collect();

    let ids: Vec<i32> = docs
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_i64))
        .filter_map(|i| i32::try_from(i).ok())
        .collect();

    let (last_tracked, budgets) = if lite || ids.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        let budget_sql = format!(
            "{} WHERE b.\"projectId\" = ANY($1) AND b.\"isEnabled\" = true",
            budget::BUDGET_SELECT
        );
        let (tracked, budgets) = tokio::try_join!(
            sqlx::query_as::<_, (i32, Option<JsDate>)>(
                r#"SELECT "projectId", MAX("endAt") AS "lastTracked" FROM "trackingTimeline"
                   WHERE "projectId" = ANY($1) AND "status" = $2 GROUP BY "projectId""#,
            )
            .bind(&ids)
            .bind(STATUS_ACTIVE)
            .fetch_all(&state.db),
            sqlx::query_as::<_, budget::BudgetRow>(&budget_sql)
                .bind(&ids)
                .fetch_all(&state.db),
        )?;
        (
            tracked
                .into_iter()
                .collect::<HashMap<i32, Option<JsDate>>>(),
            budgets
                .into_iter()
                .map(|b| (b.project_id, b))
                .collect::<HashMap<i32, budget::BudgetRow>>(),
        )
    };
    drop(permit);

    for doc in &mut docs {
        let id = doc
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|i| i32::try_from(i).ok());
        rowjson::fix(doc);
        if let Some(t) = doc.get_mut("projectType") {
            rowjson::order(t, rowjson::PROJECT_TYPE_KEYS);
        }
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut project_type = doc.get("projectType").cloned().unwrap_or(Value::Null);
        if !project_type.is_object() {
            project_type = json!({});
        }
        rowjson::merge(&mut project_type, vec![("name", json!(""))]);
        let summary = id
            .and_then(|i| budgets.get(&i))
            .map_or(Value::Null, budget::list_summary);
        let tracked = id
            .and_then(|i| last_tracked.get(&i).copied().flatten())
            .map_or(Value::Null, |d| Value::String(d.to_js_string()));
        rowjson::merge(
            doc,
            vec![
                ("name", Value::String(truncate_name(&name))),
                ("projectType", project_type),
                ("budgetSummary", summary),
                ("lastTracked", tracked),
            ],
        );
    }

    Ok(json!({
        "success": true,
        "message": "Success get all data projects",
        "data": {
            "total": total,
            "itemsPerPage": paging.per_page,
            "pageNumber": paging.page,
            "result": docs,
        }
    }))
}

async fn count_projects(state: &AppState, scope: Scope, search: Option<&str>) -> sqlx::Result<i64> {
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM project p WHERE ");
    model::push_filter(&mut count, scope, search);
    count.build_query_scalar().fetch_one(&state.db).await
}

/// Satu halaman daftar + `total` (`COUNT(*) OVER ()`) dalam satu kueri. Halaman
/// dipilih dulu dari kolom ringan; subkueri relasi hanya dijalankan untuk
/// baris di halaman itu. Urutan sama dengan `ORDER BY <kolom>, p.id ASC`.
async fn fetch_page(
    state: &AppState,
    scope: Scope,
    search: Option<&str>,
    paging: &model::LazyPaging,
    offset: i64,
    lite: bool,
) -> sqlx::Result<Vec<(i64, Value)>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "WITH page AS (SELECT p.id, COUNT(*) OVER () AS total, ROW_NUMBER() OVER (ORDER BY ",
    );
    qb.push(paging.column)
        .push(if paging.desc { " DESC" } else { " ASC" })
        .push(", p.id ASC) AS rn FROM project p WHERE ");
    model::push_filter(&mut qb, scope, search);
    // `LIMIT NULL` = tanpa batas (itemsPerPage = total).
    qb.push(" ORDER BY rn LIMIT ")
        .push_bind(paging.per_page)
        .push(" OFFSET ")
        .push_bind(offset);
    let relations = if lite {
        format!(
            r#"'projectType', {REL_TYPE}, 'projectClient', '[]'::json,
             'projectUser', '[]'::json, 'projectCompany', '[]'::json"#
        )
    } else {
        format!(
            r#"'projectType', {REL_TYPE}, 'projectClient', {REL_CLIENT},
             'projectUser', {REL_USER}, 'projectCompany', {REL_COMPANY}"#
        )
    };
    qb.push(format!(
        r#") SELECT pg.total, json_build_object(
             'id', p.id, 'uuid', p.uuid::text, 'name', p.name, 'code', p.code, 'createdAt', p."createdAt",
             {relations}) AS doc
           FROM page pg JOIN project p ON p.id = pg.id
           ORDER BY pg.rn"#
    ));
    qb.build_query_as().fetch_all(&state.db).await
}

/// `name.length > 22 ? name.substring(0, 22) + '...' : name` (unit UTF-16).
pub fn truncate_name(name: &str) -> String {
    const MAX: usize = 22;
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() > MAX {
        let head = units.get(..MAX).unwrap_or(&units);
        format!("{}...", String::from_utf16_lossy(head))
    } else {
        name.to_owned()
    }
}

// ───────────────────────── GET /projects/:uuid ─────────────────────────

/// `findOne(uuid, userId, { devicePoll })` (Node: `src/projects/projects.service.ts:376`).
///
/// Perbaikan 2026-11-17 (deviasi 6): role elevated hanya melihat project yang terhubung
/// (`projectCompany.status = 1`) dengan company aktifnya (`find_company_id` =
/// `nestedUserCompanyScope`, Node `:393-405`); project company lain dijawab `not_found()`.
/// Elevated tanpa company aktif, dan non-elevated, memakai keanggotaan `projectUser`.
/// SYSTEM_ADMIN tidak dikecualikan (hanya "company-wide").
pub async fn find_one(
    state: &AppState,
    uuid: &str,
    user_id: i32,
    device_poll: bool,
) -> ApiResult<Value> {
    let elevated = model::is_elevated(&state.db, user_id).await?;
    let company_id = find_company_id(&state.db, user_id).await?;
    let company_filter = if elevated { company_id } else { None };

    let sql = if device_poll {
        format!(
            r#"SELECT p.id, to_jsonb(p) AS doc, json_build_object('projectType', {REL_TYPE},
                 'hours', COALESCE((SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM "projectHour" h
                                    WHERE h."projectId" = p.id), '[]'::jsonb)) AS rel
               FROM project p
               WHERE p.uuid = $1::uuid
                 AND CASE WHEN $3::int IS NOT NULL
                          THEN EXISTS (SELECT 1 FROM "projectCompany" pc
                                        WHERE pc."projectId" = p.id AND pc."companyId" = $3 AND pc.status = 1)
                          ELSE EXISTS (SELECT 1 FROM "projectUser" pu
                                        WHERE pu."projectId" = p.id AND pu."userId" = $2)
                     END
               LIMIT 1"#
        )
    } else {
        format!(
            r#"SELECT p.id, to_jsonb(p) AS doc, json_build_object('projectType', {REL_TYPE},
                 'hours', COALESCE((SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM "projectHour" h
                                    WHERE h."projectId" = p.id), '[]'::jsonb),
                 'projectClient', {REL_CLIENT}, 'projectUser', {REL_USER}, 'projectCompany', {REL_COMPANY},
                 'manualTimeApprovers', COALESCE((SELECT json_agg(json_build_object(
                     'uuid', a.uuid::text, 'createdAt', a."createdAt",
                     'user', json_build_object('uuid', u.uuid::text, 'name', u.name, 'username', u.username))
                     ORDER BY a.id)
                   FROM "projectManualTimeApprover" a JOIN users u ON u.id = a."userId"
                   WHERE a."projectId" = p.id AND a.status = 1), '[]'::json)) AS rel
               FROM project p
               WHERE p.uuid = $1::uuid
                 AND CASE WHEN $3::int IS NOT NULL
                          THEN EXISTS (SELECT 1 FROM "projectCompany" pc
                                        WHERE pc."projectId" = p.id AND pc."companyId" = $3 AND pc.status = 1)
                          ELSE EXISTS (SELECT 1 FROM "projectUser" pu
                                        WHERE pu."projectId" = p.id AND pu."userId" = $2)
                     END
               LIMIT 1"#
        )
    };
    let row: Option<(i32, Value, Value)> = sqlx::query_as(&sql)
        .bind(uuid)
        .bind(user_id)
        .bind(company_filter)
        .fetch_optional(&state.db)
        .await?;
    let Some((project_id, mut doc, rel)) = row else {
        return Ok(not_found());
    };
    // Node `findFirst({ include })`: kolom skalar (urutan field model), lalu relasi menurut urutan
    // `include`. `rel` berasal dari `json_build_object` sehingga urutannya sudah urutan `include`.
    rowjson::order(&mut doc, rowjson::PROJECT_KEYS);
    if let Value::Object(m) = rel {
        for (k, v) in m {
            rowjson::merge(&mut doc, vec![(k.as_str(), v)]);
        }
    }
    if let Some(t) = doc.get_mut("projectType") {
        rowjson::order(t, rowjson::PROJECT_TYPE_KEYS);
    }
    if let Some(h) = doc.get_mut("hours") {
        rowjson::order_each(h, rowjson::PROJECT_HOUR_KEYS);
    }

    if device_poll {
        let is_fixed = doc
            .get("projectType")
            .and_then(|t| t.get("code"))
            .and_then(Value::as_str)
            == Some(FIXED_HOURS_TYPE_CODE);
        let total = if is_fixed {
            cached_total_used_seconds(state, project_id).await?
        } else {
            0.0
        };
        rowjson::merge(
            &mut doc,
            vec![
                ("projectClient", json!([])),
                ("projectUser", json!([])),
                ("projectCompany", json!([])),
                ("manualTimeApprovers", json!([])),
                ("total_used", num(total)),
                ("budget", Value::Null),
            ],
        );
    } else {
        let total = total_used_seconds(state, project_id).await?;
        let budget_summary = budget::find_one(state, uuid).await?;
        rowjson::merge(
            &mut doc,
            vec![
                ("total_used", num(total)),
                (
                    "budget",
                    budget_summary.get("data").cloned().unwrap_or(Value::Null),
                ),
            ],
        );
    }
    rowjson::fix(&mut doc);
    Ok(json!({ "success": true, "message": "Success get data project by uuid", "data": doc }))
}

async fn total_used_seconds(state: &AppState, project_id: i32) -> sqlx::Result<f64> {
    sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(EXTRACT(EPOCH FROM ("endAt" - "startAt"))), 0)::float8
           FROM "trackingTimeline" WHERE "projectId" = $1 AND "status" = $2"#,
    )
    .bind(project_id)
    .bind(STATUS_ACTIVE)
    .fetch_one(&state.db)
    .await
}

/// `cachedTotalUsedSeconds`: kunci Redis `project-total-used:{id}` (nilai JSON
/// number, TTL 60 dtk). Redis gagal → hitung langsung.
async fn cached_total_used_seconds(state: &AppState, project_id: i32) -> sqlx::Result<f64> {
    let key = format!("project-total-used:{project_id}");
    if let Ok(Some(hit)) = state.redis.next().get::<Option<String>, _>(&key).await
        && let Ok(n) = hit.parse::<f64>()
    {
        return Ok(n);
    }
    let total = total_used_seconds(state, project_id).await?;
    let _ = state
        .redis
        .next()
        .set::<(), _, _>(
            &key,
            num(total).to_string(),
            Some(Expiration::PX(DEVICE_TOTAL_USED_TTL_MS)),
            None,
            false,
        )
        .await;
    Ok(total)
}

// Guard `@UUID()` (dulu `ensure_project_access`) = `mz_core::uuid_guard::ensure_uuid_access`.

// ───────────────────────── shared selects ─────────────────────────

async fn select_clients(state: &AppState, project_id: i32) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        r#"SELECT json_build_object('uuid', c.uuid::text, 'code', c.code, 'name', c.name)
           FROM "projectClient" x JOIN client c ON c.id = x."clientId"
           WHERE x."projectId" = $1 ORDER BY x.id"#,
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

async fn select_users(state: &AppState, project_id: i32) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        r#"SELECT json_build_object('uuid', u.uuid::text, 'username', u.username, 'name', u.name)
           FROM "projectUser" x JOIN users u ON u.id = x."userId"
           WHERE x."projectId" = $1 ORDER BY x.id"#,
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// `projectCompany.findMany({ select: { company: {} } })` + `toCompanyResponse`.
async fn select_companies(state: &AppState, project_id: i32) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        r#"SELECT to_jsonb(c) FROM "projectCompany" x JOIN company c ON c.id = x."companyId"
           WHERE x."projectId" = $1 ORDER BY x.id"#,
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| rowjson::company_response(r.0))
        .collect())
}

async fn link_clients(state: &AppState, project_id: i32, codes: &[String]) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO "projectClient" ("clientId", "projectId", status)
           SELECT id, $1, $3 FROM client WHERE code = ANY($2) ORDER BY id"#,
    )
    .bind(project_id)
    .bind(codes)
    .bind(STATUS_ACTIVE)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn link_users(state: &AppState, project_id: i32, usernames: &[String]) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO "projectUser" ("userId", "projectId", status)
           SELECT id, $1, $3 FROM users WHERE username = ANY($2) ORDER BY id"#,
    )
    .bind(project_id)
    .bind(usernames)
    .bind(STATUS_ACTIVE)
    .execute(&state.db)
    .await?;
    Ok(())
}

fn project_result(
    project: Value,
    clients: Vec<Value>,
    companies: Vec<Value>,
    users: Vec<Value>,
) -> Value {
    let mut project = project;
    rowjson::order(&mut project, rowjson::PROJECT_KEYS);
    rowjson::fix(&mut project);
    json!({ "project": project, "client": clients, "company": companies, "user": users })
}

// ───────────────────────── POST /projects ─────────────────────────

/// `create(dto, userId)`: seluruh pengecualian menjadi `{ success:false }`.
pub async fn create(state: &AppState, user_id: i32, input: &ProjectInput) -> Value {
    match create_inner(state, user_id, input).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = ?err, "create project failed");
            failure("create project failed")
        }
    }
}

async fn create_inner(state: &AppState, user_id: i32, input: &ProjectInput) -> ApiResult<Value> {
    let user: Option<(String, String)> =
        sqlx::query_as("SELECT username, name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let company_id = find_company_id(&state.db, user_id).await?;
    let max_project: i32 = match company_id {
        Some(c) => {
            sqlx::query_scalar::<_, i32>(r#"SELECT "maxProject" FROM company WHERE id = $1"#)
                .bind(c)
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(0)
        }
        None => 0,
    };
    let count_active = |company_id: i32| async move {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM "projectCompany" pc JOIN project p ON p.id = pc."projectId"
               WHERE pc."companyId" = $1 AND p.status = $2"#,
        )
        .bind(company_id)
        .bind(STATUS_ACTIVE)
        .fetch_one(&state.db)
        .await
    };
    if max_project > 0
        && let Some(c) = company_id
    {
        let current = count_active(c).await?;
        if current >= i64::from(max_project) {
            return Ok(json!({
                "success": false,
                "message": format!(
                    "Project limit reached ({current}/{max_project}). Please upgrade your plan to create more projects."
                ),
            }));
        }
    }

    let type_code = input
        .project_type_code
        .as_deref()
        .ok_or_else(|| internal("projectTypeCode missing"))?;
    let type_id: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "projectType" WHERE code = $1 ORDER BY id LIMIT 1"#)
            .bind(type_code)
            .fetch_optional(&state.db)
            .await?;

    let (Some(user), Some(company_id)) = (user, company_id) else {
        return Err(internal("user has no company"));
    };

    let code = input
        .code
        .as_deref()
        .ok_or_else(|| internal("code missing"))?;
    let exists: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM project p WHERE p.code = $1 AND EXISTS
             (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id AND pc."companyId" = $2))"#,
    )
    .bind(code)
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    if exists {
        return Ok(json!({ "success": false, "message": "Project code is already exists" }));
    }

    let type_id = type_id.ok_or_else(|| internal("project type not found"))?;
    let name = input
        .name
        .as_deref()
        .ok_or_else(|| internal("name missing"))?;
    let screenshot = input
        .screenshot_per_hour
        .and_then(as_i32)
        .ok_or_else(|| internal("invalid screenshotPerHour"))?;
    let total_hour = input
        .total_hour
        .and_then(as_i32)
        .ok_or_else(|| internal("invalid totalHour"))?;
    let start_date = input
        .start_date_value()
        .ok_or_else(|| internal("invalid startDate"))?;

    let (project_id, project_uuid, project_doc): (i32, String, Value) = sqlx::query_as(
        r#"INSERT INTO project AS p (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status, billable)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING p.id, p.uuid::text, to_jsonb(p)"#,
    )
    .bind(code)
    .bind(name)
    .bind(screenshot)
    .bind(total_hour)
    .bind(type_id)
    .bind(start_date)
    .bind(STATUS_ACTIVE)
    .bind(input.billable != Some(false))
    .fetch_one(&state.db)
    .await?;

    let mut clients = Vec::new();
    if let Some(codes) = input.clients_code.as_ref().filter(|c| !c.is_empty()) {
        link_clients(state, project_id, codes).await?;
        clients = select_clients(state, project_id).await?;
    }
    let usernames = input.users_user_name.clone().unwrap_or_default();
    link_users(state, project_id, &usernames).await?;
    let users = select_users(state, project_id).await?;

    sqlx::query(
        r#"INSERT INTO "projectCompany" ("companyId", "projectId", status) VALUES ($1, $2, $3)"#,
    )
    .bind(company_id)
    .bind(project_id)
    .bind(STATUS_ACTIVE)
    .execute(&state.db)
    .await?;
    let companies = select_companies(state, project_id).await?;

    if total_hour > 0 {
        sqlx::query(r#"INSERT INTO "projectHour" ("projectId", "totalHour") VALUES ($1, $2)"#)
            .bind(project_id)
            .bind(total_hour)
            .execute(&state.db)
            .await?;
    }
    if let Some(b) = &input.budget {
        budget::upsert(state, &project_uuid, b).await?;
    }
    if let Some(list) = input
        .manual_time_approvers
        .as_ref()
        .filter(|l| !l.is_empty())
    {
        approvers::set_approvers(state, &project_uuid, project_id, list).await?;
    }

    if count_active(company_id).await? == 1 {
        // Node: `this.mailService.sendFirstProject(...)` tanpa await.
        spawn_send(
            state,
            OwnedMail {
                to_email: user.0.clone(),
                to_name: user.1.clone(),
                subject: "Let's Get to Work: Your Projects are Ready".into(),
                text: None,
                template: "first-project",
                context: json!({
                    "email": user.0,
                    "url": state.config.var("PROJECT_URL"),
                    "founderName": "The Timemoz Team",
                }),
                category: "First Project",
                label: "send first project email",
            },
        );
    }

    Ok(json!({
        "success": true,
        "message": "create project success",
        "data": project_result(project_doc, clients, companies, users),
    }))
}

// ───────────────────────── PATCH /projects/:uuid ─────────────────────────

/// `update(uuid, dto, userId)`; pengecualian tidak ditangkap (500 seperti Node).
pub async fn update(
    state: &AppState,
    uuid: &str,
    user_id: i32,
    input: &ProjectInput,
) -> ApiResult<Value> {
    let company_id = find_company_id(&state.db, user_id)
        .await?
        .ok_or_else(|| internal("user has no company"))?;
    let current_code: Option<String> =
        sqlx::query_scalar("SELECT code FROM project WHERE uuid = $1::uuid LIMIT 1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    let current_code = current_code.ok_or_else(|| internal("project not found"))?;

    if let Some(code) = input.code.as_deref() {
        let found: Option<String> = sqlx::query_scalar(
            r#"SELECT p.code FROM project p WHERE p.code = $1 AND EXISTS
                 (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id AND pc."companyId" = $2)
               ORDER BY p.id LIMIT 1"#,
        )
        .bind(code)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        if found.is_some_and(|c| c != current_code) {
            return Ok(json!({ "success": false, "message": "Project code is already exists" }));
        }
    }

    let type_id: Option<i32> = match input.project_type_code.as_deref() {
        Some(code) => {
            let id: Option<i32> = sqlx::query_scalar(
                r#"SELECT id FROM "projectType" WHERE code = $1 ORDER BY id LIMIT 1"#,
            )
            .bind(code)
            .fetch_optional(&state.db)
            .await?;
            Some(id.ok_or_else(|| internal("project type not found"))?)
        }
        None => None,
    };

    let now = now_naive();
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE project AS p SET ");
    let mut set = qb.separated(", ");
    if let Some(code) = input.code.as_deref() {
        set.push("code = ").push_bind_unseparated(code);
    }
    if let Some(name) = input.name.as_deref() {
        set.push("name = ").push_bind_unseparated(name);
    }
    if let Some(v) = input.screenshot_per_hour {
        let v = as_i32(v).ok_or_else(|| internal("invalid screenshotPerHour"))?;
        set.push("\"screenshotPerHour\" = ")
            .push_bind_unseparated(v);
    }
    if let Some(id) = type_id {
        set.push("\"projectTypeId\" = ").push_bind_unseparated(id);
    }
    if input.start_date.is_some() {
        let d = input
            .start_date_value()
            .ok_or_else(|| internal("invalid startDate"))?;
        set.push("\"startDate\" = ").push_bind_unseparated(d);
    }
    if let Some(v) = input.total_hour {
        let v = as_i32(v).ok_or_else(|| internal("invalid totalHour"))?;
        set.push("\"totalHour\" = ").push_bind_unseparated(v);
    }
    if let Some(b) = input.billable {
        set.push("billable = ").push_bind_unseparated(b);
    }
    set.push("\"updatedAt\" = ").push_bind_unseparated(now);
    set.push("status = ").push_bind_unseparated(STATUS_ACTIVE);
    qb.push(" WHERE p.uuid = ")
        .push_bind(uuid)
        .push("::uuid RETURNING p.id, to_jsonb(p)");
    let updated: Option<(i32, Value)> = qb.build_query_as().fetch_optional(&state.db).await?;
    // P2025 di Prisma (record tidak ada) → 500.
    let (project_id, project_doc) =
        updated.ok_or_else(|| internal("record to update not found"))?;

    if input.clients_code.is_some() {
        sqlx::query(r#"DELETE FROM "projectClient" WHERE "projectId" = $1"#)
            .bind(project_id)
            .execute(&state.db)
            .await?;
    }
    if let Some(codes) = input.clients_code.as_ref().filter(|c| !c.is_empty()) {
        link_clients(state, project_id, codes).await?;
    }
    let clients = select_clients(state, project_id).await?;

    if let Some(names) = &input.users_user_name {
        sqlx::query(r#"DELETE FROM "projectUser" WHERE "projectId" = $1"#)
            .bind(project_id)
            .execute(&state.db)
            .await?;
        link_users(state, project_id, names).await?;
    }
    let users = select_users(state, project_id).await?;
    let companies = select_companies(state, project_id).await?;

    if let Some(b) = &input.budget {
        budget::upsert(state, uuid, b).await?;
    }
    if let Some(list) = &input.manual_time_approvers {
        approvers::set_approvers(state, uuid, project_id, list).await?;
    }

    Ok(json!({
        "success": true,
        "message": "update project success",
        "data": project_result(project_doc, clients, companies, users),
    }))
}

// ───────────────────────── DELETE /projects/:uuid ─────────────────────────

/// `delete(uuid)`: soft delete (`status = 0`).
pub async fn delete(state: &AppState, uuid: &str) -> ApiResult<Value> {
    let res = sqlx::query("UPDATE project SET status = $2 WHERE uuid = $1::uuid")
        .bind(uuid)
        .bind(STATUS_DELETED)
        .execute(&state.db)
        .await?;
    expect_one(res.rows_affected(), "project")?;
    Ok(
        json!({ "success": true, "message": "delete project success", "data": { "isDeleted": true } }),
    )
}

// ───────────────────────── project hour ─────────────────────────

/// `totalHour` project = jumlah seluruh `projectHour`. `SUM` kosong dihitung 0
/// (`COALESCE`); kolom `project.totalHour` bertipe Int. Node: `projects.service.ts:995-1012`
/// (`createHour`), `:1047-1064` (`updateHour`), `:1099-1116` (`deleteHour`, kini `_sum ?? 0`).
async fn sync_project_total(db: &mut sqlx::PgConnection, project_id: i32) -> ApiResult<()> {
    let sum: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM("totalHour"), 0)::int8 FROM "projectHour" WHERE "projectId" = $1"#,
    )
    .bind(project_id)
    .fetch_one(&mut *db)
    .await?;
    let sum = i32::try_from(sum).map_err(|_| internal("totalHour sum out of range"))?;
    sqlx::query(r#"UPDATE project SET "totalHour" = $2 WHERE id = $1"#)
        .bind(project_id)
        .bind(sum)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// `createHour(uuid, dto)`.
pub async fn create_hour(state: &AppState, uuid: &str, total_hour: Option<f64>) -> Value {
    let run = async {
        let project: Option<i32> =
            sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid LIMIT 1")
                .bind(uuid)
                .fetch_optional(&state.db)
                .await?;
        let project_id = project.ok_or_else(|| internal("project not found"))?;
        let hours = total_hour
            .and_then(as_i32)
            .ok_or_else(|| internal("invalid totalHour"))?;
        sqlx::query(r#"INSERT INTO "projectHour" ("projectId", "totalHour") VALUES ($1, $2)"#)
            .bind(project_id)
            .bind(hours)
            .execute(&state.db)
            .await?;
        sync_project_total(&mut *state.db.acquire().await?, project_id).await
    };
    match run.await {
        Ok(()) => {
            json!({ "success": true, "message": "create project hour success", "data": { "isCreated": true } })
        }
        Err(err) => {
            tracing::error!(error = ?err, "create project hour failed");
            json!({ "success": false, "message": "create project hour failed", "data": { "isCreated": false } })
        }
    }
}

/// `updateHour(uuid, dto)`.
pub async fn update_hour(state: &AppState, uuid: &str, total_hour: Option<f64>) -> Value {
    let run = async {
        let hour: Option<i32> = sqlx::query_scalar(
            r#"SELECT "projectId" FROM "projectHour" WHERE uuid = $1::uuid LIMIT 1"#,
        )
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
        let hours = total_hour
            .and_then(as_i32)
            .ok_or_else(|| internal("invalid totalHour"))?;
        sqlx::query(r#"UPDATE "projectHour" SET "totalHour" = $2 WHERE uuid = $1::uuid"#)
            .bind(uuid)
            .bind(hours)
            .execute(&state.db)
            .await?;
        sync_project_total(
            &mut *state.db.acquire().await?,
            hour.ok_or_else(|| internal("project hour not found"))?,
        )
        .await
    };
    match run.await {
        Ok(()) => {
            json!({ "success": true, "message": "update project hour success", "data": { "isUpdated": true } })
        }
        Err(err) => {
            tracing::error!(error = ?err, "update project hour failed");
            json!({ "success": false, "message": "update project hour failed", "data": { "isUpdated": false } })
        }
    }
}

/// `deleteHour(uuid)` (Node: `projects.service.ts:1080`). Perbaikan 2026-11-17 (deviasi 5):
/// hapus + hitung ulang `totalHour` dalam satu transaksi; jam terakhir -> total 0, sukses.
pub async fn delete_hour(state: &AppState, uuid: &str) -> Value {
    let run = async {
        let hour: Option<i32> = sqlx::query_scalar(
            r#"SELECT "projectId" FROM "projectHour" WHERE uuid = $1::uuid LIMIT 1"#,
        )
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
        let project_id = hour.ok_or_else(|| internal("project hour not found"))?;
        let mut tx = state.db.begin().await?;
        sqlx::query(r#"DELETE FROM "projectHour" WHERE uuid = $1::uuid"#)
            .bind(uuid)
            .execute(&mut *tx)
            .await?;
        sync_project_total(&mut tx, project_id).await?;
        tx.commit().await?;
        Ok::<(), ApiError>(())
    };
    match run.await {
        Ok(()) => {
            json!({ "success": true, "message": "delete project hour success", "data": { "isDeleted": true } })
        }
        Err(err) => {
            tracing::error!(error = ?err, "delete project hour failed");
            json!({ "success": false, "message": "delete project hour failed", "data": { "isDeleted": false } })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_truncation_matches_js_substring() {
        assert_eq!(truncate_name("short"), "short");
        assert_eq!(
            truncate_name("1234567890123456789012"),
            "1234567890123456789012"
        );
        assert_eq!(
            truncate_name("12345678901234567890123"),
            "1234567890123456789012..."
        );
    }

    #[test]
    fn failure_shape() {
        let v = failure("x");
        assert_eq!(v["success"], false);
        assert_eq!(v["data"], Value::Null);
    }
}
