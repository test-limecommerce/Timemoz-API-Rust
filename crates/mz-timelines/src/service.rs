//! `TrackingTimelineService` (src/tracking-timelines/tracking-timelines.service.ts):
//! create/update/remove offline (jalur desktop), findOne, updateNote,
//! bulkUpdateBillable, splitEntry, splitTime.

use std::collections::HashSet;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::time::JsDate;
use mz_core::{ApiError, AppState};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::access::{can_access_owner_data, precheck};
use crate::dto::{
    SplitEntryDto, SplitTimeDto, TimelineDto, bool_of, date_ms, str_of, truthy_str, uuid_of,
};
use crate::fail::{TResult, Thrown, retry_transient};
use crate::manual::NOT_ALLOWED_FOR_USER;
use crate::range::{
    MsRange, added_ranges, date_ms as day_ms, diff_trunc, iso, local_date, moment_tz, ms_of, ndt,
    overlaps_any, same_local_day, split_to_utc, timeline_range_ms,
};
use crate::spawn::{self, Push, TaskRef};
use crate::tx::{
    OtherTrim, Target, TrimEvent, after_trim, increment_total, lock_tracking, trim_approved_manual,
    trim_owner_manual_in_other_trackings,
};

const STATUS_ACTIVE: i32 = 1;
const ISSAME_TYPE_ERROR: &str = "Cannot read properties of undefined (reading 'isSame')";
const FORMAT_TYPE_ERROR: &str = "Cannot read properties of undefined (reading 'format')";
const CROSS_MIDNIGHT: &str = "Timeline cannot cross midnight. Please split the request.";

fn fail(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

fn not_allowed_for_user() -> Value {
    fail(NOT_ALLOWED_FOR_USER)
}

/// Angka JSON → Int32; Prisma memotong pecahan ke arah nol, di luar rentang gagal.
pub(crate) fn trunc_i32(n: &serde_json::Number) -> Option<i32> {
    let t = match n.as_i64() {
        Some(i) => i as f64,
        None => n.as_f64()?.trunc(),
    };
    (t.is_finite() && t >= f64::from(i32::MIN) && t <= f64::from(i32::MAX)).then_some(t as i32)
}

/// Kolom `Int` dari angka JSON (pecahan dipotong seperti Prisma).
fn int_of(v: Option<&Value>, field: &str) -> TResult<Option<i32>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => trunc_i32(n).map(Some).ok_or_else(|| {
            Thrown::msg(format!(
                "Argument `{field}`: Invalid value provided. Expected Int, provided Float."
            ))
        }),
        Some(_) => Err(Thrown::msg(format!(
            "Argument `{field}`: Invalid value provided."
        ))),
    }
}

/// `task.findFirst({ where: { uuid, source }, select: { id } })` (uuid tak sah → tidak ketemu).
async fn linked_task_id(state: &AppState, dto: &TimelineDto) -> sqlx::Result<Option<i32>> {
    let (uuid, source) = if let Some(u) = truthy_str(dto.clickup_task_uuid.as_ref()) {
        (u, "clickup")
    } else if let Some(u) = truthy_str(dto.jira_task_uuid.as_ref()) {
        (u, "jira")
    } else {
        return Ok(None);
    };
    let Some(uuid) = uuid_of(uuid) else {
        return Ok(None);
    };
    sqlx::query_scalar(r#"SELECT id FROM task WHERE uuid = $1 AND source = $2 LIMIT 1"#)
        .bind(uuid)
        .bind(source)
        .fetch_optional(&state.db)
        .await
}

// ===========================================================================
// createOffline

#[derive(Debug, Clone, sqlx::FromRow)]
struct TrackingRow {
    id: i32,
    user_id: i32,
    taken_date: NaiveDate,
    user_uuid: Option<Uuid>,
}

fn timeline_data(id: i32, uuid: Uuid, t: &TrackingRow) -> Value {
    json!({
        "id": id,
        "uuid": uuid,
        "userUuid": t.user_uuid,
        "takenDate": JsDate::from_millis(day_ms(t.taken_date)),
    })
}

struct CreateInput<'a> {
    dto: &'a TimelineDto,
    tracking: TrackingRow,
    project_id: i32,
    project_billable: bool,
    start_ms: i64,
    end_ms: i64,
    duration: i64,
    task_id: Option<i32>,
    offset: Option<i32>,
}

struct CreateOut {
    body: Value,
    created: Option<(i32, Uuid)>,
    success_id: Option<(i32, Uuid)>,
    events: Vec<TrimEvent>,
}

#[derive(sqlx::FromRow)]
struct ActiveRow {
    id: i32,
    uuid: Uuid,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    timezone: Option<String>,
    manual_time_status: Option<String>,
}

async fn create_tx(conn: &mut PgConnection, input: &CreateInput<'_>) -> TResult<CreateOut> {
    let t = &input.tracking;
    lock_tracking(conn, t.id).await?;
    let rows: Vec<ActiveRow> = sqlx::query_as(
        r#"SELECT id, uuid, "startAt" AS start_at, "endAt" AS end_at, timezone,
                  "manualTimeStatus"::text AS manual_time_status
           FROM "trackingTimeline" WHERE "trackingId" = $1 AND status = 1 ORDER BY id"#,
    )
    .bind(t.id)
    .fetch_all(&mut *conn)
    .await?;

    let dashboard = input.dto.is_dashboard();
    let mut approved_manual = Vec::new();
    for r in &rows {
        let range = timeline_range_ms(r.start_at, r.end_at, r.timezone.as_deref());
        let is_approved_manual = r.manual_time_status.as_deref() == Some("APPROVED");
        if !is_approved_manual && input.start_ms == range.start && input.end_ms == range.end {
            return Ok(CreateOut {
                body: json!({
                    "success": true,
                    "message": "Create tracking timeline success",
                    "data": timeline_data(r.id, r.uuid, t),
                }),
                created: None,
                success_id: Some((r.id, r.uuid)),
                events: Vec::new(),
            });
        }
        let overlapping = input.start_ms < range.end && input.end_ms > range.start;
        if overlapping && dashboard {
            return Err(Thrown::msg("Overlap detected"));
        } else if overlapping && is_approved_manual {
            approved_manual.push(r.id);
        } else if overlapping {
            return Ok(CreateOut {
                body: json!({
                    "success": true,
                    "message": "Tracking timeline already exists",
                    "data": timeline_data(r.id, r.uuid, t),
                }),
                created: None,
                success_id: Some((r.id, r.uuid)),
                events: Vec::new(),
            });
        }
    }

    let dto = input.dto;
    let note = truthy_str(dto.note.as_ref()).unwrap_or("-");
    let billable = match dto.billable.as_ref() {
        None => Some(input.project_billable),
        Some(v) => bool_of(Some(v)),
    };
    for (field, v) in [
        ("startAt", dto.start_at.as_ref()),
        ("endAt", dto.end_at.as_ref()),
    ] {
        if let Some(m) = crate::dto::prisma_datetime_error(field, v) {
            return Err(Thrown::Prisma(m));
        }
    }
    let (id, uuid): (i32, Uuid) = sqlx::query_as(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
             timezone, "offsetMinutes", "trackingId", "projectId", status, billable, "taskId")
           VALUES ($1, $2, $3, $4, $3, $4, $5, $6, $7, $8, 1, $9, $10)
           RETURNING id, uuid"#,
    )
    .bind(note)
    .bind(bool_of(dto.is_offline.as_ref()).unwrap_or(false))
    .bind(ndt(input.start_ms))
    .bind(ndt(input.end_ms))
    .bind(str_of(dto.timezone.as_ref()))
    .bind(input.offset)
    .bind(t.id)
    .bind(input.project_id)
    .bind(billable)
    .bind(input.task_id)
    .fetch_one(&mut *conn)
    .await?;

    let live = MsRange::new(input.start_ms, input.end_ms);
    let target = Target {
        id,
        project_id: input.project_id,
    };
    let (manual_seconds, mut events) =
        trim_approved_manual(conn, live, &approved_manual, target).await?;
    match trim_owner_manual_in_other_trackings(conn, t.id, t.user_id, live, target, dashboard)
        .await?
    {
        OtherTrim::OverlapDetected => return Err(Thrown::msg("Overlap detected")),
        OtherTrim::Events(more) => events.extend(more),
    }
    increment_total(conn, t.id, input.duration - manual_seconds).await?;
    Ok(CreateOut {
        body: json!({
            "success": true,
            "message": "Create tracking timeline success",
            "data": timeline_data(id, uuid, t),
        }),
        created: Some((id, uuid)),
        success_id: Some((id, uuid)),
        events,
    })
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:1853 `createOffline`.
pub async fn create_offline(state: &AppState, dto: &TimelineDto, caller: i32) -> Value {
    match create_inner(state, dto, caller).await {
        Ok(v) => v,
        Err(Thrown::Http(e)) => json!({ "success": false, "message": e.to_string(), "data": {} }),
        Err(t) => json!({ "success": false, "message": t.message(), "data": t.data() }),
    }
}

async fn create_inner(state: &AppState, dto: &TimelineDto, caller: i32) -> TResult<Value> {
    let (Some(start_ms), Some(end_ms)) =
        (date_ms(dto.start_at.as_ref()), date_ms(dto.end_at.as_ref()))
    else {
        // Invalid Date: semua perbandingan moment false → berakhir di cek tengah malam.
        return Ok(fail(CROSS_MIDNIGHT));
    };
    if end_ms < start_ms {
        return Ok(fail("Invalid endtime"));
    }
    if diff_trunc(end_ms, start_ms, 3_600_000) > 14 {
        return Ok(fail("Duration exceeds maximum limit of 14 hours"));
    }

    let project_uuid = str_of(dto.project_uuid.as_ref()).and_then(uuid_of);
    let tracking_uuid = str_of(dto.tracking_uuid.as_ref()).and_then(uuid_of);
    let (project, tracking) = tokio::try_join!(
        sqlx::query_as::<_, (i32, bool, Option<i32>)>(
            r#"SELECT p.id, p.billable,
                 (SELECT pc."companyId" FROM "projectCompany" pc WHERE pc."projectId" = p.id ORDER BY pc.id LIMIT 1)
               FROM project p WHERE p.uuid = $1 LIMIT 1"#
        )
        .bind(project_uuid)
        .fetch_optional(&state.db),
        sqlx::query_as::<_, TrackingRow>(
            r#"SELECT t.id, t."userId" AS user_id, t."takenDate" AS taken_date, u.uuid AS user_uuid
               FROM tracking t LEFT JOIN users u ON u.id = t."userId"
               WHERE t.uuid = $1 AND t.status = 1 LIMIT 1"#
        )
        .bind(tracking_uuid)
        .fetch_optional(&state.db),
    )?;
    let Some((project_id, project_billable, company_id)) = project else {
        return Ok(fail("Project UUID not found"));
    };
    let company_id = company_id.filter(|c| *c != 0);

    // `userId` dipakai lagi untuk ClickUp/Jira & log: pemanggil, kecuali ditimpa `userUuid`.
    let mut user_id = caller;
    let tz_name = str_of(dto.timezone.as_ref());
    let (tracking, pre) = match tracking {
        Some(t) => {
            let (allowed, pre) = tokio::try_join!(
                can_access_owner_data(&state.db, t.user_id, caller),
                precheck(&state.db, project_id, t.user_id, company_id),
            )?;
            if !allowed {
                return Ok(not_allowed_for_user());
            }
            if !pre.owner_project {
                return Ok(fail("Project UUID not found"));
            }
            (t, pre)
        }
        None => {
            if let Some(user_uuid) = truthy_str(dto.user_uuid.as_ref()) {
                let id: Option<i32> = match uuid_of(user_uuid) {
                    Some(u) => {
                        sqlx::query_scalar(r#"SELECT id FROM users WHERE uuid = $1 LIMIT 1"#)
                            .bind(u)
                            .fetch_optional(&state.db)
                            .await?
                    }
                    None => None,
                };
                let Some(id) = id else {
                    return Ok(fail("User UUID not found"));
                };
                user_id = id;
            }
            let (allowed, pre) = tokio::try_join!(
                can_access_owner_data(&state.db, user_id, caller),
                precheck(&state.db, project_id, user_id, company_id),
            )?;
            if !allowed {
                return Ok(not_allowed_for_user());
            }
            if !pre.owner_project {
                return Ok(fail("Project UUID not found"));
            }
            // `moment.tz(timezone)`: nama kosong → undefined → TypeError `.format`.
            let Some(zone) = moment_tz(tz_name) else {
                return Err(Thrown::msg(FORMAT_TYPE_ERROR));
            };
            let today = local_date(chrono::Utc::now().timestamp_millis(), zone);
            let start_date = crate::range::ndt(start_ms).date();
            let taken = if start_ms > day_ms(today) {
                start_date
            } else {
                today
            };
            let created: Result<TrackingRow, sqlx::Error> = sqlx::query_as(
                r#"INSERT INTO tracking ("isAbsent", "takenDate", "userId", status) VALUES (false, $1, $2, 1)
                   RETURNING id, "userId" AS user_id, "takenDate" AS taken_date,
                     (SELECT uuid FROM users WHERE id = $2) AS user_uuid"#,
            )
            .bind(taken)
            .bind(user_id)
            .fetch_one(&state.db)
            .await;
            let t = match created {
                Ok(t) => t,
                Err(e) if mz_core::db::is_unique_violation(&e) => {
                    return Err(Thrown::msg(
                        "Unique constraint failed on the fields: (`userId`,`takenDate`)",
                    ));
                }
                Err(e) => return Err(e.into()),
            };
            (t, pre)
        }
    };

    let duration = diff_trunc(end_ms, start_ms, 1000);
    let Some(zone) = moment_tz(tz_name) else {
        return Err(Thrown::msg(ISSAME_TYPE_ERROR));
    };
    if !same_local_day(start_ms, end_ms, zone) {
        return Ok(fail(CROSS_MIDNIGHT));
    }

    if company_id.is_some()
        && let Some(limit) = pre.limit()
    {
        let mut conn = state.db.acquire().await?;
        let remaining = crate::worklimit::remaining_for(
            &mut conn,
            tracking.user_id,
            limit,
            tracking.taken_date,
            None,
        )
        .await?;
        match crate::worklimit::exceeded(remaining, duration) {
            Some("daily") => return Ok(fail("Daily work time limit reached")),
            Some(_) => return Ok(fail("Weekly work time limit reached")),
            None => {}
        }
    }
    if let Some(reason) = pre.budget_block() {
        return Ok(fail(reason));
    }

    let input = CreateInput {
        dto,
        task_id: linked_task_id(state, dto).await?,
        offset: int_of(dto.offset_minutes.as_ref(), "offsetMinutes")?,
        tracking,
        project_id,
        project_billable,
        start_ms,
        end_ms,
        duration,
    };
    // Aman diulang: percobaan yang ternyata sudah commit dikenali cek start/end identik.
    let out = retry_transient("create-offline", || async {
        let mut tx = state.db.begin().await?;
        let out = create_tx(&mut tx, &input).await?;
        tx.commit().await?;
        Ok(out)
    })
    .await?;

    let live_uuid = out
        .success_id
        .map(|(_, u)| u.to_string())
        .unwrap_or_default();
    after_trim(state, &out.events, &live_uuid, user_id, project_id);
    let _ = out.created;
    if let Some((timeline_id, _)) = out.success_id {
        spawn::recalculate_burn(state, project_id);
        // Node: tracking-timelines.service.ts:2245 — baris tanpa timezone = jam dinding Jakarta.
        mz_overtime::nudge_for_timelines(
            state,
            Some(input.tracking.user_id),
            vec![Some(project_id)],
            vec![mz_overtime::NudgeInterval::of_input(
                start_ms, end_ms, tz_name,
            )],
        );
        let push = Push {
            user_id,
            timeline_id,
            start_ms,
            end_ms,
            note: str_of(dto.note.as_ref()).map(str::to_owned),
        };
        if let Some(u) = truthy_str(dto.clickup_task_uuid.as_ref()) {
            spawn::clickup(state, TaskRef::Uuid(u), push.clone(), None, |_| true);
        }
        if let Some(u) = truthy_str(dto.jira_task_uuid.as_ref()) {
            spawn::jira(state, TaskRef::Uuid(u), push, None, |_| true);
        }
    }
    Ok(out.body)
}

// ===========================================================================
// updateOffline

#[derive(Debug, Clone, sqlx::FromRow)]
struct UpdateTimelineRow {
    id: i32,
    uuid: Uuid,
    tracking_id: i32,
    project_id: i32,
    start_at: JsDate,
    end_at: JsDate,
    start_at_utc: Option<JsDate>,
    end_at_utc: Option<JsDate>,
    timezone: Option<String>,
    task_id: Option<i32>,
    clickup_time_entry_id: Option<String>,
    jira_worklog_id: Option<String>,
    t_id: i32,
    owner_id: i32,
    taken_date: NaiveDate,
    user_uuid: Option<Uuid>,
}

/// `data: timeline` pada "Update ignored due to overlap" (bentuk `select` Node).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IgnoredData<'a> {
    id: i32,
    uuid: Uuid,
    tracking_id: i32,
    project_id: i32,
    start_at: JsDate,
    end_at: JsDate,
    start_at_utc: Option<JsDate>,
    end_at_utc: Option<JsDate>,
    timezone: Option<&'a str>,
    task_id: Option<i32>,
    clickup_time_entry_id: Option<&'a str>,
    jira_worklog_id: Option<&'a str>,
    tracking: Value,
}

impl UpdateTimelineRow {
    fn ignored_json(&self) -> Value {
        serde_json::to_value(IgnoredData {
            id: self.id,
            uuid: self.uuid,
            tracking_id: self.tracking_id,
            project_id: self.project_id,
            start_at: self.start_at,
            end_at: self.end_at,
            start_at_utc: self.start_at_utc,
            end_at_utc: self.end_at_utc,
            timezone: self.timezone.as_deref(),
            task_id: self.task_id,
            clickup_time_entry_id: self.clickup_time_entry_id.as_deref(),
            jira_worklog_id: self.jira_worklog_id.as_deref(),
            tracking: json!({
                "id": self.t_id,
                "userId": self.owner_id,
                "takenDate": JsDate::from_millis(day_ms(self.taken_date)),
                "user": { "uuid": self.user_uuid },
            }),
        })
        .unwrap_or(Value::Null)
    }
}

#[derive(sqlx::FromRow)]
struct OtherRow {
    id: i32,
    uuid: Uuid,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    timezone: Option<String>,
    project_id: i32,
    manual_time_status: Option<String>,
}

struct UpdateInput<'a> {
    dto: &'a TimelineDto,
    timeline: &'a UpdateTimelineRow,
    project_id: i32,
    start_ms: i64,
    end_ms: i64,
    new_duration: i64,
    task_id: Option<i32>,
}

struct UpdateOut {
    body: Value,
    ignored: bool,
    success: bool,
    duration_diff: i64,
    healed: Vec<Uuid>,
    events: Vec<TrimEvent>,
}

/// Toleransi startAt kembaran hasil POST ganda.
const TWIN_START_TOLERANCE_MS: i64 = 1000;

/// Kembaran create-offline ganda: bukan manual time, project sama, mulai ≤ 1 dtk
/// dari baris ini, dan seluruhnya tercakup perpanjangan.
pub fn is_twin(
    manual_status: Option<&str>,
    same_project: bool,
    ex: MsRange,
    current_start: i64,
    new_end: i64,
) -> bool {
    manual_status.is_none()
        && same_project
        && (ex.start - current_start).abs() <= TWIN_START_TOLERANCE_MS
        && ex.end <= new_end
}

async fn update_tx(conn: &mut PgConnection, input: &UpdateInput<'_>) -> TResult<UpdateOut> {
    let tl = input.timeline;
    lock_tracking(conn, tl.tracking_id).await?;
    let current: Option<(NaiveDateTime, NaiveDateTime, Option<String>, i32)> = sqlx::query_as(
        r#"SELECT "startAt", "endAt", timezone, "projectId" FROM "trackingTimeline" WHERE id = $1 AND status = 1"#,
    )
    .bind(tl.id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((cur_start, cur_end, cur_tz, cur_project)) = current else {
        return Ok(UpdateOut {
            body: fail("tracking timeline uuid not found"),
            ignored: false,
            success: false,
            duration_diff: 0,
            healed: Vec::new(),
            events: Vec::new(),
        });
    };
    let others: Vec<OtherRow> = sqlx::query_as(
        r#"SELECT id, uuid, "startAt" AS start_at, "endAt" AS end_at, timezone, "projectId" AS project_id,
                  "manualTimeStatus"::text AS manual_time_status
           FROM "trackingTimeline" WHERE "trackingId" = $1 AND status = 1 AND id <> $2 ORDER BY id"#,
    )
    .bind(tl.tracking_id)
    .bind(tl.id)
    .fetch_all(&mut *conn)
    .await?;

    let cur = timeline_range_ms(cur_start, cur_end, cur_tz.as_deref());
    let new = MsRange::new(input.start_ms, input.end_ms);
    let added = added_ranges(cur, new);
    let dashboard = input.dto.is_dashboard();
    let mut twins: Vec<&OtherRow> = Vec::new();
    let mut approved_manual = Vec::new();
    for t in &others {
        let ex = timeline_range_ms(t.start_at, t.end_at, t.timezone.as_deref());
        if !(new.start < ex.end && new.end > ex.start) {
            continue;
        }
        if dashboard {
            return Err(Thrown::msg("Overlap detected"));
        }
        if is_twin(
            t.manual_time_status.as_deref(),
            t.project_id == cur_project,
            ex,
            cur.start,
            new.end,
        ) {
            twins.push(t);
            continue;
        }
        if t.manual_time_status.as_deref() == Some("APPROVED") {
            approved_manual.push(t.id);
            continue;
        }
        if !overlaps_any(&added, ex) {
            continue;
        }
        return Ok(UpdateOut {
            body: json!({
                "success": true,
                "message": "Update ignored due to overlap",
                "ignored": true,
                "data": tl.ignored_json(),
            }),
            ignored: true,
            success: true,
            duration_diff: 0,
            healed: Vec::new(),
            events: Vec::new(),
        });
    }

    let mut twin_seconds = 0;
    let mut healed = Vec::new();
    if !twins.is_empty() {
        let ids: Vec<i32> = twins.iter().map(|t| t.id).collect();
        sqlx::query(r#"UPDATE screenshot SET "trackingTimelineId" = $1 WHERE "trackingTimelineId" = ANY($2)"#)
            .bind(tl.id)
            .bind(&ids)
            .execute(&mut *conn)
            .await?;
        sqlx::query(r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = ANY($1)"#)
            .bind(&ids)
            .execute(&mut *conn)
            .await?;
        for t in &twins {
            twin_seconds += (ms_of(t.end_at) - ms_of(t.start_at)).div_euclid(1000);
            healed.push(t.uuid);
        }
    }

    let target = Target {
        id: tl.id,
        project_id: input.project_id,
    };
    let (manual_seconds, mut events) =
        trim_approved_manual(conn, new, &approved_manual, target).await?;
    match trim_owner_manual_in_other_trackings(
        conn,
        tl.tracking_id,
        tl.owner_id,
        new,
        target,
        dashboard,
    )
    .await?
    {
        OtherTrim::OverlapDetected => return Err(Thrown::msg("Overlap detected")),
        OtherTrim::Events(more) => events.extend(more),
    }

    let old_duration = diff_trunc(ms_of(cur_end), ms_of(cur_start), 1000);
    let duration_diff = input.new_duration - old_duration;

    let dto = input.dto;
    for (field, v) in [
        ("startAt", dto.start_at.as_ref()),
        ("endAt", dto.end_at.as_ref()),
    ] {
        if let Some(m) = crate::dto::prisma_datetime_error(field, v) {
            return Err(Thrown::Prisma(m));
        }
    }
    let start_set = str_of(dto.start_at.as_ref()).map(|_| ndt(input.start_ms));
    let end_set = str_of(dto.end_at.as_ref()).map(|_| ndt(input.end_ms));
    let billable_set = dto.billable.is_some();
    let updated_id: i32 = sqlx::query_scalar(
        r#"UPDATE "trackingTimeline" SET "projectId" = $2,
             note = COALESCE($3, note),
             "startAt" = COALESCE($4, "startAt"), "endAt" = COALESCE($5, "endAt"),
             "startAtUtc" = COALESCE($4, "startAtUtc"), "endAtUtc" = COALESCE($5, "endAtUtc"),
             timezone = COALESCE($6, timezone),
             billable = CASE WHEN $7 THEN $8 ELSE billable END,
             "taskId" = COALESCE($9, "taskId")
           WHERE id = $1 RETURNING id"#,
    )
    .bind(tl.id)
    .bind(input.project_id)
    .bind(str_of(dto.note.as_ref()))
    .bind(start_set)
    .bind(end_set)
    .bind(str_of(dto.timezone.as_ref()))
    .bind(billable_set)
    .bind(bool_of(dto.billable.as_ref()))
    .bind(input.task_id)
    .fetch_one(&mut *conn)
    .await?;
    increment_total(
        conn,
        tl.tracking_id,
        duration_diff - twin_seconds - manual_seconds,
    )
    .await?;

    Ok(UpdateOut {
        body: json!({
            "success": true,
            "message": "update tracking timeline success",
            "data": {
                "id": updated_id,
                "uuid": tl.uuid,
                "userUuid": tl.user_uuid,
                "takenDate": JsDate::from_millis(day_ms(tl.taken_date)),
            },
        }),
        ignored: false,
        success: true,
        duration_diff,
        healed,
        events,
    })
}

/// `moment.utc(x)`: `undefined` → sekarang, `null`/tak terbaca → Invalid Date (`None`).
fn moment_utc(v: Option<&Value>, now_ms: i64) -> Option<i64> {
    match v {
        None => Some(now_ms),
        Some(_) => date_ms(v),
    }
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:2785 `updateOffline`.
pub async fn update_offline(state: &AppState, uuid: Uuid, dto: &TimelineDto, caller: i32) -> Value {
    match update_inner(state, uuid, dto, caller).await {
        Ok(v) => v,
        Err(t) => fail(&t.message()),
    }
}

async fn update_inner(
    state: &AppState,
    uuid: Uuid,
    dto: &TimelineDto,
    caller: i32,
) -> TResult<Value> {
    let now = chrono::Utc::now().timestamp_millis();
    // `startAt`/`endAt` yang TIDAK dikirim = rentang tersimpan. Node memakai
    // `moment.utc(undefined)` (= sekarang): PATCH parsial menghitung durasi ≈0
    // sehingga `totalTakenInSecond` berkurang sebesar durasi lama (bug Node,
    // sengaja TIDAK ditiru; deviasi mz-timelines #15).
    let stored = if dto.start_at.is_none() || dto.end_at.is_none() {
        sqlx::query_as::<_, (NaiveDateTime, NaiveDateTime, Option<String>)>(
            r#"SELECT "startAt", "endAt", timezone FROM "trackingTimeline"
               WHERE uuid = $1 AND status = 1 LIMIT 1"#,
        )
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?
        .map(|(s, e, tz)| timeline_range_ms(s, e, tz.as_deref()))
    } else {
        None
    };
    let start = match (&dto.start_at, stored) {
        (None, Some(r)) => Some(r.start),
        _ => moment_utc(dto.start_at.as_ref(), now),
    };
    let end = match (&dto.end_at, stored) {
        (None, Some(r)) => Some(r.end),
        _ => moment_utc(dto.end_at.as_ref(), now),
    };
    if let (Some(s), Some(e)) = (start, end)
        && e < s
    {
        return Ok(json!({ "success": false, "message": "Invalid endtime" }));
    }
    let Some(zone) = moment_tz(str_of(dto.timezone.as_ref())) else {
        return Err(Thrown::msg(ISSAME_TYPE_ERROR));
    };
    let (Some(start_ms), Some(end_ms)) = (start, end) else {
        return Ok(fail(CROSS_MIDNIGHT));
    };
    if !same_local_day(start_ms, end_ms, zone) {
        return Ok(fail(CROSS_MIDNIGHT));
    }

    let timeline: Option<UpdateTimelineRow> = sqlx::query_as(
        r#"SELECT tt.id, tt.uuid, tt."trackingId" AS tracking_id, tt."projectId" AS project_id,
                  tt."startAt" AS start_at, tt."endAt" AS end_at, tt."startAtUtc" AS start_at_utc,
                  tt."endAtUtc" AS end_at_utc, tt.timezone, tt."taskId" AS task_id,
                  tt."clickupTimeEntryId" AS clickup_time_entry_id, tt."jiraWorklogId" AS jira_worklog_id,
                  t.id AS t_id, t."userId" AS owner_id, t."takenDate" AS taken_date, u.uuid AS user_uuid
           FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
           LEFT JOIN users u ON u.id = t."userId"
           WHERE tt.uuid = $1 AND tt.status = 1 LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(timeline) = timeline else {
        return Ok(fail("tracking timeline uuid not found"));
    };
    if !can_access_owner_data(&state.db, timeline.owner_id, caller).await? {
        return Ok(fail("You are not allowed to update this tracking timeline"));
    }

    // `where: { uuid: undefined }` di Prisma = tanpa filter (baris pertama apa saja).
    let tracking_uuid = str_of(dto.tracking_uuid.as_ref()).and_then(uuid_of);
    let project_uuid = str_of(dto.project_uuid.as_ref()).and_then(uuid_of);
    let owner_sql = crate::access::owner_project_sql("$2");
    let (tracking_found, project) = tokio::try_join!(
        sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS(SELECT 1 FROM tracking WHERE ($1::uuid IS NULL OR uuid = $1))"#
        )
        .bind(tracking_uuid)
        .fetch_one(&state.db),
        async {
            // `projectUuid` TIDAK dikirim = project tetap. Node: `where: { uuid:
            // undefined }` = project pertama milik owner → timeline pindah project
            // (bug Node, sengaja TIDAK ditiru; deviasi mz-timelines #15).
            if dto.project_uuid.is_none() {
                return Ok(Some(timeline.project_id));
            }
            sqlx::query_scalar::<_, i32>(&format!(
                r#"SELECT p.id FROM project p WHERE ($1::uuid IS NULL OR p.uuid = $1) AND {owner_sql}
                   ORDER BY p.id LIMIT 1"#
            ))
            .bind(project_uuid)
            .bind(timeline.owner_id)
            .fetch_optional(&state.db)
            .await
        },
    )?;
    if !tracking_found {
        return Ok(fail("Tracking UUID not found"));
    }
    let Some(project_id) = project else {
        return Ok(fail("Project UUID not found"));
    };

    let new_duration = diff_trunc(end_ms, start_ms, 1000);
    let task_id = linked_task_id(state, dto).await?;
    let input = UpdateInput {
        dto,
        timeline: &timeline,
        project_id,
        start_ms,
        end_ms,
        new_duration,
        task_id,
    };
    let out = retry_transient("update-offline", || async {
        let mut tx = state.db.begin().await?;
        let out = update_tx(&mut tx, &input).await?;
        tx.commit().await?;
        Ok(out)
    })
    .await?;
    if out.ignored {
        return Ok(out.body);
    }

    after_trim(state, &out.events, &uuid.to_string(), caller, project_id);
    if !out.healed.is_empty() {
        tracing::warn!(
            target: "app_event",
            event = "duplicate_tracking_timeline_deactivated",
            timeline_uuid = %uuid,
            healed = ?out.healed,
        );
    }
    if out.success {
        // Node: tracking-timelines.service.ts:3161 — interval lama + baru, project lama + baru.
        mz_overtime::nudge_for_timelines(
            state,
            Some(timeline.owner_id),
            vec![Some(timeline.project_id), Some(project_id)],
            vec![
                mz_overtime::NudgeInterval::of_row(
                    timeline.start_at.0,
                    timeline.end_at.0,
                    timeline.start_at_utc.map(|d| d.0),
                    timeline.end_at_utc.map(|d| d.0),
                    timeline.timezone.as_deref(),
                ),
                mz_overtime::NudgeInterval::of_input(
                    start_ms,
                    end_ms,
                    str_of(dto.timezone.as_ref()),
                ),
            ],
        );
        spawn::recalculate_burn(state, project_id);
        let push = Push {
            user_id: caller,
            timeline_id: timeline.id,
            start_ms,
            end_ms,
            note: str_of(dto.note.as_ref()).map(str::to_owned),
        };
        let diff = out.duration_diff;
        let gate = move |has_existing: bool| {
            if has_existing {
                diff != 0
            } else {
                new_duration >= 60
            }
        };
        let clickup_task = truthy_str(dto.clickup_task_uuid.as_ref())
            .map(TaskRef::Uuid)
            .or_else(|| timeline.task_id.filter(|i| *i != 0).map(TaskRef::Id));
        if let Some(task) = clickup_task {
            let existing = timeline
                .clickup_time_entry_id
                .clone()
                .filter(|s| !s.is_empty());
            spawn::clickup(state, task, push.clone(), existing, gate);
        }
        let jira_task = truthy_str(dto.jira_task_uuid.as_ref())
            .map(TaskRef::Uuid)
            .or_else(|| timeline.task_id.filter(|i| *i != 0).map(TaskRef::Id));
        if let Some(task) = jira_task {
            let existing = timeline.jira_worklog_id.clone().filter(|s| !s.is_empty());
            spawn::jira(state, task, push, existing, gate);
        }
    }
    Ok(out.body)
}

// ===========================================================================
// removeOffline

enum RemoveOutcome {
    AlreadyDeleted,
    Deleted {
        project_id: i32,
        interval: mz_overtime::NudgeInterval,
    },
}

#[derive(sqlx::FromRow)]
struct RemoveRow {
    id: i32,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    status: i32,
    project_id: i32,
    manual_time_status: Option<String>,
    start_at_utc: Option<NaiveDateTime>,
    end_at_utc: Option<NaiveDateTime>,
    timezone: Option<String>,
}

async fn remove_tx(
    conn: &mut PgConnection,
    uuid: Uuid,
    tracking_id: i32,
) -> TResult<RemoveOutcome> {
    lock_tracking(conn, tracking_id).await?;
    let row: Option<RemoveRow> = sqlx::query_as(
        r#"SELECT id, "startAt" AS start_at, "endAt" AS end_at, status, "projectId" AS project_id,
                  "manualTimeStatus"::text AS manual_time_status,
                  "startAtUtc" AS start_at_utc, "endAtUtc" AS end_at_utc, timezone
           FROM "trackingTimeline" WHERE uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(RemoveOutcome::AlreadyDeleted);
    };
    let approved = row.manual_time_status.as_deref() == Some("APPROVED");
    if approved && row.status != STATUS_ACTIVE {
        return Ok(RemoveOutcome::AlreadyDeleted);
    }
    let seconds = (ms_of(row.end_at) - ms_of(row.start_at)).div_euclid(1000);
    let interval = mz_overtime::NudgeInterval::of_row(
        row.start_at,
        row.end_at,
        row.start_at_utc,
        row.end_at_utc,
        row.timezone.as_deref(),
    );
    if row.manual_time_status.is_some() {
        if approved {
            increment_total(conn, tracking_id, -seconds).await?;
        }
        sqlx::query(r#"DELETE FROM "trackingTimeline" WHERE id = $1"#)
            .bind(row.id)
            .execute(&mut *conn)
            .await?;
        return Ok(RemoveOutcome::Deleted {
            project_id: row.project_id,
            interval,
        });
    }
    if row.status != STATUS_ACTIVE {
        return Ok(RemoveOutcome::AlreadyDeleted);
    }
    sqlx::query(r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = $1"#)
        .bind(row.id)
        .execute(&mut *conn)
        .await?;
    increment_total(conn, tracking_id, -seconds).await?;
    Ok(RemoveOutcome::Deleted {
        project_id: row.project_id,
        interval,
    })
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:3352 `removeOffline`.
pub async fn remove_offline(state: &AppState, uuid: Uuid, caller: i32) -> Value {
    match remove_inner(state, uuid, caller).await {
        Ok(v) => v,
        Err(t) => {
            tracing::error!(error = %t.message(), %uuid, "tracking-timeline remove failed");
            fail("delete tracking timeline failed")
        }
    }
}

async fn remove_inner(state: &AppState, uuid: Uuid, caller: i32) -> TResult<Value> {
    let initial: Option<(i32, Option<i32>)> = sqlx::query_as(
        r#"SELECT tt."trackingId", t."userId" FROM "trackingTimeline" tt
           LEFT JOIN tracking t ON t.id = tt."trackingId" WHERE tt.uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((tracking_id, owner)) = initial else {
        return Ok(fail("Tracking timeline UUID not found"));
    };
    if let Some(owner) = owner
        && !can_access_owner_data(&state.db, owner, caller).await?
    {
        return Ok(fail("You are not allowed to delete this tracking timeline"));
    }
    let outcome = retry_transient("remove-offline", || async {
        let mut tx = state.db.begin().await?;
        let out = remove_tx(&mut tx, uuid, tracking_id).await?;
        tx.commit().await?;
        Ok(out)
    })
    .await?;
    if let RemoveOutcome::Deleted {
        project_id,
        interval,
    } = outcome
    {
        // Node: tracking-timelines.service.ts:3474.
        mz_overtime::nudge_for_timelines(state, owner, vec![Some(project_id)], vec![interval]);
        if project_id != 0 {
            spawn::recalculate_burn(state, project_id);
        }
    }
    let tracking: Option<(NaiveDate, Option<Uuid>)> = sqlx::query_as(
        r#"SELECT t."takenDate", u.uuid FROM tracking t LEFT JOIN users u ON u.id = t."userId" WHERE t.id = $1"#,
    )
    .bind(tracking_id)
    .fetch_optional(&state.db)
    .await?;
    // `tracking.user.uuid` pada null → TypeError → "delete tracking timeline failed".
    let Some((taken, Some(user_uuid))) = tracking else {
        return Err(Thrown::msg(
            "Cannot read properties of null (reading 'user')",
        ));
    };
    Ok(json!({
        "success": true,
        "message": "delete tracking timeline success",
        "data": {
            "isDeleted": true,
            "userUuid": user_uuid,
            "takenDate": taken.format("%Y-%m-%d").to_string(),
        },
    }))
}

// ===========================================================================
// findOne

#[derive(sqlx::FromRow)]
struct FindRow {
    id: i32,
    uuid: Uuid,
    note: String,
    is_offline: bool,
    start_at: JsDate,
    end_at: JsDate,
    created_at: JsDate,
    updated_at: Option<JsDate>,
    status: i32,
    billable: Option<bool>,
    manual_time_status: Option<String>,
    timezone: Option<String>,
    offset_minutes: Option<i32>,
    owner_id: Option<i32>,
    p_id: Option<i32>,
    p_uuid: Option<Uuid>,
    p_code: Option<String>,
    p_name: Option<String>,
}

#[derive(sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenshotItem {
    id: i32,
    uuid: Uuid,
    is_compressed: bool,
    is_blur: bool,
    taken_at: JsDate,
    created_at: JsDate,
    updated_at: Option<JsDate>,
    status: i32,
    #[sqlx(skip)]
    screenshot_url: String,
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:2372 `findOne`.
pub async fn find_one(
    state: &AppState,
    uuid: Uuid,
    raw_uuid: &str,
    caller: i32,
) -> Result<Value, ApiError> {
    let row: Option<FindRow> = sqlx::query_as(
        r#"SELECT tt.id, tt.uuid, tt.note, tt."isOffline" AS is_offline, tt."startAt" AS start_at,
                  tt."endAt" AS end_at, tt."createdAt" AS created_at, tt."updatedAt" AS updated_at,
                  tt.status, tt.billable, tt."manualTimeStatus"::text AS manual_time_status,
                  tt.timezone, tt."offsetMinutes" AS offset_minutes, t."userId" AS owner_id,
                  p.id AS p_id, p.uuid AS p_uuid, p.code AS p_code, p.name AS p_name
           FROM "trackingTimeline" tt
           LEFT JOIN tracking t ON t.id = tt."trackingId"
           LEFT JOIN project p ON p.id = tt."projectId"
           WHERE tt.uuid = $1 AND tt.status = 1 LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(fail(&format!(
            "Data tracking timeline by uuid {raw_uuid} not found"
        )));
    };
    let access = async {
        match row.owner_id {
            Some(owner) => can_access_owner_data(&state.db, owner, caller).await,
            None => Ok(true),
        }
    };
    let shots = sqlx::query_as::<_, ScreenshotItem>(
        r#"SELECT id, uuid, "isCompressed" AS is_compressed, "isBlur" AS is_blur, "takenAt" AS taken_at,
                  "createdAt" AS created_at, "updatedAt" AS updated_at, status
           FROM screenshot WHERE "trackingTimelineId" = $1 ORDER BY id"#,
    )
    .bind(row.id)
    .fetch_all(&state.db);
    let (allowed, mut shots) = tokio::try_join!(access, shots)?;
    if !allowed {
        return Ok(fail("You are not allowed to access this tracking timeline"));
    }
    for s in &mut shots {
        s.screenshot_url = format!("/screenshots/{}/image?variant=display", s.uuid);
    }
    let project = match (row.p_id, row.p_uuid) {
        (Some(id), Some(u)) => {
            json!({ "id": id, "uuid": u, "code": row.p_code, "name": row.p_name })
        }
        _ => Value::Null,
    };
    Ok(json!({
        "success": true,
        "message": format!("Success get data tracking timeline by uuid {raw_uuid}"),
        "data": {
            "id": row.id,
            "uuid": row.uuid,
            "note": row.note,
            "isOffline": row.is_offline,
            "startAt": row.start_at,
            "endAt": row.end_at,
            "createdAt": row.created_at,
            "updatedAt": row.updated_at,
            "status": row.status,
            "billable": row.billable,
            "manualTimeStatus": row.manual_time_status,
            "timezone": row.timezone,
            "offsetMinutes": row.offset_minutes,
            "project": project,
            "screenshot": shots,
        },
    }))
}

// ===========================================================================
// updateNote / bulkUpdateBillable

/// `tracking.user.userCompany.some({ companyId })` (alias timeline `tt`, param `$n`).
fn caller_company_sql(param: &str) -> String {
    format!(
        r#"EXISTS(SELECT 1 FROM tracking ct JOIN "userCompany" cuc ON cuc."userId" = ct."userId"
             WHERE ct.id = tt."trackingId" AND cuc."companyId" = {param})"#
    )
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:3728 `updateNote`.
pub async fn update_note(
    state: &AppState,
    uuid: Uuid,
    raw_uuid: &str,
    note: &str,
    caller: i32,
) -> Result<Value, ApiError> {
    let company = mz_core::company::find_company_id(&state.db, caller).await?;
    let count = match company {
        Some(cid) => sqlx::query(&format!(
            r#"UPDATE "trackingTimeline" tt SET note = $2, "updatedAt" = $3
               WHERE tt.uuid = $1 AND {}"#,
            caller_company_sql("$4")
        ))
        .bind(uuid)
        .bind(note)
        .bind(mz_core::time::now_naive())
        .bind(cid)
        .execute(&state.db)
        .await?
        .rows_affected(),
        None => 0,
    };
    if count == 0 {
        return Err(ApiError::not_found(format!(
            "Tracking timeline dengan uuid {raw_uuid} tidak ditemukan"
        )));
    }
    Ok(json!({ "success": true, "message": "Note berhasil diupdate", "data": null }))
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:3754 `bulkUpdateBillable`.
pub async fn bulk_update_billable(
    state: &AppState,
    uuids: &[String],
    billable: bool,
    caller: i32,
) -> Result<Value, ApiError> {
    match bulk_inner(state, uuids, billable, caller).await {
        Ok(v) => Ok(v),
        Err(Thrown::Http(e)) => Err(e),
        Err(t) => Ok(fail(&t.message())),
    }
}

async fn bulk_inner(
    state: &AppState,
    uuids: &[String],
    billable: bool,
    caller: i32,
) -> TResult<Value> {
    if uuids.is_empty() {
        return Ok(fail("No UUIDs provided"));
    }
    let mut seen = HashSet::new();
    let unique: Vec<&String> = uuids.iter().filter(|u| seen.insert(u.as_str())).collect();
    let Some(cid) = mz_core::company::find_company_id(&state.db, caller).await? else {
        return Err(ApiError::not_found("Tracking timeline not found").into());
    };
    let parsed: Vec<Uuid> = unique.iter().filter_map(|u| uuid_of(u)).collect();
    let mut tx = state.db.begin().await?;
    let count = sqlx::query(&format!(
        r#"UPDATE "trackingTimeline" tt SET billable = $2, "updatedAt" = $3
           WHERE tt.uuid = ANY($1) AND tt.status = 1 AND {}"#,
        caller_company_sql("$4")
    ))
    .bind(&parsed)
    .bind(billable)
    .bind(mz_core::time::now_naive())
    .bind(cid)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if usize::try_from(count).ok() != Some(unique.len()) {
        // Semua-atau-tidak: rollback (drop tx) lalu 404.
        return Err(ApiError::not_found("Tracking timeline not found").into());
    }
    tx.commit().await?;
    Ok(json!({
        "success": true,
        "message": format!("Successfully updated billable status for {count} entries"),
        "data": { "updatedCount": count },
    }))
}

// ===========================================================================
// splitEntry / splitTime

#[derive(Debug, Clone, sqlx::FromRow)]
struct SplitOriginal {
    id: i32,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    start_at_utc: Option<NaiveDateTime>,
    end_at_utc: Option<NaiveDateTime>,
    timezone: Option<String>,
    offset_minutes: Option<i32>,
    is_offline: bool,
    note: String,
    billable: Option<bool>,
    tracking_id: i32,
    project_id: i32,
    manual_time_status: Option<String>,
    manual_time_approval_source: Option<String>,
    approved_by_id: Option<i32>,
    approved_at: Option<NaiveDateTime>,
    owner_id: Option<i32>,
}

/// `TrackingTimelineService.rowInterval(original)` untuk nudge lembur.
fn original_interval(o: &SplitOriginal) -> mz_overtime::NudgeInterval {
    mz_overtime::NudgeInterval::of_row(
        o.start_at,
        o.end_at,
        o.start_at_utc,
        o.end_at_utc,
        o.timezone.as_deref(),
    )
}

async fn load_split_original(state: &AppState, uuid: Uuid) -> sqlx::Result<Option<SplitOriginal>> {
    sqlx::query_as(
        r#"SELECT tt.id, tt."startAt" AS start_at, tt."endAt" AS end_at, tt."startAtUtc" AS start_at_utc,
                  tt."endAtUtc" AS end_at_utc, tt.timezone, tt."offsetMinutes" AS offset_minutes,
                  tt."isOffline" AS is_offline, tt.note, tt.billable, tt."trackingId" AS tracking_id,
                  tt."projectId" AS project_id, tt."manualTimeStatus"::text AS manual_time_status,
                  tt."manualTimeApprovalSource"::text AS manual_time_approval_source,
                  tt."approvedById" AS approved_by_id, tt."approvedAt" AS approved_at, t."userId" AS owner_id
           FROM "trackingTimeline" tt LEFT JOIN tracking t ON t.id = tt."trackingId"
           WHERE tt.uuid = $1 AND tt.status = 1 LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await
}

/// `lockEntryForSplit`: lock, pastikan entri masih aktif dengan rentang sama,
/// kembalikan screenshot (id, takenAt ms) yang tidak DELETED.
async fn lock_entry_for_split(
    conn: &mut PgConnection,
    o: &SplitOriginal,
) -> TResult<Vec<(i32, i64)>> {
    lock_tracking(conn, o.tracking_id).await?;
    let current: Option<(i32, NaiveDateTime, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT status, "startAt", "endAt" FROM "trackingTimeline" WHERE id = $1"#,
    )
    .bind(o.id)
    .fetch_optional(&mut *conn)
    .await?;
    let unchanged = current.is_some_and(|(status, s, e)| {
        status == STATUS_ACTIVE && ms_of(s) == ms_of(o.start_at) && ms_of(e) == ms_of(o.end_at)
    });
    if !unchanged {
        return Err(Thrown::msg(
            "Tracking timeline changed while splitting, please reload and try again",
        ));
    }
    let shots: Vec<(i32, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT id, "takenAt" FROM screenshot WHERE "trackingTimelineId" = $1 AND status NOT IN (0)"#,
    )
    .bind(o.id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(shots.into_iter().map(|(id, t)| (id, ms_of(t))).collect())
}

/// `moveScreenshots`: satu UPDATE untuk semua pasangan (screenshot → segmen).
async fn move_screenshots(conn: &mut PgConnection, pairs: &[(i32, i32)]) -> sqlx::Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = pairs.iter().map(|p| p.0).collect();
    let segs: Vec<i32> = pairs.iter().map(|p| p.1).collect();
    sqlx::query(
        r#"UPDATE screenshot s SET "trackingTimelineId" = x.seg
           FROM UNNEST($1::int[], $2::int[]) AS x(id, seg) WHERE s.id = x.id"#,
    )
    .bind(&ids)
    .bind(&segs)
    .execute(conn)
    .await?;
    Ok(())
}

/// Segmen yang akan dibuat (kolom lokal & UTC terpisah, ms).
#[derive(Debug, Clone, PartialEq)]
pub struct NewSegment {
    pub start_at: i64,
    pub end_at: i64,
    pub start_utc: i64,
    pub end_utc: i64,
    pub billable: Option<bool>,
    pub project_id: i32,
    pub note: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CreatedSegment {
    id: i32,
    uuid: Uuid,
    start_at: JsDate,
    end_at: JsDate,
    billable: Option<bool>,
}

async fn insert_segment(
    conn: &mut PgConnection,
    o: &SplitOriginal,
    s: &NewSegment,
) -> sqlx::Result<CreatedSegment> {
    sqlx::query_as(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "manualTimeStatus", "manualTimeApprovalSource",
             "approvedById", "approvedAt", "startAt", "endAt", "startAtUtc", "endAtUtc", timezone,
             "offsetMinutes", "trackingId", "projectId", status, billable)
           VALUES ($1, $2, $3::"ManualTimeStatus", $4::"ManualTimeApprovalSource", $5, $6, $7, $8, $9, $10,
             $11, $12, $13, $14, 1, $15)
           RETURNING id, uuid, "startAt" AS start_at, "endAt" AS end_at, billable"#,
    )
    .bind(s.note.as_deref().unwrap_or(&o.note))
    .bind(o.is_offline)
    .bind(o.manual_time_status.as_deref())
    .bind(o.manual_time_approval_source.as_deref())
    .bind(o.approved_by_id)
    .bind(o.approved_at)
    .bind(ndt(s.start_at))
    .bind(ndt(s.end_at))
    .bind(ndt(s.start_utc))
    .bind(ndt(s.end_utc))
    .bind(o.timezone.as_deref())
    .bind(o.offset_minutes)
    .bind(o.tracking_id)
    .bind(s.project_id)
    .bind(s.billable)
    .fetch_one(conn)
    .await
}

/// Penempatan screenshot ke segmen: `[start, end)`; `last_inclusive` = segmen
/// terakhir memakai `<= end` (splitTime). Tidak cocok → segmen terakhir.
pub fn assign_segment(taken_ms: i64, segs: &[(i64, i64)], last_inclusive: bool) -> usize {
    let last = segs.len().saturating_sub(1);
    segs.iter()
        .enumerate()
        .position(|(i, (s, e))| {
            taken_ms >= *s && (taken_ms < *e || (last_inclusive && i == last && taken_ms <= *e))
        })
        .unwrap_or(last)
}

/// Rencana segmen `splitEntry` (STEP 3–5). `Err` = pesan gagal.
#[allow(clippy::too_many_arguments)]
pub fn plan_carve(
    o_start_local: i64,
    o_end_local: i64,
    o_start_utc: i64,
    o_end_utc: i64,
    carve_start: i64,
    carve_end: i64,
    parent_billable: Option<bool>,
    carved_billable: Option<bool>,
    project_id: i32,
) -> Result<Vec<NewSegment>, &'static str> {
    let eff_start = carve_start.max(o_start_utc);
    let eff_end = carve_end.min(o_end_utc);
    if eff_end <= eff_start {
        return Err("Carve-out range does not overlap with the entry or has zero duration");
    }
    let seg = |start_at, start_utc, end_at, end_utc, billable| NewSegment {
        start_at,
        end_at,
        start_utc,
        end_utc,
        billable,
        project_id,
        note: None,
    };
    let mut out = Vec::new();
    if eff_start > o_start_utc {
        out.push(seg(
            o_start_local,
            o_start_utc,
            eff_start,
            eff_start,
            parent_billable,
        ));
    }
    out.push(seg(eff_start, eff_start, eff_end, eff_end, carved_billable));
    if eff_end < o_end_utc {
        out.push(seg(
            eff_end,
            eff_end,
            o_end_local,
            o_end_utc,
            parent_billable,
        ));
    }
    Ok(out)
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:3819 `splitEntry`.
pub async fn split_entry(state: &AppState, uuid: Uuid, dto: &SplitEntryDto, caller: i32) -> Value {
    match split_entry_inner(state, uuid, dto, caller).await {
        Ok(v) => v,
        Err(t) => fail(&t.message()),
    }
}

async fn split_entry_inner(
    state: &AppState,
    uuid: Uuid,
    dto: &SplitEntryDto,
    caller: i32,
) -> TResult<Value> {
    let original = load_split_original(state, uuid).await?;
    let allowed = match original.as_ref().and_then(|o| o.owner_id) {
        Some(owner) => can_access_owner_data(&state.db, owner, caller).await?,
        None => false,
    };
    let Some(o) = original.filter(|_| allowed) else {
        return Ok(fail("Tracking timeline not found"));
    };
    if o.manual_time_status.as_deref() == Some("PENDING") {
        return Ok(fail(
            "Cannot split a manual time entry that is still pending approval",
        ));
    }
    let o_start_utc = split_to_utc(o.start_at, o.start_at_utc, o.offset_minutes);
    let o_end_utc = split_to_utc(o.end_at, o.end_at_utc, o.offset_minutes);
    let (Some(carve_start), Some(carve_end)) = (
        date_ms(dto.carve_start_at.as_ref()),
        date_ms(dto.carve_end_at.as_ref()),
    ) else {
        return Ok(fail(
            "Carve-out range does not overlap with the entry or has zero duration",
        ));
    };
    let carved_billable = match dto.carved_billable.as_ref() {
        None => o.billable,
        Some(v) => bool_of(Some(v)),
    };
    let segments = match plan_carve(
        ms_of(o.start_at),
        ms_of(o.end_at),
        o_start_utc,
        o_end_utc,
        carve_start,
        carve_end,
        o.billable,
        carved_billable,
        o.project_id,
    ) {
        Ok(s) => s,
        Err(m) => return Ok(fail(m)),
    };

    let created = retry_transient("split-entry", || async {
        let mut tx = state.db.begin().await?;
        let shots = lock_entry_for_split(&mut tx, &o).await?;
        let original_duration = diff_trunc(o_end_utc, o_start_utc, 1000);
        let mut created = Vec::with_capacity(segments.len());
        for seg in &segments {
            created.push(insert_segment(&mut tx, &o, seg).await?);
        }
        let bounds: Vec<(i64, i64)> = created
            .iter()
            .map(|c| (c.start_at.timestamp_millis(), c.end_at.timestamp_millis()))
            .collect();
        let pairs: Vec<(i32, i32)> = shots
            .iter()
            .filter_map(|(id, taken)| {
                created
                    .get(assign_segment(*taken, &bounds, false))
                    .map(|c| (*id, c.id))
            })
            .collect();
        move_screenshots(&mut tx, &pairs).await?;
        sqlx::query(r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = $1"#)
            .bind(o.id)
            .execute(&mut *tx)
            .await?;
        let new_total: i64 = segments
            .iter()
            .map(|s| diff_trunc(s.end_utc, s.start_utc, 1000))
            .sum();
        let diff = new_total - original_duration;
        if diff != 0 {
            increment_total(&mut tx, o.tracking_id, diff).await?;
        }
        tx.commit().await?;
        Ok(created)
    })
    .await?;

    // Node: tracking-timelines.service.ts:4066 — interval asli + tiap segmen (UTC).
    let mut intervals = vec![original_interval(&o)];
    intervals.extend(
        segments
            .iter()
            .map(|s| mz_overtime::NudgeInterval::new(s.start_utc, s.end_utc)),
    );
    mz_overtime::nudge_for_timelines(state, o.owner_id, vec![Some(o.project_id)], intervals);

    Ok(json!({
        "success": true,
        "message": format!("Entry split into {} segments", created.len()),
        "data": created.iter().map(|c| json!({
            "id": c.id, "uuid": c.uuid, "startAt": c.start_at, "endAt": c.end_at, "billable": c.billable,
        })).collect::<Vec<_>>(),
    }))
}

/// Validasi & snap segmen `splitTime` (STEP 4). `Err` = pesan gagal.
pub fn plan_split_time(
    segments: &mut [crate::dto::Segment],
    orig_start_utc: i64,
    orig_end_utc: i64,
) -> Result<(), String> {
    const BOUNDARY_TOLERANCE_MS: i64 = 60_000;
    segments.sort_by_key(|s| s.start_ms);
    for (i, s) in segments.iter().enumerate() {
        if s.end_ms <= s.start_ms {
            return Err(format!("Segment {}: endAt must be after startAt", i + 1));
        }
    }
    let n = segments.len();
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        return Err("Segment 1: endAt must be after startAt".into());
    };
    if (first.start_ms - orig_start_utc).abs() > BOUNDARY_TOLERANCE_MS {
        return Err(format!(
            "First segment must start at the original entry's start time ({}), tolerance 1 minute",
            iso(orig_start_utc)
        ));
    }
    if (last.end_ms - orig_end_utc).abs() > BOUNDARY_TOLERANCE_MS {
        return Err(format!(
            "Last segment must end at the original entry's end time ({}), tolerance 1 minute",
            iso(orig_end_utc)
        ));
    }
    if let Some(f) = segments.first_mut() {
        f.start_ms = orig_start_utc;
    }
    if let Some(l) = segments.last_mut() {
        l.end_ms = orig_end_utc;
    }
    for idx in [0, n - 1] {
        if let Some(s) = segments.get(idx)
            && s.end_ms <= s.start_ms
        {
            return Err(format!(
                "Segment {} has zero or negative duration after aligning to the original entry's boundaries ({} \u{2014} {})",
                idx + 1,
                iso(orig_start_utc),
                iso(orig_end_utc)
            ));
        }
    }
    for i in 0..n.saturating_sub(1) {
        let (Some(a), Some(b)) = (segments.get(i), segments.get(i + 1)) else {
            continue;
        };
        if diff_trunc(b.start_ms, a.end_ms, 1000).abs() > 1 {
            return Err(format!(
                "Gap detected between segment {} (ends {}) and segment {} (starts {})",
                i + 1,
                iso(a.end_ms),
                i + 2,
                iso(b.start_ms)
            ));
        }
    }
    Ok(())
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:4094 `splitTime`.
pub async fn split_time(state: &AppState, uuid: Uuid, dto: &SplitTimeDto, caller: i32) -> Value {
    match split_time_inner(state, uuid, dto, caller).await {
        Ok(v) => v,
        Err(t) => fail(&t.message()),
    }
}

async fn split_time_inner(
    state: &AppState,
    uuid: Uuid,
    dto: &SplitTimeDto,
    caller: i32,
) -> TResult<Value> {
    let Some(o) = load_split_original(state, uuid).await? else {
        return Ok(fail("Tracking timeline not found"));
    };
    let allowed = match o.owner_id {
        Some(owner) => can_access_owner_data(&state.db, owner, caller).await?,
        None => false,
    };
    if !allowed {
        return Ok(fail("Tracking timeline not found"));
    }
    if o.manual_time_status.as_deref() == Some("PENDING") {
        return Ok(fail(
            "Cannot split a manual time entry that is still pending approval",
        ));
    }
    let o_start_utc = split_to_utc(o.start_at, o.start_at_utc, o.offset_minutes);
    let o_end_utc = split_to_utc(o.end_at, o.end_at_utc, o.offset_minutes);
    let mut segments = dto.segments();
    if let Err(m) = plan_split_time(&mut segments, o_start_utc, o_end_utc) {
        return Ok(fail(&m));
    }

    let mut unique: Vec<&str> = Vec::new();
    for s in &segments {
        if !unique.contains(&s.project_uuid.as_str()) {
            unique.push(&s.project_uuid);
        }
    }
    let parsed: Vec<Uuid> = unique.iter().filter_map(|u| uuid_of(u)).collect();
    let owner_sql = crate::access::owner_project_sql("$2");
    let projects: Vec<(i32, Uuid, bool)> = sqlx::query_as(&format!(
        r#"SELECT p.id, p.uuid, p.billable FROM project p
           WHERE p.uuid = ANY($1) AND p.status = 1 AND {owner_sql}"#
    ))
    .bind(&parsed)
    .bind(o.owner_id)
    .fetch_all(&state.db)
    .await?;
    let find = |u: &str| uuid_of(u).and_then(|u| projects.iter().find(|p| p.1 == u).copied());
    for u in &unique {
        if find(u).is_none() {
            return Ok(fail(&format!(
                "Project with UUID {u} not found or inactive"
            )));
        }
    }
    let plan: Vec<NewSegment> = segments
        .iter()
        .filter_map(|s| {
            let (pid, _, p_billable) = find(&s.project_uuid)?;
            let billable = match s.billable {
                Some(b) => b,
                None => o.billable.or(Some(p_billable)),
            };
            Some(NewSegment {
                start_at: s.start_ms,
                end_at: s.end_ms,
                start_utc: s.start_ms,
                end_utc: s.end_ms,
                billable,
                project_id: pid,
                note: s.note.clone(),
            })
        })
        .collect();

    let created = retry_transient("split-time", || async {
        let mut tx = state.db.begin().await?;
        let shots = lock_entry_for_split(&mut tx, &o).await?;
        let mut created = Vec::with_capacity(plan.len());
        for seg in &plan {
            created.push(insert_segment(&mut tx, &o, seg).await?);
        }
        let bounds: Vec<(i64, i64)> = created
            .iter()
            .map(|c| (c.start_at.timestamp_millis(), c.end_at.timestamp_millis()))
            .collect();
        let pairs: Vec<(i32, i32)> = shots
            .iter()
            .filter_map(|(id, taken)| {
                created
                    .get(assign_segment(*taken, &bounds, true))
                    .map(|c| (*id, c.id))
            })
            .collect();
        move_screenshots(&mut tx, &pairs).await?;
        sqlx::query(r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = $1"#)
            .bind(o.id)
            .execute(&mut *tx)
            .await?;
        let orig = (o_end_utc - o_start_utc).div_euclid(1000);
        let new: i64 = plan
            .iter()
            .map(|s| s.end_utc - s.start_utc)
            .sum::<i64>()
            .div_euclid(1000);
        if new - orig != 0 {
            increment_total(&mut tx, o.tracking_id, new - orig).await?;
        }
        tx.commit().await?;
        Ok(created)
    })
    .await?;

    let mut affected = vec![o.project_id];
    for s in &plan {
        if !affected.contains(&s.project_id) {
            affected.push(s.project_id);
        }
    }
    // Node: tracking-timelines.service.ts:4422 — semua project terdampak.
    let mut intervals = vec![original_interval(&o)];
    intervals.extend(
        plan.iter()
            .map(|s| mz_overtime::NudgeInterval::new(s.start_utc, s.end_utc)),
    );
    mz_overtime::nudge_for_timelines(
        state,
        o.owner_id,
        affected.iter().copied().map(Some).collect(),
        intervals,
    );
    for p in affected {
        spawn::recalculate_burn(state, p);
    }
    Ok(json!({
        "success": true,
        "message": format!("Entry successfully split into {} segments", created.len()),
        "data": created.iter().zip(&segments).map(|(c, s)| json!({
            "uuid": c.uuid, "startAt": c.start_at, "endAt": c.end_at,
            "projectUuid": s.project_uuid, "billable": c.billable,
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::Segment;

    fn ms(s: &str) -> i64 {
        mz_core::time::parse_js_date(s).map_or(0, ms_of)
    }

    fn seg(a: &str, b: &str) -> Segment {
        Segment {
            start_ms: ms(a),
            end_ms: ms(b),
            project_uuid: "p".into(),
            billable: None,
            note: None,
        }
    }

    #[test]
    fn twin_detection() {
        let ex = MsRange::new(1_000, 5_000);
        assert!(is_twin(None, true, ex, 500, 6_000));
        assert!(!is_twin(Some("APPROVED"), true, ex, 500, 6_000));
        assert!(!is_twin(None, false, ex, 500, 6_000));
        assert!(!is_twin(None, true, ex, -1, 6_000));
        assert!(!is_twin(None, true, ex, 500, 4_999));
    }

    #[test]
    fn carve_produces_two_or_three_segments() {
        let s = plan_carve(
            0,
            100_000,
            0,
            100_000,
            20_000,
            50_000,
            Some(true),
            Some(false),
            7,
        );
        let s = s.unwrap_or_default();
        assert_eq!(s.len(), 3);
        assert_eq!(
            (s[0].start_at, s[0].end_at, s[0].billable),
            (0, 20_000, Some(true))
        );
        assert_eq!(
            (s[1].start_utc, s[1].end_utc, s[1].billable),
            (20_000, 50_000, Some(false))
        );
        assert_eq!((s[2].start_at, s[2].end_utc), (50_000, 100_000));
        // Clamp ke batas entri.
        let s = plan_carve(0, 100_000, 0, 100_000, -5, 200_000, None, None, 7).unwrap_or_default();
        assert_eq!(s.len(), 1);
        assert!(plan_carve(0, 100, 0, 100, 200, 300, None, None, 7).is_err());
    }

    #[test]
    fn screenshot_assignment() {
        let segs = [(0, 10), (10, 20)];
        assert_eq!(assign_segment(5, &segs, false), 0);
        assert_eq!(assign_segment(10, &segs, false), 1);
        assert_eq!(assign_segment(20, &segs, false), 1);
        assert_eq!(assign_segment(20, &segs, true), 1);
        assert_eq!(assign_segment(-1, &segs, true), 1);
    }

    #[test]
    fn split_time_snaps_and_detects_gaps() {
        let start = ms("2026-04-13T01:00:12.332Z");
        let end = ms("2026-04-13T02:00:40.100Z");
        let mut segs = vec![
            seg("2026-04-13T01:30:00Z", "2026-04-13T02:00:00Z"),
            seg("2026-04-13T01:00:00Z", "2026-04-13T01:30:00Z"),
        ];
        assert_eq!(plan_split_time(&mut segs, start, end), Ok(()));
        assert_eq!(segs[0].start_ms, start);
        assert_eq!(segs[1].end_ms, end);

        let mut gap = vec![
            seg("2026-04-13T01:00:00Z", "2026-04-13T01:20:00Z"),
            seg("2026-04-13T01:30:00Z", "2026-04-13T02:00:00Z"),
        ];
        let err = plan_split_time(&mut gap, start, end)
            .err()
            .unwrap_or_default();
        assert!(err.starts_with("Gap detected between segment 1 (ends 2026-04-13T01:20:00.000Z)"));

        let mut far = vec![
            seg("2026-04-13T00:50:00Z", "2026-04-13T01:30:00Z"),
            seg("2026-04-13T01:30:00Z", "2026-04-13T02:00:00Z"),
        ];
        let err = plan_split_time(&mut far, start, end)
            .err()
            .unwrap_or_default();
        assert_eq!(
            err,
            "First segment must start at the original entry's start time (2026-04-13T01:00:12.332Z), tolerance 1 minute"
        );

        let mut bad = vec![
            seg("2026-04-13T01:30:00Z", "2026-04-13T01:30:00Z"),
            seg("2026-04-13T01:00:00Z", "2026-04-13T01:30:00Z"),
        ];
        assert_eq!(
            plan_split_time(&mut bad, start, end),
            Err("Segment 2: endAt must be after startAt".to_owned())
        );
    }

    #[test]
    fn moment_utc_semantics() {
        assert_eq!(moment_utc(None, 42), Some(42));
        assert_eq!(moment_utc(Some(&Value::Null), 42), None);
        assert_eq!(
            moment_utc(Some(&json!("2026-01-01T00:00:00Z")), 42),
            Some(ms("2026-01-01T00:00:00Z"))
        );
    }
}

#[cfg(test)]
mod trunc_tests {
    use super::*;

    #[test]
    fn int_of_truncates_fractions() {
        assert!(matches!(int_of(Some(&json!(-420.5)), "o"), Ok(Some(-420))));
        assert!(matches!(int_of(Some(&json!(2.9)), "o"), Ok(Some(2))));
        assert!(int_of(Some(&json!(3_000_000_000.0)), "o").is_err());
    }
}
