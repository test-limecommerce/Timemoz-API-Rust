//! Bentuk respons (`src/divisions/entities/division.entity.ts`). Urutan field = urutan
//! literal objek yang dibangun `divisions.service.ts` (struct `Serialize` menjaga urutan).

use mz_core::time::JsDate;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Person {
    pub uuid: String,
    pub name: String,
    /// Tabel `users` tidak punya kolom foto: selalu `null`.
    pub avatar_url: Option<String>,
}

impl Person {
    pub fn new(uuid: &str, name: &str) -> Self {
        Self {
            uuid: uuid.to_owned(),
            name: name.to_owned(),
            avatar_url: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListItem {
    pub uuid: String,
    pub name: String,
    pub description: Option<String>,
    pub version: i32,
    pub leader: Option<Person>,
    pub leaders: Vec<Person>,
    pub member_count: usize,
    pub members_preview: Vec<Person>,
    pub tracked_today_seconds: i64,
    pub tracked_week_seconds: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub divisions: i64,
    pub divisions_with_leader: usize,
    pub assigned_employees: i64,
    pub active_employees: i64,
    pub unassigned_employees: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMeta {
    pub time_zone: String,
    pub today: String,
    pub week_start: String,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub data: Vec<ListItem>,
    pub summary: Option<Summary>,
    pub meta: ListMeta,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberEntity {
    pub uuid: String,
    pub name: String,
    pub email: String,
    pub role: String,
    pub joined_at: JsDate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewer {
    pub can_manage: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailEntity {
    pub uuid: String,
    pub name: String,
    pub description: Option<String>,
    pub version: i32,
    pub leader: Option<Person>,
    pub leaders: Vec<Person>,
    pub members: Vec<MemberEntity>,
    pub tracked_today_seconds: i64,
    pub tracked_week_seconds: i64,
    pub viewer: Viewer,
}

#[derive(Debug, Serialize)]
pub struct DetailResponse {
    pub data: DetailEntity,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteEntity {
    pub uuid: String,
    pub removed_members: usize,
    pub leader: Option<Person>,
    pub leaders: Vec<Person>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub data: DeleteEntity,
}

#[derive(Debug, Serialize)]
pub struct EmployeeDivision {
    pub uuid: String,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct Employee {
    pub uuid: String,
    pub name: String,
    pub email: String,
    pub division: Option<EmployeeDivision>,
    pub divisions: Vec<EmployeeDivision>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmployeesMeta {
    pub total: i64,
    pub page_number: i64,
    pub items_per_page: i64,
}

#[derive(Debug, Serialize)]
pub struct EmployeesResponse {
    pub data: Vec<Employee>,
    pub meta: EmployeesMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopeDivisionEntity {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct ScopeEmployee {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub role: String,
}

/// `{ kind }` untuk COMPANY/SELF; DIVISION menambah `division`, `divisions`, `employees`.
#[derive(Debug, Serialize)]
pub struct MyScopeEntity {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub division: Option<ScopeDivisionEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divisions: Option<Vec<ScopeDivisionEntity>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub employees: Option<Vec<ScopeEmployee>>,
}

#[derive(Debug, Serialize)]
pub struct MyScopeResponse {
    pub data: MyScopeEntity,
}

#[derive(Debug, Serialize)]
pub struct TrackingProject {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackingItem {
    pub uuid: String,
    pub name: String,
    pub role: String,
    pub status: &'static str,
    pub current_project: Option<TrackingProject>,
    pub last_activity_at: Option<JsDate>,
    pub tracked_today_seconds: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackingMeta {
    pub time_zone: String,
    pub today: String,
    pub online_window_minutes: i64,
    pub idle_window_minutes: i64,
}

#[derive(Debug, Serialize)]
pub struct TrackingResponse {
    pub data: Vec<TrackingItem>,
    pub meta: TrackingMeta,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRangeEntity {
    pub start_at: String,
    pub end_at: String,
    pub clamped_by_history_limit: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTotals {
    pub total_seconds: i64,
    pub billable_seconds: i64,
    pub non_billable_seconds: i64,
    pub average_per_member_seconds: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportMember {
    pub uuid: String,
    pub name: String,
    pub total_seconds: i64,
    pub billable_seconds: i64,
    pub non_billable_seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct ReportEntity {
    pub range: ReportRangeEntity,
    pub totals: ReportTotals,
    pub members: Vec<ReportMember>,
}

#[derive(Debug, Serialize)]
pub struct ReportResponse {
    pub data: ReportEntity,
}
