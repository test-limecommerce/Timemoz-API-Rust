//! Data contoh untuk unit test ekspor XLSX dan PDF (padanan fixture `*.spec.ts` Node).

use chrono::{TimeZone, Utc};

use super::entity::{
    EmployeeRef, OvertimeReport, Period, ReportDay, ReportEmployee, ReportFilters, ReportMeta,
    WeeklyExcessEmployee, WeeklyExcessMonth, WeeklyExcessReport, WeeklyExcessWeek,
};
use super::pdf::LEGAL_NOTE;

pub const WEEKLY_LABEL: &str = "Melebihi jam kerja mingguan";

pub fn meta() -> ReportMeta {
    ReportMeta {
        company_name: "PT Maju Jaya".into(),
        time_zone: "Asia/Jakarta".into(),
        generated_by: "Rina Admin".into(),
        // 2026-10-25 20:30 UTC = 26 Okt 03:30 WIB (lintas tanggal).
        generated_at: Utc
            .with_ymd_and_hms(2026, 10, 25, 20, 30, 0)
            .single()
            .unwrap_or_default(),
        workweek_start_day: 1,
        filters: ReportFilters::default(),
    }
}

pub fn day() -> ReportDay {
    ReportDay {
        request_id: "r-1".into(),
        date: "2026-10-01".into(),
        day_type: Some("WORKDAY".into()),
        approved_minutes: 120,
        worked_minutes: Some(90),
        overtime_minutes: Some(90),
        status: "COMPLETED".into(),
        kind: "SELF".into(),
        requested_by_name: "Staff Citra".into(),
        note: None,
        daily_limit_minutes: Some(240),
        exceeds_limit: false,
        exceeds_daily_limit: false,
        exceeds_weekly_limit: false,
        is_final: true,
    }
}

pub fn person(name: &str) -> EmployeeRef {
    EmployeeRef {
        id: name.into(),
        name: name.into(),
        email: format!("{}@example.com", name.to_lowercase().replace(' ', ".")),
    }
}

/// Karyawan dengan total dari baris COMPLETED (seperti helper `employee()` spec PDF).
pub fn employee(name: &str, days: Vec<ReportDay>) -> ReportEmployee {
    let total_sec: i64 = days
        .iter()
        .filter(|d| d.status == "COMPLETED")
        .map(|d| d.overtime_minutes.unwrap_or(0) * 60)
        .sum();
    ReportEmployee {
        employee: person(name),
        total_overtime_seconds: total_sec,
        total_overtime_minutes: total_sec / 60,
        not_final: days.iter().any(|d| !d.is_final),
        days,
    }
}

pub fn daily_of(employees: Vec<ReportEmployee>, from: &str, to: &str) -> OvertimeReport {
    OvertimeReport {
        period: Period {
            from: from.into(),
            to: to.into(),
        },
        employees,
        legal_note: LEGAL_NOTE.into(),
    }
}

pub fn weekly_of(months: Vec<WeeklyExcessMonth>) -> WeeklyExcessReport {
    WeeklyExcessReport {
        months,
        label: WEEKLY_LABEL.into(),
    }
}

/// Dua karyawan: Ani (3 baris) dan Budi (1 baris) untuk uji XLSX.
pub fn xlsx_daily() -> OvertimeReport {
    let mut ani = employee(
        "Ani Wijaya",
        vec![
            ReportDay {
                request_id: "r-1".into(),
                ..day()
            },
            ReportDay {
                request_id: "r-2".into(),
                date: "2026-10-02".into(),
                overtime_minutes: Some(180),
                is_final: false,
                ..day()
            },
            ReportDay {
                request_id: "r-3".into(),
                date: "2026-10-03".into(),
                day_type: Some("HOLIDAY".into()),
                approved_minutes: 300,
                worked_minutes: None,
                overtime_minutes: None,
                status: "APPROVED".into(),
                exceeds_limit: true,
                exceeds_daily_limit: true,
                ..day()
            },
        ],
    );
    ani.total_overtime_minutes = 270;
    ani.total_overtime_seconds = 16_200;
    ani.not_final = true;
    let budi = employee(
        "Budi Santoso",
        vec![ReportDay {
            request_id: "r-4".into(),
            date: "2026-10-05".into(),
            overtime_minutes: Some(0),
            ..day()
        }],
    );
    daily_of(vec![ani, budi], "2026-09-26", "2026-10-25")
}

pub fn xlsx_weekly() -> WeeklyExcessReport {
    weekly_of(vec![WeeklyExcessMonth {
        month: "2026-10".into(),
        employees: vec![WeeklyExcessEmployee {
            employee: person("Ani Wijaya"),
            total_excess_minutes: 360,
            weeks: vec![WeeklyExcessWeek {
                week_start: "2026-09-28".into(),
                week_end: "2026-10-04".into(),
                worked_minutes: 3000,
                normal_minutes: 2400,
                overtime_minutes: 240,
                excess_minutes: 360,
                not_final: true,
            }],
        }],
    }])
}
