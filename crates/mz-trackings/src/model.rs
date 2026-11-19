//! Baris DB dan bentuk JSON-nya (kolom persis `prisma/schema.prisma`).
//!
//! `tracking."takenDate"` bertipe `date`; dibaca sebagai `::timestamp` supaya
//! terserialisasi `2026-11-16T00:00:00.000Z` seperti Prisma. Kolom enum dibaca
//! `::text`, uuid `::text`.

use mz_core::time::JsDate;
use serde::Serialize;
use sqlx::FromRow;

/// Kolom `tracking` (urutan = urutan field model Prisma).
pub const TRACKING_COLS: &str = r#"t.id, t.uuid::text AS uuid, t."takenDate"::timestamp AS "takenDate", t."isAbsent", t."userId", t."totalTakenInSecond", t."createdAt", t."updatedAt", t.status"#;

/// `RETURNING` untuk statement tanpa alias tabel.
pub const TRACKING_RETURNING: &str = r#"id, uuid::text AS uuid, "takenDate"::timestamp AS "takenDate", "isAbsent", "userId", "totalTakenInSecond", "createdAt", "updatedAt", status"#;

/// Satu baris model `tracking` (semua kolom skalar).
#[derive(Debug, Clone, FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub struct TrackingRow {
    pub id: i32,
    pub uuid: String,
    pub taken_date: JsDate,
    pub is_absent: Option<bool>,
    pub user_id: i32,
    pub total_taken_in_second: Option<i32>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
}

/// Timeline untuk `findAll` (kolom `select` di service Node + `taskId` internal).
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "camelCase")]
pub struct ListTimelineRow {
    pub id: i32,
    pub uuid: String,
    pub tracking_id: i32,
    pub note: String,
    pub is_offline: bool,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub start_at_utc: Option<JsDate>,
    pub end_at_utc: Option<JsDate>,
    pub timezone: Option<String>,
    pub offset_minutes: Option<i32>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
    pub clickup_time_entry_id: Option<String>,
    pub jira_worklog_id: Option<String>,
    pub manual_time_status: Option<String>,
    pub manual_time_approval_source: Option<String>,
    pub task_id: Option<i32>,
    pub project_id: i32,
    pub project_uuid: String,
    pub project_code: String,
    pub project_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRef {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotOut {
    pub uuid: String,
    pub screenshot_url: String,
    pub is_blur: bool,
    pub taken_at: JsDate,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskOut {
    pub uuid: String,
    pub name: String,
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,
    #[serde(rename = "externalUrl")]
    pub external_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrationOut {
    pub platform: String,
    pub task: Option<TaskOut>,
}

/// Timeline di respons `findAll`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTimelineOut {
    pub id: i32,
    pub uuid: String,
    pub note: String,
    pub is_offline: bool,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub start_at_utc: Option<JsDate>,
    pub end_at_utc: Option<JsDate>,
    pub timezone: Option<String>,
    pub offset_minutes: Option<i32>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
    pub clickup_time_entry_id: Option<String>,
    pub jira_worklog_id: Option<String>,
    pub manual_time_status: Option<String>,
    pub manual_time_approval_source: Option<String>,
    pub project: ProjectRef,
    pub screenshot: Vec<ScreenshotOut>,
    pub integration: Option<IntegrationOut>,
}

/// Seluruh kolom skalar `trackingTimeline` (respons `findOne`).
#[derive(Debug, Clone, FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub struct FullTimelineRow {
    pub id: i32,
    pub uuid: String,
    pub note: String,
    pub is_offline: bool,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub start_at_utc: Option<JsDate>,
    pub end_at_utc: Option<JsDate>,
    pub offset_minutes: Option<i32>,
    pub timezone: Option<String>,
    pub tracking_id: i32,
    pub project_id: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
    pub manual_time_status: Option<String>,
    pub manual_time_approval_source: Option<String>,
    pub reason: Option<String>,
    pub approved_by_id: Option<i32>,
    pub approved_at: Option<JsDate>,
    pub rejected_reason: Option<String>,
    pub requested_at: Option<JsDate>,
    pub billable: Option<bool>,
    pub clickup_time_entry_id: Option<String>,
    pub jira_worklog_id: Option<String>,
    pub task_id: Option<i32>,
}

pub const FULL_TIMELINE_COLS: &str = r#"tt.id, tt.uuid::text AS uuid, tt.note, tt."isOffline", tt."startAt", tt."endAt", tt."startAtUtc", tt."endAtUtc", tt."offsetMinutes", tt.timezone, tt."trackingId", tt."projectId", tt."createdAt", tt."updatedAt", tt.status, tt."manualTimeStatus"::text AS "manualTimeStatus", tt."manualTimeApprovalSource"::text AS "manualTimeApprovalSource", tt.reason, tt."approvedById", tt."approvedAt", tt."rejectedReason", tt."requestedAt", tt.billable, tt."clickupTimeEntryId", tt."jiraWorklogId", tt."taskId""#;

/// `users` dengan kolom yang dikirim `findAll`.
#[derive(Debug, Clone, FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub struct ListUserRow {
    pub id: i32,
    pub uuid: String,
    pub username: String,
    pub name: String,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub sent_at: Option<JsDate>,
    pub status: i32,
}

/// `TRACKING_USER_SELECT` (`findOne`).
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct TrackingUserRow {
    pub id: i32,
    pub uuid: String,
    pub username: String,
    pub name: String,
    pub status: i32,
}

/// Seluruh kolom skalar `userCompany`.
#[derive(Debug, Clone, FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub struct UserCompanyRow {
    pub id: i32,
    pub uuid: String,
    pub user_id: i32,
    pub company_id: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
    pub is_screenshot_active: bool,
    pub is_allow_manual_time: bool,
    pub is_blur: bool,
    pub is_allow_edit_time: bool,
    pub screenshot_per_hour: i32,
    pub idle_time_in_second: i32,
    pub is_default: bool,
    pub invited_by_id: Option<i32>,
    pub invite_method: Option<String>,
}

/// Kolom `task` yang dibaca untuk blok `integration`.
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "camelCase")]
pub struct TaskRow {
    pub id: i32,
    pub project_id: i32,
    pub uuid: String,
    pub source: String,
    pub name: String,
    pub external_id: Option<String>,
    pub external_url: Option<String>,
}

impl TaskRow {
    pub fn to_out(&self) -> TaskOut {
        TaskOut {
            uuid: self.uuid.clone(),
            name: self.name.clone(),
            external_id: self.external_id.clone(),
            external_url: self.external_url.clone(),
        }
    }
}
