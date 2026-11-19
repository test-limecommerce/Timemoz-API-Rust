//! Port `src/manual-time-approvals` (service). `approve-manual-time.ts` ada di
//! `mz_timelines::approve` (dipakai juga auto-approve `createManualTime`).
//! Route berkas (lampiran) ada di `attachments.rs`.
//!
//! Nudge lembur (`OvertimeJobsService.nudge`) sesudah approve: `mz_overtime::nudge_for_timelines`
//! (aktif hanya bila `MZ_JOBS` memuat `overtime-recalc`/`all`; lihat deviasi).

use axum::Json;
use axum::extract::{Query, State};
use axum::response::Response;
use chrono::NaiveDateTime;
use mz_account::util::encode_uri_component;
use mz_core::company::find_company_id;
use mz_core::enums::{BaseRole, effective_role_code, sees_whole_company};
use mz_core::extract::{PathParams, Validate, ValidatedJson, ValidatedQuery};
use mz_core::iso8601::is_iso8601;
use mz_core::mail::{OwnedMail, spawn_send};
use mz_core::time::{JsDate, now_naive, parse_js_date};
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::util::{defined, parse_uuid};
use mz_timelines::approve::{ApproveOutcome, approve_in_tx, format_day_month_year, reject_in_tx};

/// Pesan bila seluruh rentang manual time sudah tercatat timeline lain.
pub const FULLY_COVERED_MESSAGE: &str =
    "This time range is already fully recorded by tracking or approved manual time";

/// `to_char` → bentuk `Date.toJSON()` (kolom `timestamp(3)`).
fn ts(expr: &str) -> String {
    format!(r#"to_char({expr}, 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#)
}

// ───────────────────────── format teks ─────────────────────────

/// `formatManualTimeSeconds` (`format-manual-time-duration.ts`).
pub use mz_timelines::approve::format_manual_time_seconds as format_seconds;

/// `formatManualTimeDuration`: `moment.diff(..., 'seconds')` memotong ke nol.
pub fn format_duration(start: NaiveDateTime, end: NaiveDateTime) -> String {
    let diff_ms = end.and_utc().timestamp_millis() - start.and_utc().timestamp_millis();
    format_seconds(diff_ms / 1000)
}

/// `moment.utc(x).format('DD MMMM YYYY')`.
fn format_date(d: NaiveDateTime) -> String {
    d.format("%d %B %Y").to_string()
}

// ───────────────────────── approve / reject (transaksi) ─────────────────────────

// ───────────────────────── akses ─────────────────────────

/// Peran efektif dari baris `userAccessLevel` TERBARU user (id desc, tanpa filter status),
/// seperti `effectiveRoleCodeOfRow(userInfo.userAccessLevel[0])`.
pub async fn latest_role_code(db: &sqlx::PgPool, user_id: i32) -> sqlx::Result<Option<String>> {
    let row: Option<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(code, is_system, base)| {
        let base = base.and_then(|b| b.parse::<BaseRole>().ok());
        effective_role_code(&code, is_system, base)
    }))
}

/// `canUserActOnEntry`.
async fn can_act_on_entry(state: &AppState, user_id: i32, project_id: i32) -> sqlx::Result<bool> {
    let role = latest_role_code(&state.db, user_id).await?;
    if sees_whole_company(role.as_deref())
        && let Some(company_id) = find_company_id(&state.db, user_id).await?
    {
        let owned: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectCompany" WHERE "projectId" = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(project_id)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        if owned.is_some() {
            return Ok(true);
        }
    }
    is_active_approver(&state.db, user_id, project_id).await
}

/// `projectManualTimeApprover` aktif (userId, projectId, status 1).
pub async fn is_active_approver(
    db: &sqlx::PgPool,
    user_id: i32,
    project_id: i32,
) -> sqlx::Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "projectManualTimeApprover"
           WHERE "userId" = $1 AND "projectId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(project_id)
    .fetch_optional(db)
    .await?;
    Ok(found.is_some())
}

// ───────────────────────── mail & burn ─────────────────────────

struct StatusMail<'a> {
    staff_email: &'a str,
    staff_name: &'a str,
    is_approved: bool,
    date: String,
    duration: String,
    project_name: &'a str,
    reason: Option<&'a str>,
    rejected_reason: Option<&'a str>,
}

/// `MailService.sendManualTimeStatus` (fire-and-forget, `.catch(() => {})`).
fn send_status_mail(state: &AppState, m: &StatusMail<'_>) {
    let label = if m.is_approved {
        "Approved"
    } else {
        "Rejected"
    };
    let subject = format!("Your Manual Time Request Has Been {label}");
    spawn_send(
        state,
        OwnedMail {
            to_email: m.staff_email.to_owned(),
            to_name: m.staff_name.to_owned(),
            subject,
            text: None,
            template: "manual-time-status",
            context: json!({
                "staffName": m.staff_name,
                "isApproved": m.is_approved,
                "date": m.date,
                "duration": m.duration,
                "projectName": m.project_name,
                "reason": m.reason,
                "rejectedReason": m.rejected_reason,
            }),
            category: "Manual Time Status",
            label: "send manual time status email",
        },
    );
}

/// `recalculateBurnInBackground`.
fn recalc_burn_background(state: &AppState, project_id: i32) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        if let Err(err) = mz_projects::budget::recalculate_burn(&state, project_id).await {
            tracing::error!(error = %err.status(), project_id, "Budget recalculate error");
        }
    });
}

// ───────────────────────── approve / reject (service) ─────────────────────────

#[derive(sqlx::FromRow)]
struct EntryRow {
    id: i32,
    project_id: i32,
    manual_time_status: Option<String>,
    start_at: JsDate,
    end_at: JsDate,
    reason: Option<String>,
    project_name: String,
    owner_name: String,
    owner_username: String,
    owner_id: i32,
}

async fn load_entry(state: &AppState, uuid: &str) -> sqlx::Result<Option<EntryRow>> {
    sqlx::query_as(
        r#"SELECT t.id, t."projectId" AS project_id, t."manualTimeStatus"::text AS manual_time_status,
                  t."startAt" AS start_at, t."endAt" AS end_at, t.reason,
                  p.name AS project_name, u.name AS owner_name, u.username AS owner_username,
                  tr."userId" AS owner_id
           FROM "trackingTimeline" t
           JOIN project p ON p.id = t."projectId"
           JOIN tracking tr ON tr.id = t."trackingId"
           JOIN users u ON u.id = tr."userId"
           WHERE t.uuid = $1::uuid AND t.status IN (0, 1) AND t."isOffline" = true
           LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await
}

fn plain_failure(message: &str) -> Value {
    json!({ "success": false, "message": message })
}

async fn updated_entry(state: &AppState, id: i32, with_rejected: bool) -> sqlx::Result<Value> {
    // Urutan `select` Node: uuid, manualTimeStatus, manualTimeApprovalSource, [rejectedReason],
    // approvedAt, approvedBy (`json_build_object` menjaga urutan argumen; jsonb tidak).
    let rejected = if with_rejected {
        r#"'rejectedReason', t."rejectedReason","#
    } else {
        ""
    };
    let approved_at = ts(r#"t."approvedAt""#);
    let sql = format!(
        r#"SELECT json_build_object('uuid', t.uuid::text, 'manualTimeStatus', t."manualTimeStatus"::text,
              'manualTimeApprovalSource', t."manualTimeApprovalSource"::text,
              {rejected}'approvedAt', {approved_at},
              'approvedBy', (SELECT json_build_object('uuid', u.uuid::text, 'name', u.name)
                             FROM users u WHERE u.id = t."approvedById"))
           FROM "trackingTimeline" t WHERE t.id = $1"#
    );
    let v: Option<Value> = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    Ok(v.unwrap_or(Value::Null))
}

/// Hasil internal: respons sukses, atau galat HTTP yang dilempar ulang (`HttpException`).
enum Flow {
    Done(Value),
    Http(ApiError),
}

async fn approve_inner(state: &AppState, uuid: &str, user_id: i32) -> sqlx::Result<Flow> {
    let Some(t) = load_entry(state, uuid).await? else {
        return Ok(Flow::Done(plain_failure("Manual time entry not found")));
    };
    if t.manual_time_status.as_deref() != Some("PENDING") {
        return Ok(Flow::Done(plain_failure(
            "Only PENDING entries can be approved",
        )));
    }
    if !can_act_on_entry(state, user_id, t.project_id).await? {
        return Ok(Flow::Done(plain_failure(
            "You are not authorized to approve this entry",
        )));
    }

    // T4: approve manager hanya memegang lock tracking.
    let mut tx = state.db.begin().await?;
    let outcome = approve_in_tx(&mut tx, t.id, Some(user_id), "MANUAL", None).await?;
    match &outcome {
        ApproveOutcome::Approved { .. } => tx.commit().await?,
        _ => tx.rollback().await?,
    }
    log_outcome(uuid, user_id, &outcome);
    let (requested, approved_seconds, adjusted) = match outcome {
        // `over-limit` tak terjadi: route ini tidak memberi `maxApprovedSeconds`.
        ApproveOutcome::NotPending | ApproveOutcome::OverLimit { .. } => {
            return Ok(Flow::Done(plain_failure(
                "Only PENDING entries can be approved",
            )));
        }
        // 409, bukan 200 + success:false (dashboard menganggap 200 sukses).
        ApproveOutcome::Covered { .. } => {
            return Ok(Flow::Http(ApiError::conflict(FULLY_COVERED_MESSAGE)));
        }
        ApproveOutcome::Approved {
            requested,
            approved_seconds,
            adjusted,
            ..
        } => (requested, approved_seconds, adjusted),
    };

    // Node: manual-time-approvals.service.ts:290.
    mz_overtime::nudge_for_timelines(
        state,
        Some(t.owner_id),
        vec![Some(t.project_id)],
        vec![mz_overtime::NudgeInterval::new(
            requested.start,
            requested.end,
        )],
    );
    let updated = updated_entry(state, t.id, false).await?;
    recalc_burn_background(state, t.project_id);
    send_status_mail(
        state,
        &StatusMail {
            staff_email: &t.owner_username,
            staff_name: &t.owner_name,
            is_approved: true,
            date: format_day_month_year(requested.start),
            duration: format_seconds(approved_seconds),
            project_name: &t.project_name,
            reason: t.reason.as_deref(),
            rejected_reason: None,
        },
    );
    let message = if adjusted {
        "Manual time approved; the part already recorded was removed"
    } else {
        "Manual time approved"
    };
    Ok(Flow::Done(
        json!({ "success": true, "message": message, "data": updated }),
    ))
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:189 (`approve`)
pub async fn approve(state: &AppState, uuid: &str, user_id: i32) -> ApiResult<Value> {
    match approve_inner(state, uuid, user_id).await {
        Ok(Flow::Done(v)) => Ok(v),
        Ok(Flow::Http(e)) => Err(e),
        Err(err) => {
            tracing::error!(error = %err, "manual-time-approvals-approve");
            Ok(json!({ "success": false, "message": err.to_string(), "data": null }))
        }
    }
}

async fn reject_inner(
    state: &AppState,
    uuid: &str,
    reason: &str,
    user_id: i32,
) -> sqlx::Result<Value> {
    let Some(t) = load_entry(state, uuid).await? else {
        return Ok(plain_failure("Manual time entry not found"));
    };
    if t.manual_time_status.as_deref() != Some("PENDING") {
        return Ok(plain_failure("Only PENDING entries can be rejected"));
    }
    if !can_act_on_entry(state, user_id, t.project_id).await? {
        return Ok(plain_failure("You are not authorized to reject this entry"));
    }
    let mut tx = state.db.begin().await?;
    let rejected = reject_in_tx(&mut tx, t.id, user_id, reason).await?;
    tx.commit().await?;
    if !rejected {
        return Ok(plain_failure("Only PENDING entries can be rejected"));
    }
    // Reject tak mengubah angka lembur: tanpa nudge.
    let updated = updated_entry(state, t.id, true).await?;
    send_status_mail(
        state,
        &StatusMail {
            staff_email: &t.owner_username,
            staff_name: &t.owner_name,
            is_approved: false,
            date: format_date(t.start_at.0),
            duration: format_duration(t.start_at.0, t.end_at.0),
            project_name: &t.project_name,
            reason: t.reason.as_deref(),
            rejected_reason: Some(reason),
        },
    );
    Ok(json!({ "success": true, "message": "Manual time rejected", "data": updated }))
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:291 (`reject`)
pub async fn reject(state: &AppState, uuid: &str, reason: &str, user_id: i32) -> Value {
    match reject_inner(state, uuid, reason, user_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "manual-time-approvals-reject");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

/// `logApproveOutcome` → app-events (di sini `tracing::warn!`).
fn log_outcome(uuid: &str, approver_id: i32, outcome: &ApproveOutcome) {
    match outcome {
        ApproveOutcome::Approved {
            requested,
            pieces,
            adjusted: true,
            approved_seconds,
            extra_uuids,
        } => tracing::warn!(
            event = "manual_time_approved_adjusted", manual_timeline_uuid = uuid, approver_id,
            requested = ?requested, approved = ?pieces, approved_seconds, extra_uuids = ?extra_uuids
        ),
        ApproveOutcome::Covered { requested } => tracing::warn!(
            event = "manual_time_approval_fully_covered", manual_timeline_uuid = uuid, approver_id,
            requested = ?requested
        ),
        _ => {}
    }
}

// ───────────────────────── daftar ─────────────────────────

/// `withAttachmentUrls`: `fileUrl` = path proxy ber-JWT; `fileFullpath` tidak keluar.
const ATTACHMENTS_SQL: &str = r#"COALESCE((SELECT json_agg(json_build_object('uuid', a.uuid::text,
    'fileName', a."fileName",
    'fileUrl', '/manual-time-approvals/attachments/' || a.uuid::text || '/file') ORDER BY a.id)
    FROM "manualTimeAttachment" a WHERE a."trackingTimelineId" = t.id AND a.status = 1), '[]'::json)"#;

/// Query `GET /manual-time-approvals` (`QueryManualTimeApprovalDto`).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApprovalsQuery {
    pub project_uuid: Option<String>,
    pub status: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub user_uuid: Option<String>,
}

impl Validate for ApprovalsQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let uuid = |out: &mut Vec<String>, k: &str, v: &Option<String>| {
            if v.as_deref().is_some_and(|s| !is_uuid(s)) {
                out.push(format!("{k} must be a UUID"));
            }
        };
        uuid(&mut out, "projectUuid", &self.project_uuid);
        if self
            .status
            .as_deref()
            .is_some_and(|s| !["PENDING", "APPROVED", "REJECTED"].contains(&s))
        {
            out.push(
                "status must be one of the following values: PENDING, APPROVED, REJECTED".into(),
            );
        }
        for (k, v) in [("startDate", &self.start_date), ("endDate", &self.end_date)] {
            if v.as_deref().is_some_and(|s| !is_iso8601(s)) {
                out.push(format!("{k} must be a valid ISO 8601 date string"));
            }
        }
        uuid(&mut out, "userUuid", &self.user_uuid);
        out
    }
}

fn list_select() -> String {
    let user = |alias: &str| {
        format!(
            "json_build_object('uuid', {alias}.uuid::text, 'name', {alias}.name, 'username', {alias}.username)"
        )
    };
    format!(
        r#"SELECT json_build_object(
              'uuid', t.uuid::text, 'note', t.note, 'reason', t.reason,
              'startAt', {start}, 'endAt', {end}, 'timezone', t.timezone,
              'offsetMinutes', t."offsetMinutes", 'manualTimeStatus', t."manualTimeStatus"::text,
              'manualTimeApprovalSource', t."manualTimeApprovalSource"::text,
              'requestedAt', {requested}, 'rejectedReason', t."rejectedReason",
              'approvedAt', {approved},
              'approvedBy', (SELECT {approver} FROM users ab WHERE ab.id = t."approvedById"),
              'tracking', json_build_object('user', (SELECT {owner} FROM users ou WHERE ou.id = tr."userId")),
              'project', json_build_object('uuid', p.uuid::text, 'name', p.name),
              'manualTimeAttachments', {ATTACHMENTS_SQL})
           FROM "trackingTimeline" t
           JOIN tracking tr ON tr.id = t."trackingId"
           JOIN project p ON p.id = t."projectId""#,
        start = ts(r#"t."startAt""#),
        end = ts(r#"t."endAt""#),
        requested = ts(r#"t."requestedAt""#),
        approved = ts(r#"t."approvedAt""#),
        approver = user("ab"),
        owner = user("ou"),
    )
}

async fn find_all_inner(state: &AppState, q: &ApprovalsQuery, user_id: i32) -> sqlx::Result<Value> {
    let empty = json!({ "success": true, "data": [], "total": 0 });

    // Tanggal: `new Date(startDate)` / `new Date(endDate + 'T23:59:59Z')`.
    let start = match q.start_date.as_deref() {
        Some(s) => match parse_js_date(s) {
            Some(d) => Some(d),
            None => return Ok(invalid_date()),
        },
        None => None,
    };
    let end = match q.end_date.as_deref() {
        Some(s) => match parse_js_date(&format!("{s}T23:59:59Z")) {
            Some(d) => Some(d),
            None => return Ok(invalid_date()),
        },
        None => None,
    };

    let mut owner_id: Option<i32> = None;
    if let Some(uuid) = q.user_uuid.as_deref() {
        owner_id = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid LIMIT 1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    }
    let mut project_filter: Option<i32> = None;
    if let Some(uuid) = q.project_uuid.as_deref() {
        project_filter = sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid LIMIT 1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    }

    let project_ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "projectId" FROM "projectManualTimeApprover" WHERE "userId" = $1 AND status = 1"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    if project_ids.is_empty() {
        return Ok(empty);
    }
    if let Some(p) = project_filter
        && !project_ids.contains(&p)
    {
        return Ok(empty);
    }

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE t."isOffline" = true AND t."manualTimeStatus" IS NOT NULL AND t.status IN (0, 1)"#);
        if let Some(s) = q.status.clone() {
            qb.push(r#" AND t."manualTimeStatus" = "#)
                .push_bind(s)
                .push(r#"::"ManualTimeStatus""#);
        }
        if let Some(d) = start {
            qb.push(r#" AND t."startAt" >= "#).push_bind(d);
        }
        if let Some(d) = end {
            qb.push(r#" AND t."startAt" <= "#).push_bind(d);
        }
        if let Some(o) = owner_id {
            qb.push(r#" AND tr."userId" = "#).push_bind(o);
        }
        match project_filter {
            Some(p) => {
                qb.push(r#" AND t."projectId" = "#).push_bind(p);
            }
            None => {
                qb.push(r#" AND t."projectId" = ANY("#)
                    .push_bind(project_ids.clone())
                    .push(")");
            }
        }
    };

    let mut data_q = QueryBuilder::<Postgres>::new(list_select());
    push_where(&mut data_q);
    data_q.push(r#" ORDER BY t."createdAt" DESC, t.id DESC"#);
    let data: Vec<Value> = data_q.build_query_scalar().fetch_all(&state.db).await?;

    let mut count_q = QueryBuilder::<Postgres>::new(
        r#"SELECT COUNT(*) FROM "trackingTimeline" t JOIN tracking tr ON tr.id = t."trackingId""#,
    );
    push_where(&mut count_q);
    let total: i64 = count_q.build_query_scalar().fetch_one(&state.db).await?;

    Ok(json!({ "success": true, "data": data, "total": total }))
}

fn invalid_date() -> Value {
    json!({
        "success": false,
        "message": "Invalid value for argument: Provided Date object is invalid. Expected Date.",
        "data": null,
    })
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:100 (`findAll`)
pub async fn find_all(state: &AppState, q: &ApprovalsQuery, user_id: i32) -> Value {
    match find_all_inner(state, q, user_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "manual-time-approvals-findAll");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

async fn assigned_projects_inner(state: &AppState, user_id: i32) -> sqlx::Result<Value> {
    let rows: Vec<Value> = sqlx::query_scalar(
        r#"SELECT json_build_object('uuid', p.uuid::text, 'name', p.name, 'code', p.code, 'status', p.status)
           FROM "projectManualTimeApprover" a JOIN project p ON p.id = a."projectId"
           WHERE a."userId" = $1 AND a.status = 1 ORDER BY a.id"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({ "success": true, "data": rows }))
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:330 (`getAssignedProjects`)
pub async fn assigned_projects(state: &AppState, user_id: i32) -> Value {
    match assigned_projects_inner(state, user_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "manual-time-approvals-getAssignedProjects");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

async fn my_pending_inner(state: &AppState, user_id: i32) -> sqlx::Result<Value> {
    let sql = format!(
        r#"SELECT json_build_object(
              'uuid', t.uuid::text, 'note', t.note, 'reason', t.reason,
              'startAt', {start}, 'endAt', {end}, 'timezone', t.timezone,
              'offsetMinutes', t."offsetMinutes", 'manualTimeStatus', t."manualTimeStatus"::text,
              'requestedAt', {requested},
              'project', json_build_object('uuid', p.uuid::text, 'name', p.name),
              'manualTimeAttachments', {ATTACHMENTS_SQL})
           FROM "trackingTimeline" t
           JOIN tracking tr ON tr.id = t."trackingId"
           JOIN project p ON p.id = t."projectId"
           WHERE t."isOffline" = true AND t."manualTimeStatus" = 'PENDING' AND t.status IN (0, 1)
             AND tr."userId" = $1
           ORDER BY t."createdAt" DESC, t.id DESC"#,
        start = ts(r#"t."startAt""#),
        end = ts(r#"t."endAt""#),
        requested = ts(r#"t."requestedAt""#),
    );
    let rows: Vec<Value> = sqlx::query_scalar(&sql)
        .bind(user_id)
        .fetch_all(&state.db)
        .await?;
    Ok(json!({ "success": true, "data": rows }))
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:364 (`findMyPending`)
pub async fn my_pending(state: &AppState, user_id: i32) -> Value {
    match my_pending_inner(state, user_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "manual-time-approvals-findMyPending");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

// ───────────────────────── email-action ─────────────────────────

#[derive(sqlx::FromRow)]
struct TokenRow {
    approver_id: i32,
    used_at: Option<JsDate>,
    expired_at: JsDate,
    timeline_id: i32,
    timeline_uuid: String,
    project_id: i32,
    manual_time_status: Option<String>,
    start_at: JsDate,
    end_at: JsDate,
    reason: Option<String>,
    project_name: String,
    owner_name: String,
    owner_username: String,
    owner_id: i32,
}

/// Kegagalan transaksi email: hasil approve (untuk membedakan pesan) atau lain-lain.
enum EmailTxError {
    Outcome(ApproveOutcome),
    RejectNotPending,
    Other,
}

async fn email_tx(
    state: &AppState,
    token: uuid::Uuid,
    action: &str,
    tl: &TokenRow,
) -> Result<Option<ApproveOutcome>, EmailTxError> {
    let mut tx = state.db.begin().await.map_err(|_| EmailTxError::Other)?;
    let marked = sqlx::query(
        r#"UPDATE "manualTimeApprovalToken" SET "usedAt" = $2 WHERE uuid = $1::uuid AND "usedAt" IS NULL"#,
    )
    .bind(token)
    .bind(now_naive())
    .execute(&mut *tx)
    .await
    .map_err(|_| EmailTxError::Other)?;
    if marked.rows_affected() == 0 {
        return Err(EmailTxError::Other); // TOKEN_ALREADY_USED
    }

    let mut outcome = None;
    if action == "approve" {
        let o = approve_in_tx(
            &mut tx,
            tl.timeline_id,
            Some(tl.approver_id),
            "MANUAL",
            None,
        )
        .await
        .map_err(|_| EmailTxError::Other)?;
        // Batalkan pemakaian token: approver masih bisa reject lewat link yang sama.
        if !matches!(o, ApproveOutcome::Approved { .. }) {
            return Err(EmailTxError::Outcome(o));
        }
        outcome = Some(o);
    } else {
        let ok = reject_in_tx(
            &mut tx,
            tl.timeline_id,
            tl.approver_id,
            "Rejected via email",
        )
        .await
        .map_err(|_| EmailTxError::Other)?;
        if !ok {
            return Err(EmailTxError::RejectNotPending);
        }
    }
    tx.commit().await.map_err(|_| EmailTxError::Other)?;
    Ok(outcome)
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:429 (`handleEmailAction`)
pub async fn handle_email_action(state: &AppState, token: &str, action: &str) -> ApiResult<String> {
    let host_fe = state
        .config
        .var("HOST_FE")
        .unwrap_or_else(|| "http://localhost:3001".to_owned());
    let base = format!("{host_fe}/manual-time-approvals?tab=approvals");
    let err = |msg: &str| format!("{base}&status=error&message={msg}");

    if action != "approve" && action != "reject" {
        return Ok(err("Invalid+action"));
    }
    // Kolom `@db.Uuid`: token yang ditolak Prisma membuatnya melempar (500 di Node).
    let token = mz_core::prisma::parse_uuid(token)
        .map_err(|e| ApiError::internal(anyhow::anyhow!("invalid uuid token: {e}")))?;

    let tl: Option<TokenRow> = sqlx::query_as(
        r#"SELECT k."approverId" AS approver_id, k."usedAt" AS used_at, k."expiredAt" AS expired_at,
                  t.id AS timeline_id, t.uuid::text AS timeline_uuid, t."projectId" AS project_id,
                  t."manualTimeStatus"::text AS manual_time_status, t."startAt" AS start_at,
                  t."endAt" AS end_at, t.reason, p.name AS project_name,
                  u.name AS owner_name, u.username AS owner_username, tr."userId" AS owner_id
           FROM "manualTimeApprovalToken" k
           JOIN "trackingTimeline" t ON t.id = k."trackingTimelineId"
           JOIN project p ON p.id = t."projectId"
           JOIN tracking tr ON tr.id = t."trackingId"
           JOIN users u ON u.id = tr."userId"
           WHERE k.uuid = $1::uuid"#,
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await?;
    let Some(tl) = tl else {
        return Ok(err("Invalid+or+expired+token"));
    };
    if tl.used_at.is_some() {
        return Ok(err("This+link+has+already+been+used"));
    }
    if now_naive() > tl.expired_at.0 {
        return Ok(err("This+link+has+expired"));
    }
    if tl.manual_time_status.as_deref() != Some("PENDING") {
        return Ok(err("This+request+has+already+been+processed"));
    }

    let outcome = match email_tx(state, token, action, &tl).await {
        Ok(o) => o,
        // Hanya `covered` yang dicatat: hasil `approved` di sini sudah di-rollback.
        Err(EmailTxError::Outcome(o @ ApproveOutcome::Covered { .. })) => {
            log_outcome(&tl.timeline_uuid, tl.approver_id, &o);
            return Ok(err(&encode_uri_component(FULLY_COVERED_MESSAGE)));
        }
        Err(EmailTxError::Outcome(ApproveOutcome::NotPending) | EmailTxError::RejectNotPending) => {
            return Ok(err("This+request+has+already+been+processed"));
        }
        Err(_) => return Ok(err("Failed+to+process+request")),
    };

    let is_approve = action == "approve";
    let mut mail_date = format_date(tl.start_at.0);
    let mut mail_duration = format_duration(tl.start_at.0, tl.end_at.0);
    if let Some(
        o @ ApproveOutcome::Approved {
            requested,
            approved_seconds,
            ..
        },
    ) = &outcome
    {
        log_outcome(&tl.timeline_uuid, tl.approver_id, o);
        recalc_burn_background(state, tl.project_id);
        // Node: manual-time-approvals.service.ts:599 (reject tidak pernah nudge).
        mz_overtime::nudge_for_timelines(
            state,
            Some(tl.owner_id),
            vec![Some(tl.project_id)],
            vec![mz_overtime::NudgeInterval::new(
                requested.start,
                requested.end,
            )],
        );
        mail_date = format_day_month_year(requested.start);
        mail_duration = format_seconds(*approved_seconds);
    }
    send_status_mail(
        state,
        &StatusMail {
            staff_email: &tl.owner_username,
            staff_name: &tl.owner_name,
            is_approved: is_approve,
            date: mail_date,
            duration: mail_duration,
            project_name: &tl.project_name,
            reason: tl.reason.as_deref(),
            rejected_reason: (!is_approve).then_some("Rejected via email"),
        },
    );

    let (status_label, message_label) = if is_approve {
        ("approved", "Manual+time+request+has+been+approved")
    } else {
        ("rejected", "Manual+time+request+has+been+rejected")
    };
    Ok(format!(
        "{base}&status={status_label}&message={message_label}"
    ))
}
// ───────────────────────── handler HTTP ─────────────────────────

/// Query `email-action` (`@Query('token')`, `@Query('action')`).
#[derive(Debug, Default, Deserialize)]
pub struct EmailActionQuery {
    pub token: Option<String>,
    pub action: Option<String>,
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:42 (`handleEmailAction`, 302)
pub async fn h_email_action(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<EmailActionQuery>,
) -> ApiResult<Response> {
    let url = handle_email_action(
        &state,
        q.token.as_deref().unwrap_or_default(),
        q.action.as_deref().unwrap_or_default(),
    )
    .await?;
    // `res.redirect(url)` Express.
    Ok(mz_core::redirect::redirect_302(&headers, &url))
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:58 (`findMyPending`)
pub async fn h_my_pending(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    Ok(Json(my_pending(&state, ctx.require_user_id()?).await))
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:68 (`getAssignedProjects`)
pub async fn h_assigned(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    Ok(Json(
        assigned_projects(&state, ctx.require_user_id()?).await,
    ))
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:81 (`findAll`)
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<ApprovalsQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(find_all(&state, &q, ctx.require_user_id()?).await))
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:93 (`approve`)
pub async fn h_approve(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(approve(&state, &uuid, ctx.require_user_id()?).await?))
}

/// Body `RejectManualTimeDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct RejectBody(pub Map<String, Value>);

impl Validate for RejectBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let v = self.0.get("rejectedReason");
        if v.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
            out.push("rejectedReason should not be empty".to_owned());
        }
        if v.and_then(Value::as_str).is_none() {
            out.push("rejectedReason must be a string".to_owned());
        }
        out
    }
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:106 (`reject`)
pub async fn h_reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<RejectBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let reason = defined(body.0.get("rejectedReason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(Json(
        reject(&state, &uuid, reason, ctx.require_user_id()?).await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};

    #[test]
    fn duration_text() {
        assert_eq!(format_seconds(0), "0 minute");
        assert_eq!(format_seconds(60), "1 minute");
        assert_eq!(format_seconds(120), "2 minutes");
        assert_eq!(format_seconds(3600), "1 hour");
        assert_eq!(format_seconds(7500), "2 hours 5 minutes");
        assert_eq!(format_seconds(3660), "1 hour 1 minute");
        assert_eq!(format_seconds(59), "0 minute");
    }

    #[test]
    fn query_validation() {
        let q = ApprovalsQuery {
            project_uuid: Some("x".into()),
            status: Some("NOPE".into()),
            start_date: Some("bad".into()),
            ..Default::default()
        };
        assert_eq!(
            q.validate(),
            vec![
                "projectUuid must be a UUID",
                "status must be one of the following values: PENDING, APPROVED, REJECTED",
                "startDate must be a valid ISO 8601 date string",
            ]
        );
        assert!(ApprovalsQuery::default().validate().is_empty());
    }

    #[test]
    fn reject_body_validation() {
        let b = RejectBody(Map::new());
        assert_eq!(
            b.validate(),
            vec![
                "rejectedReason should not be empty",
                "rejectedReason must be a string"
            ]
        );
    }

    #[test]
    fn redirect_is_302_with_location() {
        let r =
            mz_core::redirect::redirect_302(&axum::http::HeaderMap::new(), "http://x/y?a=b&c=d");
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(
            r.headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("http://x/y?a=b&c=d")
        );
    }
}
