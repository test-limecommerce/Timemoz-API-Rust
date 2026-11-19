//! `POST /tracking-timelines/manual-time` (`createManualTime`, service:629) +
//! bagian `manual-time-settings` yang dipanggilnya (locks, quota). Approve otomatisnya
//! lewat [`crate::approve`].

use chrono::NaiveDateTime;
use chrono_tz::Tz;
use mz_core::AppState;
use mz_core::time::JsDate;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::approve::{ApproveOutcome, approve_in_tx};
use crate::dto::{CreateManualTimeDto, date_ms, str_of, truthy_str, uuid_of};
use crate::fail::{TResult, Thrown, with_tx_timeout};
use crate::range::{
    DAY_MS, MANUAL_TIME_MAX_DURATION_MS, MsRange, decide_eligibility, ends_in_future, local_date,
    ndt, period_bounds, reason, timeline_range_ms, zone_or_utc,
};
use crate::tx::lock_tracking;

const ALLOWED_ATTACHMENT_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "application/pdf",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
];
const MAX_ATTACHMENT_BYTES: f64 = 10.0 * 1024.0 * 1024.0;

const MANUAL_TIME_FUTURE_END_MESSAGE: &str = "Manual time cannot end in the future";
const MANUAL_TIME_AUTO_APPROVED_MESSAGE: &str = "Manual time submitted and approved automatically";
const MANUAL_TIME_TRIMMED_SUFFIX: &str = "; the part already recorded was removed";
pub const NOT_ALLOWED_FOR_USER: &str =
    "You are not allowed to manage tracking timeline for this user";

fn fail(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

// ---------------------------------------------------------------------------
// createManualTime

#[derive(sqlx::FromRow)]
struct Settings {
    mode: String,
    limit_minutes: Option<i32>,
    period: String,
    time_zone: Option<String>,
    workweek_start_day: i32,
    is_allow_manual_time: Option<bool>,
    has_exception: bool,
    exception_limit: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct Quota {
    company_id: i32,
    limit_seconds: i64,
    week: bool,
    zone: Tz,
    workweek_start_day: i32,
}

#[derive(sqlx::FromRow)]
struct ManualRow {
    id: i32,
    uuid: Uuid,
    start_at: JsDate,
    end_at: JsDate,
    note: String,
    reason: Option<String>,
    manual_time_status: Option<String>,
    manual_time_approval_source: Option<String>,
    approved_at: Option<JsDate>,
    requested_at: Option<JsDate>,
    // Untuk nudge lembur (`rowInterval(timeline)`).
    start_at_utc: Option<NaiveDateTime>,
    end_at_utc: Option<NaiveDateTime>,
    timezone: Option<String>,
}

const MANUAL_ROW_COLS: &str = r#"id, uuid, "startAt" AS start_at, "endAt" AS end_at, note, reason,
    "manualTimeStatus"::text AS manual_time_status,
    "manualTimeApprovalSource"::text AS manual_time_approval_source,
    "approvedAt" AS approved_at, "requestedAt" AS requested_at,
    "startAtUtc" AS start_at_utc, "endAtUtc" AS end_at_utc, timezone"#;

enum TxResult {
    Duplicate,
    Pending(ManualRow, String),
    Approved(ManualRow, bool),
}

struct AttachmentRow {
    uuid: Uuid,
    file_name: String,
    full_path: String,
    mime: String,
}

/// Masukan yang sudah dibaca di luar transaksi.
struct Ctx<'a> {
    dto: &'a CreateManualTimeDto,
    start_ms: i64,
    end_ms: i64,
    entry: MsRange,
    now_ms: i64,
    project_id: i32,
    project_billable: bool,
    tracking_id: i32,
    owner_id: i32,
    budget_allowed: bool,
}

/// Angka JSON → kolom `Int` (Prisma menolak pecahan).
fn int_col(v: Option<&Value>, field: &str) -> TResult<Option<i32>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => crate::service::trunc_i32(n).map(Some).ok_or_else(|| {
            Thrown::msg(format!(
                "Argument `{field}`: Invalid value provided. Expected Int."
            ))
        }),
        Some(_) => Err(Thrown::msg(format!(
            "Argument `{field}`: Invalid value provided."
        ))),
    }
}

async fn run_tx(
    conn: &mut PgConnection,
    c: &Ctx<'_>,
    attachments: &mut Vec<AttachmentRow>,
) -> TResult<TxResult> {
    // resolveDecidingCompanyId
    let company_id: Option<i32> = sqlx::query_scalar(
        r#"SELECT pc."companyId" FROM "projectCompany" pc
           WHERE pc."projectId" = $1 AND pc.status = 1
             AND EXISTS(SELECT 1 FROM "userCompany" uc
                        WHERE uc."companyId" = pc."companyId" AND uc."userId" = $2 AND uc.status = 1)
           ORDER BY pc.id ASC LIMIT 1"#,
    )
    .bind(c.project_id)
    .bind(c.owner_id)
    .fetch_optional(&mut *conn)
    .await?;

    let mut decision: Result<(), &'static str> = Err(reason::NOT_ALLOWED);
    let mut mode: Option<String> = None;
    let mut company_zone = Tz::UTC;
    let mut quota: Option<Quota> = None;
    if let Some(cid) = company_id {
        // Urutan lock: settings → quota → tracking.
        mz_core::db::advisory_xact_lock_shared(
            conn,
            &format!("manual-time-settings:company:{cid}"),
        )
        .await?;
        let settings: Option<Settings> = sqlx::query_as(
            r#"SELECT c."manualTimeApprovalMode"::text AS mode,
                      c."manualTimeAutoApproveLimitMinutes" AS limit_minutes,
                      c."manualTimeAutoApprovePeriod"::text AS period,
                      c."timeZone" AS time_zone, c."workweekStartDay" AS workweek_start_day,
                      (SELECT uc."isAllowManualTime" FROM "userCompany" uc
                        WHERE uc."userId" = $2 AND uc."companyId" = $1 AND uc.status = 1 LIMIT 1) AS is_allow_manual_time,
                      (e.id IS NOT NULL) AS has_exception, e."limitMinutes" AS exception_limit
               FROM company c
               LEFT JOIN LATERAL (SELECT id, "limitMinutes" FROM "manualTimeApprovalException"
                                  WHERE "companyId" = $1 AND "userId" = $2 AND "deletedAt" IS NULL LIMIT 1) e ON true
               WHERE c.id = $1"#,
        )
        .bind(cid)
        .bind(c.owner_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(s) = settings {
            let has_tz = str_of(c.dto.timezone.as_ref()).is_some_and(|t| !t.trim().is_empty());
            decision = decide_eligibility(
                &s.mode,
                s.is_allow_manual_time == Some(true),
                s.has_exception && s.exception_limit.is_none(),
                has_tz,
                c.entry.start,
                c.now_ms,
            );
            if decision.is_ok() && !c.budget_allowed {
                decision = Err(reason::BUDGET);
            }
            company_zone = zone_or_utc(s.time_zone.as_deref());
            if s.mode == "LIMIT"
                && let Some(limit) = s.limit_minutes.filter(|l| *l > 0)
                && (s.period == "DAY" || s.period == "WEEK")
            {
                let minutes = s
                    .exception_limit
                    .filter(|_| s.has_exception)
                    .unwrap_or(limit);
                quota = Some(Quota {
                    company_id: cid,
                    limit_seconds: i64::from(minutes) * 60,
                    week: s.period == "WEEK",
                    zone: company_zone,
                    workweek_start_day: s.workweek_start_day,
                });
            }
            mode = Some(s.mode);
        }
    }

    if decision.is_ok()
        && let Some(q) = quota
    {
        mz_core::db::advisory_xact_lock(
            conn,
            &format!(
                "manual-time-quota:company:{}:user:{}",
                q.company_id, c.owner_id
            ),
        )
        .await?;
    }

    lock_tracking(conn, c.tracking_id).await?;
    let duplicate: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "trackingTimeline"
           WHERE "trackingId" = $1 AND "projectId" = $2 AND "startAt" = $3 AND "endAt" = $4 LIMIT 1"#,
    )
    .bind(c.tracking_id)
    .bind(c.project_id)
    .bind(ndt(c.start_ms))
    .bind(ndt(c.end_ms))
    .fetch_optional(&mut *conn)
    .await?;
    if duplicate.is_some() {
        return Ok(TxResult::Duplicate);
    }

    let dto = c.dto;
    let start_utc = date_ms(dto.start_at_utc.as_ref()).unwrap_or(c.start_ms);
    let end_utc = date_ms(dto.end_at_utc.as_ref()).unwrap_or(c.end_ms);
    let billable = match dto.billable.as_ref() {
        None => Some(c.project_billable),
        Some(v) => v.as_bool(),
    };
    let offset = int_col(dto.offset_minutes.as_ref(), "offsetMinutes")?;
    let timeline: ManualRow = sqlx::query_as(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
             timezone, "offsetMinutes", "trackingId", "projectId", status, "manualTimeStatus", reason,
             "requestedAt", billable)
           VALUES ($1, true, $2, $3, $4, $5, $6, $7, $8, $9, 0, 'PENDING', $10, $11, $12)
           RETURNING {MANUAL_ROW_COLS}"#
    ))
    .bind(truthy_str(dto.note.as_ref()).unwrap_or("-"))
    .bind(ndt(c.start_ms))
    .bind(ndt(c.end_ms))
    .bind(ndt(start_utc))
    .bind(ndt(end_utc))
    .bind(str_of(dto.timezone.as_ref()))
    .bind(offset)
    .bind(c.tracking_id)
    .bind(c.project_id)
    .bind(str_of(dto.reason.as_ref()))
    .bind(mz_core::time::now_naive())
    .bind(billable)
    .fetch_one(&mut *conn)
    .await?;

    // Lampiran ikut transaksi entri; `uuid` dibuat di sini supaya path final.
    attachments.clear();
    let mut sizes = Vec::new();
    for att in dto.attachments().unwrap_or_default() {
        let uuid = Uuid::new_v4();
        let size = if att.file_size.is_finite()
            && att.file_size.trunc() >= f64::from(i32::MIN)
            && att.file_size.trunc() <= f64::from(i32::MAX)
        {
            #[allow(clippy::cast_possible_truncation)]
            Ok(att.file_size.trunc() as i32)
        } else {
            Err(Thrown::msg(
                "Argument `fileSize`: Invalid value provided. Expected Int, provided Float.",
            ))
        }?;
        sizes.push(size);
        attachments.push(AttachmentRow {
            full_path: format!("manual-time/{}/{uuid}_{}", timeline.uuid, att.file_name),
            uuid,
            file_name: att.file_name,
            mime: att.mime_type,
        });
    }
    if !attachments.is_empty() {
        let uuids: Vec<Uuid> = attachments.iter().map(|a| a.uuid).collect();
        let names: Vec<&str> = attachments.iter().map(|a| a.file_name.as_str()).collect();
        let paths: Vec<&str> = attachments.iter().map(|a| a.full_path.as_str()).collect();
        let mimes: Vec<&str> = attachments.iter().map(|a| a.mime.as_str()).collect();
        sqlx::query(
            r#"INSERT INTO "manualTimeAttachment" (uuid, "fileName", "fileFullpath", "fileSize", "mimeType",
                 "trackingTimelineId", status)
               SELECT u, n, p, s, m, $6, 1 FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::int[], $5::text[])
                 AS x(u, n, p, s, m)"#,
        )
        .bind(&uuids)
        .bind(&names)
        .bind(&paths)
        .bind(&sizes)
        .bind(&mimes)
        .bind(timeline.id)
        .execute(&mut *conn)
        .await?;
    }

    if let Err(why) = decision {
        return Ok(TxResult::Pending(timeline, why.to_owned()));
    }

    let mut max_approved: Option<f64> = None;
    if mode.as_deref() == Some("LIMIT") {
        let Some(q) = quota else {
            return Ok(TxResult::Pending(timeline, "quota-unavailable".into()));
        };
        let bounds = period_bounds(c.entry.start, q.zone, q.week, q.workweek_start_day);
        let used = used_approved_manual_seconds(conn, c.owner_id, q.company_id, bounds).await?;
        #[allow(clippy::cast_precision_loss)]
        {
            max_approved = Some((q.limit_seconds - used).max(0) as f64);
        }
    }

    let mut work_remaining: Option<f64> = None;
    if let Some(cid) = company_id {
        let work_day = local_date(c.entry.start, company_zone);
        if let Some(limit) = crate::worklimit::load_limit(conn, c.owner_id, cid).await? {
            let r = crate::worklimit::remaining_for(
                conn,
                c.owner_id,
                limit,
                work_day,
                Some(company_zone),
            )
            .await?;
            work_remaining = match (r.daily, r.weekly) {
                (Some(d), Some(w)) => Some(d.min(w)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
        }
    }
    if let Some(w) = work_remaining {
        max_approved = Some(max_approved.unwrap_or(f64::INFINITY).min(w));
    }

    let outcome = approve_in_tx(conn, timeline.id, None, "AUTO", max_approved).await?;
    let ApproveOutcome::Approved { adjusted, .. } = outcome else {
        #[allow(clippy::cast_precision_loss)]
        let why = match outcome {
            ApproveOutcome::OverLimit {
                approved_seconds, ..
            } if work_remaining.is_some_and(|w| approved_seconds as f64 > w) => "work-limit",
            o => o.kind(),
        };
        return Ok(TxResult::Pending(timeline, why.to_owned()));
    };
    let approved: Option<ManualRow> = sqlx::query_as(&format!(
        r#"SELECT {MANUAL_ROW_COLS} FROM "trackingTimeline" WHERE id = $1"#
    ))
    .bind(timeline.id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(TxResult::Approved(approved.unwrap_or(timeline), adjusted))
}

// `manual-time-settings/manual-time-quota.ts`.
/// `usedApprovedManualSeconds(tx, { ownerId, companyId, periodStartMs, periodEndMs })`.
async fn used_approved_manual_seconds(
    conn: &mut PgConnection,
    owner: i32,
    company_id: i32,
    period: MsRange,
) -> sqlx::Result<i64> {
    let rows: Vec<(NaiveDateTime, NaiveDateTime, Option<String>)> = sqlx::query_as(
        r#"SELECT tt."startAt", tt."endAt", tt.timezone
           FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
           WHERE t."userId" = $1 AND tt."startAt" >= $2 AND tt."startAt" < $3
             AND tt."manualTimeStatus" = 'APPROVED' AND tt.status = 1
             AND EXISTS(SELECT 1 FROM "projectCompany" pc
                        WHERE pc."projectId" = tt."projectId" AND pc."companyId" = $4 AND pc.status = 1)"#,
    )
    .bind(owner)
    .bind(ndt(period.start - DAY_MS))
    .bind(ndt(period.end + DAY_MS))
    .bind(company_id)
    .fetch_all(conn)
    .await?;
    Ok(sum_in_period(&rows, period))
}

fn sum_in_period(rows: &[(NaiveDateTime, NaiveDateTime, Option<String>)], period: MsRange) -> i64 {
    rows.iter()
        .map(|(s, e, tz)| timeline_range_ms(*s, *e, tz.as_deref()))
        .filter(|r| r.start >= period.start && r.start < period.end)
        .map(|r| (r.end - r.start) / 1000)
        .sum()
}

fn row_json(row: &ManualRow, approved: bool, attachments: &[Value]) -> Value {
    json!({
        "uuid": row.uuid,
        "startAt": row.start_at,
        "endAt": row.end_at,
        "note": row.note,
        "reason": row.reason,
        "manualTimeStatus": row.manual_time_status,
        "manualTimeApprovalSource": if approved { json!(row.manual_time_approval_source) } else { Value::Null },
        "approvedAt": if approved { json!(row.approved_at) } else { Value::Null },
        "requestedAt": row.requested_at,
        "attachments": attachments,
    })
}

/// Node: src/tracking-timelines/tracking-timelines.service.ts:629 `createManualTime`.
pub async fn create_manual_time(state: &AppState, dto: &CreateManualTimeDto, caller: i32) -> Value {
    match create_inner(state, dto, caller).await {
        Ok(v) => v,
        // `{ success: false, message: error.message, data: error }` — `JSON.stringify(Error)` = `{}`.
        Err(t) => json!({ "success": false, "message": t.message(), "data": t.data() }),
    }
}

async fn create_inner(state: &AppState, dto: &CreateManualTimeDto, caller: i32) -> TResult<Value> {
    let (Some(start_ms), Some(end_ms)) =
        (date_ms(dto.start_at.as_ref()), date_ms(dto.end_at.as_ref()))
    else {
        return Ok(fail("Invalid endtime"));
    };
    if end_ms <= start_ms {
        return Ok(fail("Invalid endtime"));
    }
    if end_ms - start_ms > MANUAL_TIME_MAX_DURATION_MS {
        return Ok(fail("Duration exceeds maximum limit of 14 hours"));
    }
    let attachments = dto.attachments();
    if let Some(list) = &attachments {
        if list.len() > 5 {
            return Ok(fail("Maximum 5 attachments allowed"));
        }
        for att in list {
            if !ALLOWED_ATTACHMENT_MIME_TYPES.contains(&att.mime_type.as_str()) {
                return Ok(fail(&format!("File type not allowed: {}", att.mime_type)));
            }
            if att.file_size > MAX_ATTACHMENT_BYTES {
                return Ok(fail(&format!("File {} exceeds 10MB limit", att.file_name)));
            }
        }
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let entry = timeline_range_ms(ndt(start_ms), ndt(end_ms), str_of(dto.timezone.as_ref()));
    if ends_in_future(entry.end, now_ms) {
        return Ok(fail(MANUAL_TIME_FUTURE_END_MESSAGE));
    }

    let project_uuid = str_of(dto.project_uuid.as_ref()).and_then(uuid_of);
    let tracking_uuid = str_of(dto.tracking_uuid.as_ref()).and_then(uuid_of);
    let (project, tracking) = tokio::try_join!(
        sqlx::query_as::<_, (i32, bool)>(
            r#"SELECT id, billable FROM project WHERE uuid = $1 LIMIT 1"#
        )
        .bind(project_uuid)
        .fetch_optional(&state.db),
        sqlx::query_as::<_, (i32, i32)>(
            r#"SELECT id, "userId" FROM tracking WHERE uuid = $1 AND status = 1 LIMIT 1"#
        )
        .bind(tracking_uuid)
        .fetch_optional(&state.db),
    )?;
    let Some((project_id, project_billable)) = project else {
        return Ok(fail("Project UUID not found"));
    };
    let Some((tracking_id, owner_id)) = tracking else {
        return Ok(fail("Tracking UUID not found"));
    };
    if !crate::access::can_access_owner_data(&state.db, owner_id, caller).await? {
        return Ok(fail(NOT_ALLOWED_FOR_USER));
    }
    let pre = crate::access::precheck(&state.db, project_id, owner_id, None).await?;
    if !pre.owner_project {
        return Ok(fail("Project UUID not found"));
    }
    let ctx = Ctx {
        dto,
        start_ms,
        end_ms,
        entry,
        now_ms,
        project_id,
        project_billable,
        tracking_id,
        owner_id,
        budget_allowed: pre.budget_block().is_none(),
    };

    let mut rows = Vec::new();
    let result = with_tx_timeout(async {
        let mut tx = state.db.begin().await?;
        let r = run_tx(&mut tx, &ctx, &mut rows).await?;
        tx.commit().await?;
        Ok(r)
    })
    .await?;

    let (timeline, approved, adjusted, why) = match result {
        TxResult::Duplicate => {
            return Ok(fail(
                "Manual time with the same start and end time already exists",
            ));
        }
        TxResult::Pending(t, why) => (t, false, false, why),
        TxResult::Approved(t, adjusted) => (t, true, adjusted, "eligible".to_owned()),
    };
    tracing::info!(
        target: "app_event",
        "manual-time decision uuid={} owner={owner_id} outcome={} reason={why}",
        timeline.uuid,
        if approved { "AUTO" } else { "PENDING" },
    );
    // Node: tracking-timelines.service.ts:1086 — entri PENDING tidak dihitung `workedSec`;
    // nudge hanya bila disetujui otomatis.
    if approved {
        mz_overtime::nudge_for_timelines(
            state,
            Some(owner_id),
            vec![Some(project_id)],
            vec![mz_overtime::NudgeInterval::of_row(
                timeline.start_at.0,
                timeline.end_at.0,
                timeline.start_at_utc,
                timeline.end_at_utc,
                timeline.timezone.as_deref(),
            )],
        );
    }

    let mut uploads = Vec::with_capacity(rows.len());
    for att in &rows {
        let signed = mz_files::get_signed_file_url(state, &att.full_path, 3600, &att.mime)
            .await
            .map_err(|e| Thrown::msg(e.to_string()))?;
        uploads
            .push(json!({ "uuid": att.uuid, "fileName": att.file_name, "uploadUrl": signed.url }));
    }

    if approved {
        crate::spawn::recalculate_burn(state, project_id);
        let message = if adjusted {
            format!("{MANUAL_TIME_AUTO_APPROVED_MESSAGE}{MANUAL_TIME_TRIMMED_SUFFIX}")
        } else {
            MANUAL_TIME_AUTO_APPROVED_MESSAGE.to_owned()
        };
        return Ok(
            json!({ "success": true, "message": message, "data": row_json(&timeline, true, &uploads) }),
        );
    }

    crate::spawn::approval_emails(
        state,
        timeline.id,
        owner_id,
        project_id,
        start_ms,
        end_ms,
        str_of(dto.reason.as_ref()).unwrap_or_default().to_owned(),
    );
    Ok(json!({
        "success": true,
        "message": "Manual time submitted, waiting for approval",
        "data": row_json(&timeline, false, &uploads),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approve::approve_pieces;
    use crate::range::ms_of;

    fn dt(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn approve_trims_already_recorded_parts() {
        let req = MsRange::new(0, 8 * 3_600_000);
        // Tracking live 1-2 jam & 2.5-2.99 jam; sisa di bawah 1 menit dibuang.
        let others = [
            MsRange::new(3_600_000, 7_200_000),
            MsRange::new(7_230_000, 9_000_000),
        ];
        let pieces = approve_pieces(req, &others);
        assert_eq!(
            pieces,
            vec![
                MsRange::new(0, 3_600_000),
                MsRange::new(9_000_000, 8 * 3_600_000)
            ]
        );
        assert!(approve_pieces(req, &[MsRange::new(-1, 8 * 3_600_000 + 1)]).is_empty());
    }

    #[test]
    fn quota_sums_by_start_in_period() {
        let period = MsRange::new(
            ms_of(dt("2026-11-16T00:00:00Z")),
            ms_of(dt("2026-11-17T00:00:00Z")),
        );
        let rows = vec![
            (
                dt("2026-11-16T01:00:00Z"),
                dt("2026-11-16T02:00:00Z"),
                Some("UTC".to_owned()),
            ),
            // Mulai sebelum periode: tidak dihitung walau berakhir di dalamnya.
            (
                dt("2026-11-15T23:00:00Z"),
                dt("2026-11-16T00:30:00Z"),
                Some("UTC".to_owned()),
            ),
            // Legacy: jam dinding 06:00 WIB = 23:00Z tgl 15 → di luar.
            (dt("2026-11-16T06:00:00Z"), dt("2026-11-16T07:00:00Z"), None),
        ];
        assert_eq!(sum_in_period(&rows, period), 3600);
    }

    #[test]
    fn int_column_truncates_fractions() {
        assert!(matches!(
            int_col(Some(&json!(420)), "offsetMinutes"),
            Ok(Some(420))
        ));
        assert!(matches!(
            int_col(Some(&json!(-420.5)), "offsetMinutes"),
            Ok(Some(-420))
        ));
        assert!(int_col(Some(&json!(3_000_000_000.0)), "offsetMinutes").is_err());
        assert!(matches!(int_col(None, "x"), Ok(None)));
    }
}
