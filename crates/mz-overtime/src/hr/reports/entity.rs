//! Bentuk laporan lembur: `entities/overtime-report.entity.ts`, `entities/weekly-excess.entity.ts`,
//! dan `overtime-report-meta.ts`. Dipakai bersama oleh service, ekspor XLSX, dan generator PDF.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Rujukan karyawan: `id` = `users.uuid`, `email` = `users.username`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmployeeRef {
    pub id: String,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Period {
    pub from: String,
    pub to: String,
}

/// Satu baris pengajuan pada laporan harian (`OvertimeReportDayEntity`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportDay {
    pub request_id: String,
    pub date: String,
    pub day_type: Option<String>,
    pub approved_minutes: i64,
    pub worked_minutes: Option<i64>,
    pub overtime_minutes: Option<i64>,
    pub status: String,
    pub kind: String,
    pub requested_by_name: String,
    pub note: Option<String>,
    pub daily_limit_minutes: Option<i64>,
    pub exceeds_limit: bool,
    pub exceeds_daily_limit: bool,
    pub exceeds_weekly_limit: bool,
    pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportEmployee {
    pub employee: EmployeeRef,
    pub total_overtime_minutes: i64,
    pub total_overtime_seconds: i64,
    pub not_final: bool,
    pub days: Vec<ReportDay>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OvertimeReport {
    pub period: Period,
    pub employees: Vec<ReportEmployee>,
    pub legal_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyExcessWeek {
    pub week_start: String,
    pub week_end: String,
    pub worked_minutes: i64,
    pub normal_minutes: i64,
    pub overtime_minutes: i64,
    pub excess_minutes: i64,
    pub not_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyExcessEmployee {
    pub employee: EmployeeRef,
    pub total_excess_minutes: i64,
    pub weeks: Vec<WeeklyExcessWeek>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeeklyExcessMonth {
    /// `YYYY-MM` dari `weekEnd`.
    pub month: String,
    pub employees: Vec<WeeklyExcessEmployee>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeeklyExcessReport {
    pub months: Vec<WeeklyExcessMonth>,
    pub label: String,
}

/// Filter laporan yang dipakai (baris "Filter" di header ekspor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportFilters {
    /// Status yang dikenal saja; kosong = semua.
    pub status: Vec<String>,
    /// `WORKDAY` | `HOLIDAY`; `None` = semua.
    pub day_type: Option<String>,
    /// `None` = tidak disaring.
    pub exceeds_limit: Option<bool>,
    /// Jumlah karyawan terpilih; 0 = semua.
    pub employee_count: usize,
}

/// Konteks cetak untuk ekspor (`OvertimeReportMeta`).
#[derive(Debug, Clone)]
pub struct ReportMeta {
    pub company_name: String,
    /// Zona IANA company.
    pub time_zone: String,
    /// Nama user pemanggil.
    pub generated_by: String,
    pub generated_at: DateTime<Utc>,
    /// 0 = Minggu … 6 = Sabtu.
    pub workweek_start_day: i32,
    pub filters: ReportFilters,
}
