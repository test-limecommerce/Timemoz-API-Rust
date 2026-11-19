//! `GET /admin/analytics/users` dan `GET /admin/analytics/live-activity`.
//! Node: `src/admin/admin.controller.ts:65/85` → `AdminService.getUserAnalytics` (:577) /
//! `getLiveActivity` (:800). Keduanya hanya digerbangi `JwtAdminAuthGuard` (tanpa `@OperatorRoles`)
//! dan menangkap SEMUA galat → `{ success: false, message, data: null }`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use chrono::{Duration, NaiveDateTime};
use fred::prelude::*;
use fred::types::Expiration;
use mz_core::jsfmt::{num_string, number as js_number, parse_int};
use mz_core::qs::{self, QVal};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiResult, AppState};
use mz_operator::Operator;
use serde_json::{Value, json};

use crate::dto::RawQuery;

/// `parseInt(String(+rawQuery), 10) || fallback`. `@Query('days') days?: number` di Nest dengan
/// `transform: true` → `+value` (string → `Number(s)`; array → `NaN` kecuali satu elemen).
fn query_int(q: &[(String, QVal)], key: &str, fallback: i64) -> i64 {
    let n = match qs::get(q, key) {
        None | Some(QVal::Obj) => f64::NAN,
        Some(QVal::One(s)) => js_number(s),
        Some(QVal::Many(items)) if items.len() == 1 => {
            items.first().map_or(f64::NAN, |s| js_number(s))
        }
        Some(QVal::Many(_)) => f64::NAN,
    };
    // `String(undefined)`/`String(NaN)` → NaN; `parseInt("1e+21")` → 1.
    parse_int(&num_string(n))
        .filter(|v| *v != 0)
        .unwrap_or(fallback)
}

fn failure_users() -> Json<Value> {
    Json(json!({
        "success": false,
        "message": "Error retrieving user analytics",
        "data": null,
    }))
}

type OwnerRow = (i32, i32, i32);

/// Anggota company (`userCompany.status = 1`) beserta ringkasan tracking-nya.
struct Member {
    id: i32,
    uuid: String,
    username: String,
    name: String,
    created_at: JsDate,
    has_downloaded_app: bool,
    has_track: bool,
    total_tracking: i64,
    last_tracking_at: Option<String>,
}

/// `formatJakartaDate`: `YYYY-MM-DD HH:mm` waktu Asia/Jakarta (UTC+7, tanpa DST).
fn format_jakarta(t: NaiveDateTime) -> String {
    (t + Duration::hours(7))
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Node: src/admin/admin.controller.ts:65 (`getUserAnalytics`).
pub async fn user_analytics(
    State(state): State<AppState>,
    _op: Operator,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    let days = query_int(&q, "days", 7);
    let page_number = query_int(&q, "pageNumber", 1);
    let items_per_page = query_int(&q, "itemsPerPage", 20);
    match compute_user_analytics(&state, days, page_number, items_per_page).await {
        Ok(Some(data)) => Ok(Json(json!({
            "success": true,
            "message": "Success get user analytics",
            "data": data,
        }))),
        Ok(None) => Ok(failure_users()),
        Err(err) => {
            tracing::warn!(error = %err, "GET /admin/analytics/users gagal");
            Ok(failure_users())
        }
    }
}

/// `None` = galat yang di Node dilempar Prisma (skip negatif / di luar rentang) → `success: false`.
async fn compute_user_analytics(
    state: &AppState,
    days: i64,
    page_number: i64,
    items_per_page: i64,
) -> Result<Option<Value>, sqlx::Error> {
    // `since.setDate(since.getDate() - days)`.
    let Some(since) = Duration::try_days(days).and_then(|d| now_naive().checked_sub_signed(d))
    else {
        return Ok(None);
    };
    let Some(skip) = page_number
        .checked_sub(1)
        .and_then(|p| p.checked_mul(items_per_page))
    else {
        return Ok(None);
    };
    // Prisma: `skip` negatif ditolak; `take` negatif membaca mundur dari titik skip.
    if skip < 0 {
        return Ok(None);
    }
    let backwards = items_per_page < 0;
    let take = items_per_page.checked_abs().unwrap_or(i64::MAX);

    // Pemilik = `userAccessLevel` aktif ber-role bawaan SUPER_ADMIN (`roleCodeFilter`: cabang `baseRole`
    // kosong untuk SUPER_ADMIN) dengan user aktif yang mendaftar sejak `since`.
    const OWNER_FROM: &str = r#"
        FROM "userAccessLevel" ual
        JOIN "accessLevel" al ON al.id = ual."accessLevelId"
        JOIN users u ON u.id = ual."userId"
        WHERE ual.status = 1
          AND al."isSystem" = true AND al.code = 'SUPER_ADMIN'
          AND u."createdAt" >= $1 AND u.status = 1"#;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {OWNER_FROM}"))
        .bind(since)
        .fetch_one(&state.db)
        .await?;
    let order = if backwards {
        r#"u."createdAt" ASC, ual.id ASC"#
    } else {
        r#"u."createdAt" DESC, ual.id DESC"#
    };
    let mut owners: Vec<OwnerRow> = sqlx::query_as(&format!(
        r#"SELECT ual.id, ual."userId", ual."companyId" {OWNER_FROM}
           ORDER BY {order} OFFSET $2 LIMIT $3"#
    ))
    .bind(since)
    .bind(skip)
    .bind(take)
    .fetch_all(&state.db)
    .await?;
    if backwards {
        owners.reverse();
    }

    let owner_ids: Vec<i32> = owners.iter().map(|o| o.1).collect();
    let company_ids: Vec<i32> = owners.iter().map(|o| o.2).collect();

    // Pemilik.
    type UserRow = (i32, String, String, String, JsDate, bool);
    let owner_rows: Vec<UserRow> = sqlx::query_as(
        r#"SELECT u.id, u.uuid::text, u.username, u.name, u."createdAt",
                  EXISTS (SELECT 1 FROM "userConnected" c WHERE c."userId" = u.id)
             FROM users u WHERE u.id = ANY($1)"#,
    )
    .bind(&owner_ids)
    .fetch_all(&state.db)
    .await?;
    let owner_by_id: HashMap<i32, UserRow> = owner_rows.into_iter().map(|r| (r.0, r)).collect();

    // Company.
    type CompanyRow = (i32, String, String, Option<String>);
    let company_rows: Vec<CompanyRow> =
        sqlx::query_as("SELECT id, uuid::text, name, telp FROM company WHERE id = ANY($1)")
            .bind(&company_ids)
            .fetch_all(&state.db)
            .await?;
    let company_by_id: HashMap<i32, CompanyRow> =
        company_rows.into_iter().map(|r| (r.0, r)).collect();

    // Anggota aktif tiap company + ringkasan timeline (agregasi di Postgres, bukan di Node).
    type MemberRow = (
        i32,
        i32,
        String,
        String,
        String,
        JsDate,
        bool,
        i64,
        Option<NaiveDateTime>,
        i64,
    );
    let member_rows: Vec<MemberRow> = sqlx::query_as(
        r#"SELECT uc."companyId", u.id, u.uuid::text, u.username, u.name, u."createdAt",
                  EXISTS (SELECT 1 FROM "userConnected" c WHERE c."userId" = u.id),
                  COALESCE(tl.n, 0)::bigint, tl.last_end, COALESCE(tl.total, 0)::bigint
             FROM "userCompany" uc
             JOIN users u ON u.id = uc."userId"
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS n, MAX(tt."endAt") AS last_end,
                        SUM(FLOOR(EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt")))) AS total
                   FROM tracking t
                   JOIN "trackingTimeline" tt ON tt."trackingId" = t.id AND tt.status = 1
                  WHERE t."userId" = u.id AND t.status = 1
             ) tl ON true
            WHERE uc."companyId" = ANY($1) AND uc.status = 1
            ORDER BY uc."companyId", uc.id"#,
    )
    .bind(&company_ids)
    .fetch_all(&state.db)
    .await?;
    let mut members_by_company: HashMap<i32, Vec<Member>> = HashMap::new();
    for (company_id, id, uuid, username, name, created_at, downloaded, n, last_end, total) in
        member_rows
    {
        members_by_company
            .entry(company_id)
            .or_default()
            .push(Member {
                id,
                uuid,
                username,
                name,
                created_at,
                has_downloaded_app: downloaded,
                has_track: n > 0,
                total_tracking: total,
                last_tracking_at: last_end.map(format_jakarta),
            });
    }

    let mut result = Vec::with_capacity(owners.len());
    for (_, owner_id, company_id) in &owners {
        let Some((id, uuid, username, name, created_at, downloaded)) = owner_by_id.get(owner_id)
        else {
            continue;
        };
        let company = company_by_id.get(company_id);
        let members = members_by_company
            .get(company_id)
            .map_or(&[][..], Vec::as_slice);
        let employees: Vec<Value> = members
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "uuid": m.uuid,
                    "username": m.username,
                    "name": m.name,
                    "createdAt": m.created_at,
                    "hasDownloadedApp": m.has_downloaded_app,
                    "hasTrack": m.has_track,
                    "totalTracking": m.total_tracking,
                    "lastTrackingAt": m.last_tracking_at,
                })
            })
            .collect();
        let has_track = members.iter().any(|m| m.has_track);
        let total_tracking: i64 = members.iter().map(|m| m.total_tracking).sum();
        // Perbandingan string `YYYY-MM-DD HH:mm` = urutan waktu.
        let last_tracking_at = members
            .iter()
            .filter_map(|m| m.last_tracking_at.as_deref())
            .max();
        result.push(json!({
            "id": id,
            "uuid": uuid,
            "username": username,
            "name": name,
            "createdAt": created_at,
            "hasDownloadedApp": downloaded,
            "company": company.map(|(cid, cuuid, cname, telp)| json!({
                "id": cid,
                "uuid": cuuid,
                "name": cname,
                "telp": telp,
            })),
            "tracking": {
                "hasTracked": has_track,
                "totalTracking": total_tracking,
                "lastTrackingAt": last_tracking_at,
            },
            "employees": employees,
        }));
    }

    Ok(Some(json!({
        "total": total,
        "pageNumber": page_number,
        "itemsPerPage": items_per_page,
        "result": result,
    })))
}

// ── live-activity ───────────────────────────────────────────────────────────

const LIVE_ACTIVITY_TTL_MS: i64 = 60_000;

/// Node: src/admin/admin.controller.ts:85 (`getLiveActivity`). Angka global seluruh platform,
/// di-cache Redis 60 detik (kunci `admin-live-activity-<tanggal WIB>`, nilai JSON, sama dengan Node).
pub async fn live_activity(State(state): State<AppState>, _op: Operator) -> ApiResult<Json<Value>> {
    let date = (now_naive() + Duration::hours(7))
        .format("%Y-%m-%d")
        .to_string();
    let key = format!("admin-live-activity-{date}");

    // Redis bermasalah tidak boleh menjatuhkan endpoint — lanjut hitung dari DB.
    let cached: Option<String> = state.redis.next().get(&key).await.unwrap_or(None);
    if let Some(raw) = cached
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
        && !v.is_null()
    {
        return Ok(Json(json!({
            "success": true,
            "message": "Success get live activity",
            "data": v,
        })));
    }

    match compute_live_activity(&state, &date).await {
        Ok(data) => {
            if let Ok(raw) = serde_json::to_string(&data) {
                let _ = state
                    .redis
                    .next()
                    .set::<(), _, _>(
                        &key,
                        raw,
                        Some(Expiration::PX(LIVE_ACTIVITY_TTL_MS)),
                        None,
                        false,
                    )
                    .await;
            }
            Ok(Json(json!({
                "success": true,
                "message": "Success get live activity",
                "data": data,
            })))
        }
        Err(err) => {
            tracing::warn!(error = %err, "GET /admin/analytics/live-activity gagal");
            Ok(Json(json!({
                "success": false,
                "message": "Error retrieving live activity",
                "data": null,
            })))
        }
    }
}

async fn compute_live_activity(state: &AppState, date: &str) -> Result<Value, sqlx::Error> {
    // User unik yang punya segmen tracking aktif pada tanggal itu.
    let tracking = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(DISTINCT t."userId")
             FROM "tracking" t
             JOIN "trackingTimeline" tl
                  ON tl."trackingId" = t."id" AND tl."status" = 1 AND tl."endAt" > tl."startAt"
            WHERE t."takenDate" = $1::date"#,
    )
    .bind(date)
    .fetch_one(&state.db);
    // Satu sales hanya bisa punya satu kunjungan aktif (partial unique index).
    let visits = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM "storeVisit" WHERE "state" = 'active'"#,
    )
    .fetch_one(&state.db);
    let (tracking, visits) = tokio::try_join!(tracking, visits)?;
    Ok(json!({
        "date": date,
        "totalUserTracking": tracking,
        "totalSalesOnVisit": visits,
        "generatedAt": JsDate::now().to_js_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(raw: &str) -> Vec<(String, QVal)> {
        qs::parse(raw)
    }

    #[test]
    fn query_int_follows_nest_number_transform_then_parse_int() {
        assert_eq!(query_int(&q(""), "days", 7), 7);
        assert_eq!(query_int(&q("days=3"), "days", 7), 3);
        assert_eq!(query_int(&q("days=abc"), "days", 7), 7);
        assert_eq!(query_int(&q("days=0"), "days", 7), 7);
        assert_eq!(query_int(&q("days="), "days", 7), 7);
        assert_eq!(query_int(&q("days=5.7"), "days", 7), 5);
        assert_eq!(query_int(&q("days=1e3"), "days", 7), 1000);
        assert_eq!(query_int(&q("days=1&days=2"), "days", 7), 7);
        assert_eq!(query_int(&q("days=-2"), "days", 7), -2);
    }

    #[test]
    fn jakarta_format_is_utc_plus_seven() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 17).and_then(|d| d.and_hms_opt(20, 5, 9));
        assert_eq!(t.map(format_jakarta).as_deref(), Some("2026-11-18 03:05"));
    }
}
