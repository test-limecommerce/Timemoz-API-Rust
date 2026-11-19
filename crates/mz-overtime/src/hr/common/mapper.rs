//! Port `src/overtime/overtime-request.mapper.ts`: baris `overtimeRequest` →
//! `OvertimeRequestEntity` (specs/004 T043). Menit selalu `floor(detik / 60)`; detik
//! lembur dikirim persis. Tanpa query: hari "live" sebelum persetujuan dihitung
//! pemanggil secara batch (`crate::calendar::load_batch` + `day_info_for`).
//!
//! Query baris: `SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM} WHERE …`
//! (alias tabel `r`), lalu `sqlx::query_as::<_, OvertimeRequestRow>`.

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::time::JsDate;
use serde::Serialize;

use super::enums::{
    OvertimeCancelReason, OvertimeDayType, OvertimeRequestKind, OvertimeRequestStatus,
};
use crate::calendar::{DayInfo, normal_sec_after_leave};

/// Kolom `OVERTIME_REQUEST_SELECT` (alias `r` untuk `overtimeRequest`, `eu`/`rb`/`db`
/// untuk `user`/`requestedBy`/`decidedBy`). Pasangkan dengan [`OVERTIME_REQUEST_FROM`].
pub const OVERTIME_REQUEST_COLUMNS: &str = r#"r.id, r.uuid::text AS uuid, r."userId" AS user_id,
  r."requestedById" AS requested_by_id, r."decidedById" AS decided_by_id,
  r.kind::text AS kind, r.status::text AS status, r.date, r."requestedSec" AS requested_sec,
  r.reason, r."responseDeadline" AS response_deadline, r."finalAt" AS final_at,
  r."decidedAt" AS decided_at, r."rejectReason" AS reject_reason,
  r."cancelReason"::text AS cancel_reason, r."dayType"::text AS day_type,
  r."baseNormalSec" AS base_normal_sec, r."dailyLimitSec" AS daily_limit_sec,
  r."normalSec" AS normal_sec, r."workedSec" AS worked_sec, r."overtimeSec" AS overtime_sec,
  r."computedAt" AS computed_at, r."finalizedAt" AS finalized_at,
  r."exceedsLimit" AS exceeds_limit, r."exceedsDailyLimit" AS exceeds_daily_limit,
  r."exceedsWeeklyLimit" AS exceeds_weekly_limit, r."createdAt" AS created_at,
  eu.uuid::text AS user_uuid, eu.name AS user_name,
  rb.uuid::text AS requested_by_uuid, rb.name AS requested_by_name,
  dbu.uuid::text AS decided_by_uuid, dbu.name AS decided_by_name"#;

/// `FROM` + join relasi orang untuk [`OVERTIME_REQUEST_COLUMNS`].
pub const OVERTIME_REQUEST_FROM: &str = r#""overtimeRequest" r
  JOIN users eu ON eu.id = r."userId"
  JOIN users rb ON rb.id = r."requestedById"
  LEFT JOIN users dbu ON dbu.id = r."decidedById""#;

/// `OvertimeRequestRow` (hasil `OVERTIME_REQUEST_SELECT`).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OvertimeRequestRow {
    pub id: i32,
    pub uuid: String,
    pub user_id: i32,
    pub requested_by_id: i32,
    pub decided_by_id: Option<i32>,
    pub kind: OvertimeRequestKind,
    pub status: OvertimeRequestStatus,
    pub date: NaiveDate,
    pub requested_sec: i32,
    pub reason: String,
    pub response_deadline: NaiveDateTime,
    pub final_at: NaiveDateTime,
    pub decided_at: Option<NaiveDateTime>,
    pub reject_reason: Option<String>,
    pub cancel_reason: Option<OvertimeCancelReason>,
    /// Snapshot `dayType` (`WORKDAY`/`HOLIDAY`), terisi saat disetujui.
    pub day_type: Option<String>,
    pub base_normal_sec: Option<i32>,
    pub daily_limit_sec: Option<i32>,
    pub normal_sec: Option<i32>,
    pub worked_sec: Option<i32>,
    pub overtime_sec: Option<i32>,
    pub computed_at: Option<NaiveDateTime>,
    pub finalized_at: Option<NaiveDateTime>,
    pub exceeds_limit: bool,
    pub exceeds_daily_limit: bool,
    pub exceeds_weekly_limit: bool,
    pub created_at: NaiveDateTime,
    pub user_uuid: String,
    pub user_name: String,
    pub requested_by_uuid: String,
    pub requested_by_name: String,
    pub decided_by_uuid: Option<String>,
    pub decided_by_name: Option<String>,
}

/// Hari "live" sebelum persetujuan; `normal_sec` sudah dipotong cuti setengah hari.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OvertimeLiveDay {
    pub day_type: OvertimeDayType,
    pub normal_sec: i64,
}

/// `normalSecAfterLeave` untuk tampilan/kuota. Nilai dasar selalu genap
/// (28 800/25 200/18 000/0); bila ganjil (data rusak) dipakai `floor(base / 2)` —
/// Node menghasilkan pecahan yang lalu di-`floor` saat jadi menit.
pub fn normal_sec_live(info: &DayInfo) -> i64 {
    normal_sec_after_leave(info.base_normal_sec, info.leave).unwrap_or(info.base_normal_sec / 2)
}

/// `liveDayOf(info)`: `DayInfo` kalender → hari live (normal setelah cuti, R1.1).
pub fn live_day_of(info: &DayInfo) -> OvertimeLiveDay {
    OvertimeLiveDay {
        day_type: info.day_type,
        normal_sec: normal_sec_live(info),
    }
}

/// `dateKeyOf(date)`: kolom `@db.Date` → `YYYY-MM-DD`.
pub fn date_key_of(date: NaiveDate) -> String {
    crate::zone::date_key(date)
}

/// Rujukan orang: `id` = `users.uuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OvertimePersonRef {
    pub id: String,
    pub name: String,
}

/// `OvertimeRequestEntity` (urutan kunci = Node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OvertimeRequestEntity {
    pub id: String,
    pub kind: OvertimeRequestKind,
    pub status: OvertimeRequestStatus,
    pub date: String,
    pub duration_minutes: i64,
    pub reason: String,
    pub employee: OvertimePersonRef,
    pub requested_by: OvertimePersonRef,
    pub decided_by: Option<OvertimePersonRef>,
    pub decided_at: Option<JsDate>,
    pub reject_reason: Option<String>,
    pub cancel_reason: Option<OvertimeCancelReason>,
    pub cancel_reason_label: Option<&'static str>,
    pub response_deadline: JsDate,
    pub day_type: Option<OvertimeDayType>,
    pub normal_minutes: Option<i64>,
    pub worked_minutes: Option<i64>,
    pub overtime_minutes: Option<i64>,
    pub overtime_seconds: Option<i32>,
    pub computed_at: Option<JsDate>,
    pub is_final: bool,
    pub daily_limit_minutes: Option<i64>,
    pub exceeds_limit: bool,
    pub exceeds_daily_limit: bool,
    pub exceeds_weekly_limit: bool,
    pub created_at: JsDate,
}

/// `floor(detik / 60)`.
pub fn to_minutes(sec: i64) -> i64 {
    sec.div_euclid(60)
}

fn minutes_or_null(sec: Option<i32>) -> Option<i64> {
    sec.map(|s| to_minutes(i64::from(s)))
}

/// `toOvertimeRequestEntity(row, live?)`. Setelah persetujuan (snapshot `dayType`
/// terisi): snapshot, `normalSec ?? baseNormalSec`. Sebelumnya: `live`. Tanpa keduanya: null.
pub fn to_overtime_request_entity(
    row: &OvertimeRequestRow,
    live: Option<&OvertimeLiveDay>,
) -> OvertimeRequestEntity {
    let snapshot = row.day_type.as_deref().and_then(OvertimeDayType::parse);
    let (day_type, normal_sec) = match (snapshot, live) {
        (Some(dt), _) => (
            Some(dt),
            row.normal_sec.or(row.base_normal_sec).map(i64::from),
        ),
        (None, Some(l)) => (Some(l.day_type), Some(l.normal_sec)),
        (None, None) => (None, None),
    };
    let decided_by = match (&row.decided_by_uuid, &row.decided_by_name) {
        (Some(id), Some(name)) => Some(OvertimePersonRef {
            id: id.clone(),
            name: name.clone(),
        }),
        _ => None,
    };
    OvertimeRequestEntity {
        id: row.uuid.clone(),
        kind: row.kind,
        status: row.status,
        date: date_key_of(row.date),
        duration_minutes: to_minutes(i64::from(row.requested_sec)),
        reason: row.reason.clone(),
        employee: OvertimePersonRef {
            id: row.user_uuid.clone(),
            name: row.user_name.clone(),
        },
        requested_by: OvertimePersonRef {
            id: row.requested_by_uuid.clone(),
            name: row.requested_by_name.clone(),
        },
        decided_by,
        decided_at: row.decided_at.map(JsDate),
        reject_reason: row.reject_reason.clone(),
        cancel_reason: row.cancel_reason,
        cancel_reason_label: row.cancel_reason.map(|r| r.label()),
        response_deadline: JsDate(row.response_deadline),
        day_type,
        normal_minutes: normal_sec.map(to_minutes),
        worked_minutes: minutes_or_null(row.worked_sec),
        overtime_minutes: minutes_or_null(row.overtime_sec),
        overtime_seconds: row.overtime_sec,
        computed_at: row.computed_at.map(JsDate),
        is_final: row.finalized_at.is_some(),
        daily_limit_minutes: minutes_or_null(row.daily_limit_sec),
        exceeds_limit: row.exceeds_limit,
        exceeds_daily_limit: row.exceeds_daily_limit,
        exceeds_weekly_limit: row.exceeds_weekly_limit,
        created_at: JsDate(row.created_at),
    }
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-request.mapper.spec.ts`.
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::calendar::LeaveCoverage;
    use serde_json::json;

    fn ts(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    fn person(n: u32) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn make_row() -> OvertimeRequestRow {
        OvertimeRequestRow {
            id: 1,
            uuid: "11111111-1111-4111-8111-111111111111".into(),
            user_id: 7,
            requested_by_id: 7,
            decided_by_id: None,
            kind: OvertimeRequestKind::SELF,
            status: OvertimeRequestStatus::PENDING_APPROVAL,
            date: crate::zone::parse_date_key("2026-10-22").unwrap_or_default(),
            requested_sec: 7_259,
            reason: "Closing bulanan".into(),
            response_deadline: ts("2026-10-29T16:59:59.999Z"),
            final_at: ts("2026-10-29T16:59:59.999Z"),
            decided_at: None,
            reject_reason: None,
            cancel_reason: None,
            day_type: None,
            base_normal_sec: None,
            daily_limit_sec: None,
            normal_sec: None,
            worked_sec: None,
            overtime_sec: None,
            computed_at: None,
            finalized_at: None,
            exceeds_limit: false,
            exceeds_daily_limit: false,
            exceeds_weekly_limit: false,
            created_at: ts("2026-10-21T08:00:00.000Z"),
            user_uuid: person(7),
            user_name: "Budi".into(),
            requested_by_uuid: person(7),
            requested_by_name: "Budi".into(),
            decided_by_uuid: None,
            decided_by_name: None,
        }
    }

    #[test]
    fn before_approval_uses_live() {
        let e = to_overtime_request_entity(
            &make_row(),
            Some(&OvertimeLiveDay {
                day_type: OvertimeDayType::Workday,
                normal_sec: 25_259,
            }),
        );
        let v = serde_json::to_value(&e).unwrap_or_default();
        assert_eq!(v["id"], "11111111-1111-4111-8111-111111111111");
        assert_eq!(v["kind"], "SELF");
        assert_eq!(v["status"], "PENDING_APPROVAL");
        assert_eq!(v["date"], "2026-10-22");
        assert_eq!(v["durationMinutes"], 120);
        assert_eq!(v["employee"], json!({"id": person(7), "name": "Budi"}));
        assert_eq!(v["requestedBy"], json!({"id": person(7), "name": "Budi"}));
        assert_eq!(v["decidedBy"], Value::Null);
        assert_eq!(v["decidedAt"], Value::Null);
        assert_eq!(v["dayType"], "WORKDAY");
        assert_eq!(v["normalMinutes"], 420);
        assert_eq!(v["workedMinutes"], Value::Null);
        assert_eq!(v["overtimeMinutes"], Value::Null);
        assert_eq!(v["overtimeSeconds"], Value::Null);
        assert_eq!(v["computedAt"], Value::Null);
        assert_eq!(v["isFinal"], false);
        assert_eq!(v["responseDeadline"], "2026-10-29T16:59:59.999Z");
        assert_eq!(v["createdAt"], "2026-10-21T08:00:00.000Z");
        // Urutan kunci = entity Node.
        let raw = serde_json::to_string(&e).unwrap_or_default();
        assert!(raw.starts_with(r#"{"id":"#), "{raw}");
        assert!(
            raw.contains(r#""createdAt":"2026-10-21T08:00:00.000Z"}"#),
            "{raw}"
        );
        assert_eq!(v.as_object().map(serde_json::Map::len), Some(26));
    }

    use serde_json::Value;

    #[test]
    fn cancel_reason_labels() {
        assert_eq!(
            to_overtime_request_entity(&make_row(), None).cancel_reason_label,
            None
        );
        let label = |r: OvertimeCancelReason| {
            let mut row = make_row();
            row.status = OvertimeRequestStatus::CANCELLED;
            row.cancel_reason = Some(r);
            to_overtime_request_entity(&row, None).cancel_reason_label
        };
        assert_eq!(
            label(OvertimeCancelReason::BY_REQUESTER),
            Some("Dibatalkan pengaju")
        );
        assert_eq!(
            label(OvertimeCancelReason::NO_RESPONSE),
            Some("Tidak direspons sampai batas waktu")
        );
        assert_eq!(
            label(OvertimeCancelReason::EMPLOYEE_ON_LEAVE),
            Some("Karyawan cuti")
        );
        assert_eq!(
            label(OvertimeCancelReason::FEATURE_DISABLED),
            Some("Fitur lembur dinonaktifkan")
        );
    }

    #[test]
    fn no_snapshot_no_live_is_null() {
        let mut row = make_row();
        row.status = OvertimeRequestStatus::REJECTED;
        row.reject_reason = Some("x".into());
        let e = to_overtime_request_entity(&row, None);
        assert_eq!(e.day_type, None);
        assert_eq!(e.normal_minutes, None);
    }

    #[test]
    fn snapshot_wins_over_live() {
        let mut approved = make_row();
        approved.status = OvertimeRequestStatus::APPROVED;
        approved.day_type = Some("HOLIDAY".into());
        approved.base_normal_sec = Some(0);
        approved.decided_by_id = Some(9);
        approved.decided_by_uuid = Some(person(9));
        approved.decided_by_name = Some("Sari".into());
        approved.decided_at = Some(ts("2026-10-21T09:12:00.000Z"));
        let live = OvertimeLiveDay {
            day_type: OvertimeDayType::Workday,
            normal_sec: 28_800,
        };
        let e = to_overtime_request_entity(&approved, Some(&live));
        assert_eq!(e.day_type, Some(OvertimeDayType::Holiday));
        assert_eq!(e.normal_minutes, Some(0));
        assert_eq!(
            e.decided_by,
            Some(OvertimePersonRef {
                id: person(9),
                name: "Sari".into()
            })
        );

        let mut computed = make_row();
        computed.status = OvertimeRequestStatus::COMPLETED;
        computed.day_type = Some("WORKDAY".into());
        computed.base_normal_sec = Some(28_800);
        computed.normal_sec = Some(14_400);
        assert_eq!(
            to_overtime_request_entity(&computed, Some(&live)).normal_minutes,
            Some(240)
        );
    }

    #[test]
    fn computed_figures() {
        let mut row = make_row();
        row.status = OvertimeRequestStatus::COMPLETED;
        row.day_type = Some("WORKDAY".into());
        row.base_normal_sec = Some(28_800);
        row.normal_sec = Some(28_800);
        row.worked_sec = Some(34_259);
        row.overtime_sec = Some(5_459);
        row.computed_at = Some(ts("2026-10-23T00:00:00.000Z"));
        row.finalized_at = Some(ts("2026-10-30T00:00:00.000Z"));
        let e = to_overtime_request_entity(&row, None);
        assert_eq!(e.worked_minutes, Some(570));
        assert_eq!(e.overtime_minutes, Some(90));
        assert_eq!(e.overtime_seconds, Some(5_459));
        assert!(e.is_final);
    }

    #[test]
    fn daily_limit_minutes() {
        assert_eq!(
            to_overtime_request_entity(&make_row(), None).daily_limit_minutes,
            None
        );
        let mut row = make_row();
        row.daily_limit_sec = Some(14_400);
        assert_eq!(
            to_overtime_request_entity(&row, None).daily_limit_minutes,
            Some(240)
        );
    }

    #[test]
    fn live_day_halves_for_half_leave() {
        let info = DayInfo {
            day_type: OvertimeDayType::Workday,
            base_normal_sec: 28_800,
            daily_limit_sec: 14_400,
            pattern_work_days: 5,
            leave: LeaveCoverage::Half,
        };
        assert_eq!(
            live_day_of(&info),
            OvertimeLiveDay {
                day_type: OvertimeDayType::Workday,
                normal_sec: 14_400
            }
        );
    }
}
