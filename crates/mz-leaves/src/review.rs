//! `LeavesService` dasbor reviewer (`leaves.service.ts`): daftar, detail, dokumen,
//! tolak, minta revisi, kalender. Persetujuan ada di [`crate::approve`].

use chrono::{Datelike, NaiveDate};
use mz_core::{ApiError, ApiResult, AppState};
use mz_leave::enums::LeaveStatus;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::approve::find_visible;
use crate::dto::{CalendarQuery, LeavesQuery};
use crate::model::{Include, RequestRow, RequestView, Visibility, fetch_by_id, select_where};
use crate::notify;
use crate::util::{invalid_date, iso_year_bounds, js_local_date};

/// `{ data, meta: { total, page, limit, totalPages } }` + `summary`.
fn meta(total: i64, page: i64, limit: i64) -> Value {
    #[allow(clippy::cast_precision_loss)]
    let pages = (total as f64 / limit as f64).ceil();
    json!({ "total": total, "page": page, "limit": limit, "totalPages": mz_core::jsfmt::num(pages) })
}

/// Filter rentang bulan: `startDate` dalam bulan, `endDate` dalam bulan, atau melintasinya.
fn push_month_overlap(qb: &mut QueryBuilder<'_, Postgres>, start: NaiveDate, end: NaiveDate) {
    qb.push(r#" AND ((lr."startDate" >= "#)
        .push_bind(start)
        .push(r#" AND lr."startDate" <= "#)
        .push_bind(end)
        .push(r#") OR (lr."endDate" >= "#)
        .push_bind(start)
        .push(r#" AND lr."endDate" <= "#)
        .push_bind(end)
        .push(r#") OR (lr."startDate" <= "#)
        .push_bind(start)
        .push(r#" AND lr."endDate" >= "#)
        .push_bind(end)
        .push("))");
}

/// `listRequests(reviewerId, query)` (`leaves.service.ts:153`).
pub async fn list_requests(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    q: &LeavesQuery,
) -> ApiResult<Value> {
    let page = q.page.unwrap_or(1);
    let limit = q.limit.unwrap_or(20);
    let skip = page.saturating_sub(1).saturating_mul(limit);
    let vis = Visibility::load(&state.db, reviewer_id, company_id).await?;

    let mut month_range = None;
    let mut year_range = None;
    match (q.year.filter(|y| *y != 0), q.month.filter(|m| *m != 0)) {
        (Some(y), Some(m)) => {
            let start = js_local_date(y, m - 1, 1).ok_or_else(invalid_date)?;
            let end = js_local_date(y, m, 0).ok_or_else(invalid_date)?;
            month_range = Some((start, end));
        }
        (Some(y), None) => year_range = Some(iso_year_bounds(y).ok_or_else(invalid_date)?),
        _ => {}
    }
    let filters = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#"lr."companyId" = "#).push_bind(company_id);
        if let Some(s) = &q.status {
            qb.push(" AND lr.status::text = ").push_bind(s.clone());
        }
        if let Some(t) = q.leave_type_id.filter(|t| *t != 0) {
            qb.push(r#" AND lr."leaveTypeId" = "#).push_bind(t);
        }
        if let Some(u) = q.user_id.filter(|u| *u != 0) {
            qb.push(r#" AND lr."userId" = "#).push_bind(u);
        }
        if let Some((start, end)) = year_range {
            qb.push(r#" AND lr."startDate" >= "#)
                .push_bind(start)
                .push(r#" AND lr."startDate" <= "#)
                .push_bind(end);
        }
        qb.push(" AND ");
        vis.push(qb, "lr");
        if let Some((start, end)) = month_range {
            push_month_overlap(qb, start, end);
        }
    };

    let mut count = QueryBuilder::new(r#"SELECT COUNT(*) FROM "leaveRequest" lr WHERE "#);
    filters(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut items = select_where();
    filters(&mut items);
    items
        .push(r#" ORDER BY lr."createdAt" DESC, lr.id DESC LIMIT "#)
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<RequestRow> = items.build_query_as().fetch_all(&state.db).await?;
    let data: Vec<RequestView> = rows.iter().map(|r| r.view(Include::ALL)).collect();

    let summary = build_summary(state, company_id, vis).await?;
    Ok(json!({ "data": data, "meta": meta(total, page, limit), "summary": summary }))
}

/// `buildSummary(...)`: pending, disetujui bulan ini (UTC), tumpang tindih, rata-rata jam proses.
async fn build_summary(state: &AppState, company_id: i32, vis: Visibility) -> ApiResult<Value> {
    let now = mz_core::time::now_naive();
    let month_start = js_local_date(i64::from(now.year()), i64::from(now.month0()), 1)
        .ok_or_else(invalid_date)?
        .and_time(chrono::NaiveTime::MIN);
    let next_month_start = js_local_date(i64::from(now.year()), i64::from(now.month0()) + 1, 1)
        .ok_or_else(invalid_date)?
        .and_time(chrono::NaiveTime::MIN);

    let mut pending =
        QueryBuilder::new(r#"SELECT COUNT(*) FROM "leaveRequest" lr WHERE lr."companyId" = "#);
    pending.push_bind(company_id).push(" AND ");
    vis.push(&mut pending, "lr");
    pending.push(r#" AND lr.status = 'PENDING'::"LeaveStatus""#);
    let total_pending: i64 = pending.build_query_scalar().fetch_one(&state.db).await?;

    let mut approved =
        QueryBuilder::new(r#"SELECT COUNT(*) FROM "leaveRequest" lr WHERE lr."companyId" = "#);
    approved.push_bind(company_id).push(" AND ");
    vis.push(&mut approved, "lr");
    approved
        .push(r#" AND lr.status = 'APPROVED'::"LeaveStatus" AND lr."reviewedAt" >= "#)
        .push_bind(month_start)
        .push(r#" AND lr."reviewedAt" < "#)
        .push_bind(next_month_start);
    let total_approved: i64 = approved.build_query_scalar().fetch_one(&state.db).await?;

    let mut overlap = QueryBuilder::new(
        r#"SELECT COUNT(DISTINCT lr1.id)::int FROM "leaveRequest" lr1
           JOIN "leaveRequest" lr2
             ON lr1."companyId" = lr2."companyId"
            AND lr1."userId" != lr2."userId"
            AND lr1."startDate" <= lr2."endDate"
            AND lr1."endDate" >= lr2."startDate"
          WHERE lr1."companyId" = "#,
    );
    overlap
        .push_bind(company_id)
        .push(r#" AND lr1.status IN ('PENDING', 'APPROVED') AND lr2.status IN ('PENDING', 'APPROVED') AND "#);
    vis.push(&mut overlap, "lr1");
    overlap.push(" AND ");
    vis.push(&mut overlap, "lr2");
    let overlapping: i32 = overlap.build_query_scalar().fetch_one(&state.db).await?;

    let mut avg = QueryBuilder::new(
        r#"SELECT ROUND(AVG(EXTRACT(EPOCH FROM ("reviewedAt" - "createdAt")) / 3600)::numeric, 1)::float8
             FROM "leaveRequest"
            WHERE "companyId" = "#,
    );
    avg.push_bind(company_id)
        .push(r#" AND "reviewedAt" IS NOT NULL AND status IN ('APPROVED', 'REJECTED') AND "#);
    vis.push(&mut avg, r#""leaveRequest""#);
    let avg_hours: Option<f64> = avg.build_query_scalar().fetch_one(&state.db).await?;

    Ok(json!({
        "totalPending": total_pending,
        "totalApprovedThisMonth": total_approved,
        "overlappingRequests": overlapping,
        "avgProcessingHours": avg_hours.map(mz_core::jsfmt::num),
    }))
}

/// `getRequest(reviewerId, uuid)`.
pub async fn get_request(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
) -> ApiResult<RequestView> {
    find_visible(state, reviewer_id, company_id, uuid)
        .await?
        .map(|r| r.view(Include::ALL))
        .ok_or_else(|| ApiError::not_found("Leave request not found"))
}

/// Signed URL dokumen (`getRequestDocumentUrl` / `getMyRequestDocumentUrl`): MIME dari
/// ekstensi key, bukan input klien; nama berkas = segmen terakhir key.
pub async fn signed_document(
    state: &AppState,
    key: Option<String>,
) -> ApiResult<mz_files::SignedReadUrl> {
    let key = key.ok_or_else(|| ApiError::not_found("Document not found"))?;
    let ext = key.rsplit('.').next().unwrap_or_default().to_lowercase();
    let mime = mz_leave::document_mime(&ext).unwrap_or("application/octet-stream");
    let filename = key.rsplit('/').next();
    Ok(mz_files::get_signed_object_url(
        state,
        &key,
        &mz_files::SignedObjectOpts {
            content_type: Some(Some(mime)),
            filename,
        },
        chrono::Utc::now().timestamp_millis(),
    )
    .await?)
}

/// `getRequestDocumentUrl(reviewerId, uuid)`.
pub async fn get_request_document(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
) -> ApiResult<mz_files::SignedReadUrl> {
    let row = find_visible(state, reviewer_id, company_id, uuid).await?;
    signed_document(state, row.and_then(|r| r.document_key)).await
}

/// `rejectRequest(reviewerId, uuid, dto)`.
pub async fn reject_request(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
    rejection_reason: String,
) -> ApiResult<RequestView> {
    let request = find_visible(state, reviewer_id, company_id, uuid)
        .await?
        .ok_or_else(|| ApiError::not_found("Leave request not found"))?;
    if request.status == LeaveStatus::APPROVED.as_str() {
        return Err(ApiError::bad_request(
            "Cannot reject an already approved request",
        ));
    }
    let now = mz_core::time::now_naive();
    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET status = 'REJECTED'::"LeaveStatus", "rejectionReason" = $2, "reviewedById" = $3,
                  "reviewedAt" = $4, "updatedAt" = $4
            WHERE id = $1"#,
    )
    .bind(request.id)
    .bind(&rejection_reason)
    .bind(reviewer_id)
    .bind(now)
    .execute(&state.db)
    .await?;
    let rejected = fetch_by_id(&state.db, request.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    notify::decision(
        state,
        &rejected,
        reviewer_id,
        mz_leave::mail::LeaveDecision::Rejected {
            rejection_reason: Some(rejection_reason),
        },
    );
    Ok(rejected.view(Include::USER_TYPE))
}

/// `requestRevision(reviewerId, uuid, dto)`.
pub async fn request_revision(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
    revision_note: Option<String>,
) -> ApiResult<RequestView> {
    let request = find_visible(state, reviewer_id, company_id, uuid)
        .await?
        .ok_or_else(|| ApiError::not_found("Leave request not found"))?;
    if request.status != LeaveStatus::PENDING.as_str() {
        return Err(ApiError::bad_request(
            "Only pending requests can be marked as needs revision",
        ));
    }
    let now = mz_core::time::now_naive();
    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET status = 'NEEDS_REVISION'::"LeaveStatus", "revisionNote" = $2, "reviewedById" = $3,
                  "reviewedAt" = $4, "updatedAt" = $4
            WHERE id = $1"#,
    )
    .bind(request.id)
    .bind(revision_note.as_deref())
    .bind(reviewer_id)
    .bind(now)
    .execute(&state.db)
    .await?;
    let updated = fetch_by_id(&state.db, request.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    Ok(updated.view(Include::USER_TYPE))
}

#[derive(Serialize)]
struct CalendarDay {
    date: String,
    leaves: Vec<RequestView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalendarOut {
    year: i64,
    month: i64,
    total_requests: usize,
    calendar: Vec<CalendarDay>,
}

/// `getCalendar(reviewerId, query)` (`leaves.service.ts:937`).
pub async fn get_calendar(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    q: &CalendarQuery,
) -> ApiResult<Value> {
    let month_start = js_local_date(q.year, q.month - 1, 1).ok_or_else(invalid_date)?;
    let month_end = js_local_date(q.year, q.month, 0).ok_or_else(invalid_date)?;
    let vis = Visibility::load(&state.db, reviewer_id, company_id).await?;

    let mut qb = select_where();
    qb.push(r#"lr."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND lr.status IN ('PENDING'::"LeaveStatus", 'APPROVED'::"LeaveStatus") AND "#);
    vis.push(&mut qb, "lr");
    push_month_overlap(&mut qb, month_start, month_end);
    if let Some(u) = q.user_id.filter(|u| *u != 0) {
        qb.push(r#" AND lr."userId" = "#).push_bind(u);
    }
    qb.push(r#" ORDER BY lr."startDate" ASC, u.name ASC, lr.id ASC"#);
    let requests: Vec<RequestRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let mut calendar = Vec::new();
    for date in month_start.iter_days().take_while(|d| *d <= month_end) {
        let leaves: Vec<RequestView> = requests
            .iter()
            .filter(|r| date >= r.start_date && date <= r.end_date)
            .map(|r| r.view(Include::USER_TYPE))
            .collect();
        if !leaves.is_empty() {
            calendar.push(CalendarDay {
                date: mz_leave::day_count::to_date_key(date),
                leaves,
            });
        }
    }
    serde_json::to_value(CalendarOut {
        year: q.year,
        month: q.month,
        total_requests: requests.len(),
        calendar,
    })
    .map_err(ApiError::internal)
}
