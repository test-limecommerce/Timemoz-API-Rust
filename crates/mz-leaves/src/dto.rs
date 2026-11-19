//! DTO + `ValidationPipe({ transform: true })` (class-validator/class-transformer).
//!
//! Query dan body dibaca sebagai objek JS (`Map<String, Value>`): query lewat `qs` (nilai
//! tunggal = string, kunci berulang = array), body JSON apa adanya, multipart = string.
//! Pesan per properti berurutan dari dekorator PALING BAWAH (urutan metadata
//! class-validator), properti berurutan seperti di kelas DTO. Field tak dikenal diabaikan.

use mz_core::{ApiError, ApiResult};
use mz_leave::enums::{LeaveDayType, LeaveStatus};
use serde_json::{Map, Value};

/// Objek `req.query` dari query string mentah.
pub fn query_object(raw: Option<&str>) -> Map<String, Value> {
    mz_core::qs::parse(raw.unwrap_or_default())
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                mz_core::qs::QVal::One(s) => Value::String(s),
                mz_core::qs::QVal::Many(items) => {
                    Value::Array(items.into_iter().map(Value::String).collect())
                }
                mz_core::qs::QVal::Obj => Value::Object(Map::new()),
            };
            (k, v)
        })
        .collect()
}

/// Pengumpul pesan validasi.
#[derive(Debug, Default)]
pub struct V {
    pub errors: Vec<String>,
}

/// `undefined` / `null` (dilewati `@IsOptional()`).
fn absent(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// `@Type(() => Number)`: `Number(value)`; array/objek/null tetap bukan angka.
pub fn to_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(mz_core::jsfmt::number(s)),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

impl V {
    pub fn finish(self) -> ApiResult<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ApiError::Validation(self.errors))
        }
    }

    fn push(&mut self, msg: String) {
        self.errors.push(msg);
    }

    /// `@IsInt() [@Min(min)] [@Max(max)] [@IsOptional()] @Type(() => Number)`.
    /// Pesan: Max, Min, IsInt.
    pub fn int(
        &mut self,
        m: &Map<String, Value>,
        key: &str,
        optional: bool,
        min: Option<i64>,
        max: Option<i64>,
    ) -> Option<i64> {
        let raw = m.get(key);
        if optional && absent(raw) {
            return None;
        }
        let n = raw.and_then(to_number).filter(|n| !n.is_nan());
        let before = self.errors.len();
        #[allow(clippy::cast_precision_loss)]
        if let Some(max) = max
            && n.is_none_or(|n| n > max as f64)
        {
            self.push(format!("{key} must not be greater than {max}"));
        }
        #[allow(clippy::cast_precision_loss)]
        if let Some(min) = min
            && n.is_none_or(|n| n < min as f64)
        {
            self.push(format!("{key} must not be less than {min}"));
        }
        let int = n.filter(|n| n.is_finite() && n.fract() == 0.0);
        if int.is_none() {
            self.push(format!("{key} must be an integer number"));
        }
        #[allow(clippy::cast_possible_truncation)]
        int.filter(|_| self.errors.len() == before)
            .map(|n| n as i64)
    }

    /// `@IsNumber() @Min(min) @Max(max) @IsOptional() @Type(() => Number)`. Pesan: Max, Min, IsNumber.
    pub fn number(&mut self, m: &Map<String, Value>, key: &str, min: f64, max: f64) -> Option<f64> {
        let raw = m.get(key);
        if absent(raw) {
            return None;
        }
        let n = raw.and_then(to_number).filter(|n| !n.is_nan());
        let before = self.errors.len();
        if n.is_none_or(|n| n > max) {
            self.push(format!("{key} must not be greater than {}", num(max)));
        }
        if n.is_none_or(|n| n < min) {
            self.push(format!("{key} must not be less than {}", num(min)));
        }
        let finite = n.filter(|n| n.is_finite());
        if finite.is_none() {
            self.push(format!(
                "{key} must be a number conforming to the specified constraints"
            ));
        }
        finite.filter(|_| self.errors.len() == before)
    }

    /// `@IsEnum(E) [@IsOptional()]`.
    pub fn enumeration(
        &mut self,
        m: &Map<String, Value>,
        key: &str,
        optional: bool,
        allowed: &[&str],
    ) -> Option<String> {
        let raw = m.get(key);
        if optional && absent(raw) {
            return None;
        }
        match raw {
            Some(Value::String(s)) if allowed.contains(&s.as_str()) => Some(s.clone()),
            _ => {
                self.push(format!(
                    "{key} must be one of the following values: {}",
                    allowed.join(", ")
                ));
                None
            }
        }
    }

    /// `@IsISO8601()` / `@IsDateString({ strict })` `[@IsOptional()]`.
    pub fn iso_date(
        &mut self,
        m: &Map<String, Value>,
        key: &str,
        optional: bool,
        strict: bool,
    ) -> Option<String> {
        let raw = m.get(key);
        if optional && absent(raw) {
            return None;
        }
        match raw {
            Some(Value::String(s))
                if mz_core::iso8601::is_iso8601(s) && (!strict || strict_calendar(s)) =>
            {
                Some(s.clone())
            }
            _ => {
                self.push(format!("{key} must be a valid ISO 8601 date string"));
                None
            }
        }
    }

    /// `@IsString() [@MaxLength(max)] [@IsNotEmpty()] [@IsOptional()]`.
    /// Pesan: IsNotEmpty, MaxLength, IsString (dekorator dari bawah).
    pub fn string(
        &mut self,
        m: &Map<String, Value>,
        key: &str,
        optional: bool,
        max_len: Option<usize>,
        not_empty: bool,
    ) -> Option<String> {
        let raw = m.get(key);
        if optional && absent(raw) {
            return None;
        }
        let before = self.errors.len();
        if not_empty && (absent(raw) || raw == Some(&Value::String(String::new()))) {
            self.push(format!("{key} should not be empty"));
        }
        let s = match raw {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        if let Some(n) = max_len
            && s.is_none_or(|s| mz_core::extract::validator_js_length(s) > n)
        {
            self.push(format!(
                "{key} must be shorter than or equal to {n} characters"
            ));
        }
        if s.is_none() {
            self.push(format!("{key} must be a string"));
        }
        s.filter(|_| self.errors.len() == before).cloned()
    }
}

fn num(x: f64) -> String {
    mz_core::jsfmt::num_string(x)
}

/// Bagian kalender `isISO8601(.., { strict: true })` — satu implementasi di `mz_core::iso8601`.
pub use mz_core::iso8601::strict_calendar;

fn status_values() -> Vec<&'static str> {
    LeaveStatus::ALL.iter().map(|s| s.as_str()).collect()
}

fn day_type_values() -> Vec<&'static str> {
    LeaveDayType::ALL.iter().map(|s| s.as_str()).collect()
}

fn day_type(s: Option<String>) -> Option<LeaveDayType> {
    s.and_then(|s| s.parse().ok())
}

// ── leaves (dasbor) ────────────────────────────────────────────────────────

/// `LeavesQueryDto`.
#[derive(Debug, Clone, Default)]
pub struct LeavesQuery {
    pub status: Option<String>,
    pub leave_type_id: Option<i64>,
    pub user_id: Option<i64>,
    pub year: Option<i64>,
    pub month: Option<i64>,
    pub page: Option<i64>,
    pub limit: Option<i64>,
}

impl LeavesQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let q = Self {
            status: v.enumeration(m, "status", true, &status_values()),
            leave_type_id: v.int(m, "leaveTypeId", true, None, None),
            user_id: v.int(m, "userId", true, None, None),
            year: v.int(m, "year", true, None, None),
            month: v.int(m, "month", true, None, None),
            page: v.int(m, "page", true, Some(1), None),
            limit: v.int(m, "limit", true, Some(1), None),
        };
        v.finish().map(|()| q)
    }
}

/// `LeaveCalendarQueryDto`.
#[derive(Debug, Clone, Copy)]
pub struct CalendarQuery {
    pub year: i64,
    pub month: i64,
    pub user_id: Option<i64>,
}

impl CalendarQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let year = v.int(m, "year", false, None, None);
        let month = v.int(m, "month", false, Some(1), Some(12));
        let user_id = v.int(m, "userId", true, None, None);
        v.finish()?;
        match (year, month) {
            (Some(year), Some(month)) => Ok(Self {
                year,
                month,
                user_id,
            }),
            _ => Err(ApiError::Validation(Vec::new())),
        }
    }
}

/// `LeaveReportFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Csv,
    Excel,
}

/// `LeaveReportQueryDto`.
#[derive(Debug, Clone)]
pub struct ReportQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub leave_type_id: Option<i64>,
    pub status: Option<String>,
    pub format: ReportFormat,
}

impl ReportQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let start_date = v.iso_date(m, "startDate", true, false);
        let end_date = v.iso_date(m, "endDate", true, false);
        let leave_type_id = v.int(m, "leaveTypeId", true, None, None);
        let status = v.enumeration(m, "status", true, &status_values());
        // `format = EXCEL` (inisialisasi properti) hanya berlaku bila kunci tidak dikirim.
        let format = match m.get("format") {
            None => Some("excel".to_owned()),
            Some(_) => v.enumeration(m, "format", true, &["csv", "excel"]),
        };
        v.finish()?;
        Ok(Self {
            start_date,
            end_date,
            leave_type_id,
            status,
            format: if format.as_deref() == Some("csv") {
                ReportFormat::Csv
            } else {
                ReportFormat::Excel
            },
        })
    }
}

/// `LeaveAnalyticsQueryDto`.
#[derive(Debug, Clone, Copy)]
pub struct AnalyticsQuery {
    pub year: Option<i64>,
    pub user_id: Option<i64>,
    pub workday_hours: Option<f64>,
}

impl AnalyticsQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let q = Self {
            year: v.int(m, "year", true, Some(2000), Some(2100)),
            user_id: v.int(m, "userId", true, Some(1), None),
            // `workdayHours = 8`: bawaan inisialisasi properti (kunci tidak dikirim).
            workday_hours: v.number(m, "workdayHours", 1.0, 24.0),
        };
        v.finish().map(|()| q)
    }
}

/// `LeaveOverviewQueryDto`.
#[derive(Debug, Clone, Copy)]
pub struct OverviewQuery {
    pub year: Option<i64>,
    pub month: Option<i64>,
    pub workday_hours: Option<f64>,
}

impl OverviewQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let q = Self {
            year: v.int(m, "year", true, Some(2000), Some(2100)),
            month: v.int(m, "month", true, Some(1), Some(12)),
            workday_hours: v.number(m, "workdayHours", 1.0, 24.0),
        };
        v.finish().map(|()| q)
    }
}

/// `RejectLeaveDto.rejectionReason` (wajib).
pub fn reject_reason(m: &Map<String, Value>) -> ApiResult<String> {
    let mut v = V::default();
    let r = v.string(m, "rejectionReason", false, Some(1000), false);
    v.finish()?;
    r.ok_or_else(|| ApiError::Validation(Vec::new()))
}

/// `ApproveLeaveDto.approvalNote` / `NeedsRevisionLeaveDto.revisionNote` (opsional).
pub fn optional_note(m: &Map<String, Value>, key: &str) -> ApiResult<Option<String>> {
    let mut v = V::default();
    let r = v.string(m, key, true, Some(1000), false);
    v.finish().map(|()| r)
}

// ── mobile-leaves ──────────────────────────────────────────────────────────

/// `DayCountQueryDto`.
#[derive(Debug, Clone)]
pub struct DayCountQuery {
    pub start_date: String,
    pub end_date: String,
    pub start_day_type: Option<LeaveDayType>,
    pub end_day_type: Option<LeaveDayType>,
}

impl DayCountQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let dt = day_type_values();
        let start_date = v.iso_date(m, "startDate", false, true);
        let end_date = v.iso_date(m, "endDate", false, true);
        let start_day_type = day_type(v.enumeration(m, "startDayType", true, &dt));
        let end_day_type = day_type(v.enumeration(m, "endDayType", true, &dt));
        v.finish()?;
        match (start_date, end_date) {
            (Some(start_date), Some(end_date)) => Ok(Self {
                start_date,
                end_date,
                start_day_type,
                end_day_type,
            }),
            _ => Err(ApiError::Validation(Vec::new())),
        }
    }
}

/// `LeaveRequestQueryDto`.
#[derive(Debug, Clone, Default)]
pub struct MyRequestsQuery {
    pub status: Option<String>,
    pub leave_type_id: Option<i64>,
    pub year: Option<i64>,
    pub page: Option<i64>,
    pub limit: Option<i64>,
}

impl MyRequestsQuery {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let q = Self {
            status: v.enumeration(m, "status", true, &status_values()),
            leave_type_id: v.int(m, "leaveTypeId", true, None, None),
            year: v.int(m, "year", true, None, None),
            page: v.int(m, "page", true, Some(1), None),
            limit: v.int(m, "limit", true, Some(1), None),
        };
        v.finish().map(|()| q)
    }
}

/// `SubmitLeaveRequestDto`.
#[derive(Debug, Clone)]
pub struct SubmitDto {
    pub leave_type_id: i64,
    pub approver_id: i64,
    pub start_date: String,
    pub end_date: String,
    pub start_day_type: Option<LeaveDayType>,
    pub end_day_type: Option<LeaveDayType>,
    pub reason: String,
}

impl SubmitDto {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let dt = day_type_values();
        let leave_type_id = v.int(m, "leaveTypeId", false, Some(1), None);
        let approver_id = v.int(m, "approverId", false, Some(1), None);
        let start_date = v.iso_date(m, "startDate", false, true);
        let end_date = v.iso_date(m, "endDate", false, true);
        let start_day_type = day_type(v.enumeration(m, "startDayType", true, &dt));
        let end_day_type = day_type(v.enumeration(m, "endDayType", true, &dt));
        let reason = v.string(m, "reason", false, None, true);
        v.finish()?;
        match (leave_type_id, approver_id, start_date, end_date, reason) {
            (
                Some(leave_type_id),
                Some(approver_id),
                Some(start_date),
                Some(end_date),
                Some(reason),
            ) => Ok(Self {
                leave_type_id,
                approver_id,
                start_date,
                end_date,
                start_day_type,
                end_day_type,
                reason,
            }),
            _ => Err(ApiError::Validation(Vec::new())),
        }
    }
}

/// Nilai opsional yang membedakan `undefined` (tidak dikirim) dan `null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt<T> {
    Undefined,
    Null,
    Value(T),
}

impl<T: Copy> Opt<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }
}

/// `ReviseLeaveRequestDto`.
#[derive(Debug, Clone)]
pub struct ReviseDto {
    pub leave_type_id: Option<i64>,
    pub approver_id: Opt<i64>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub start_day_type: Option<LeaveDayType>,
    pub end_day_type: Option<LeaveDayType>,
    pub reason: Option<String>,
}

impl ReviseDto {
    pub fn parse(m: &Map<String, Value>) -> ApiResult<Self> {
        let mut v = V::default();
        let dt = day_type_values();
        let leave_type_id = v.int(m, "leaveTypeId", true, Some(1), None);
        let approver = v.int(m, "approverId", true, Some(1), None);
        let approver_id = match (m.get("approverId"), approver) {
            (None, _) => Opt::Undefined,
            (Some(Value::Null), _) => Opt::Null,
            (_, Some(a)) => Opt::Value(a),
            (_, None) => Opt::Undefined,
        };
        let start_date = v.iso_date(m, "startDate", true, true);
        let end_date = v.iso_date(m, "endDate", true, true);
        let start_day_type = day_type(v.enumeration(m, "startDayType", true, &dt));
        let end_day_type = day_type(v.enumeration(m, "endDayType", true, &dt));
        let reason = v.string(m, "reason", true, None, false);
        v.finish()?;
        Ok(Self {
            leave_type_id,
            approver_id,
            start_date,
            end_date,
            start_day_type,
            end_day_type,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn errs<T: std::fmt::Debug>(r: ApiResult<T>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(e)) => e,
            other => vec![format!("{other:?}")],
        }
    }

    #[test]
    fn query_ints_and_enums() {
        let q = LeavesQuery::parse(&query_object(Some("status=PENDING&page=2&limit=5&userId=")))
            .ok()
            .unwrap_or_default();
        assert_eq!(q.status.as_deref(), Some("PENDING"));
        assert_eq!((q.page, q.limit, q.user_id), (Some(2), Some(5), Some(0)));
        assert_eq!(
            errs(LeavesQuery::parse(&query_object(Some(
                "status=X&page=0&year=1.5"
            )))),
            vec![
                "status must be one of the following values: PENDING, APPROVED, REJECTED, NEEDS_REVISION",
                "year must be an integer number",
                "page must not be less than 1",
            ]
        );
    }

    #[test]
    fn calendar_requires_year_month() {
        assert_eq!(
            errs(CalendarQuery::parse(&query_object(None))),
            vec![
                "year must be an integer number",
                "month must not be greater than 12",
                "month must not be less than 1",
                "month must be an integer number",
            ]
        );
    }

    #[test]
    fn report_format_default_and_iso() {
        let q = ReportQuery::parse(&query_object(Some("startDate=2026-01-01"))).ok();
        assert_eq!(q.map(|q| q.format), Some(ReportFormat::Excel));
        assert_eq!(
            errs(ReportQuery::parse(&query_object(Some(
                "endDate=x&format=pdf"
            )))),
            vec![
                "endDate must be a valid ISO 8601 date string",
                "format must be one of the following values: csv, excel",
            ]
        );
    }

    #[test]
    fn analytics_ranges() {
        assert_eq!(
            errs(AnalyticsQuery::parse(&query_object(Some(
                "year=1999&workdayHours=abc"
            )))),
            vec![
                "year must not be less than 2000",
                "workdayHours must not be greater than 24",
                "workdayHours must not be less than 1",
                "workdayHours must be a number conforming to the specified constraints",
            ]
        );
        let q = AnalyticsQuery::parse(&query_object(Some("workdayHours=7.5"))).ok();
        assert_eq!(q.and_then(|q| q.workday_hours), Some(7.5));
    }

    #[test]
    fn review_dtos() {
        assert_eq!(
            errs(reject_reason(&obj(json!({})))),
            vec![
                "rejectionReason must be shorter than or equal to 1000 characters",
                "rejectionReason must be a string",
            ]
        );
        assert_eq!(
            errs(optional_note(
                &obj(json!({ "approvalNote": "x".repeat(1001) })),
                "approvalNote"
            )),
            vec!["approvalNote must be shorter than or equal to 1000 characters"]
        );
        assert_eq!(
            optional_note(&obj(json!({ "approvalNote": null })), "approvalNote").ok(),
            Some(None)
        );
    }

    #[test]
    fn submit_dto_messages() {
        assert_eq!(
            errs(SubmitDto::parse(&obj(
                json!({ "startDate": "2026-02-30", "reason": "" })
            ))),
            vec![
                "leaveTypeId must not be less than 1",
                "leaveTypeId must be an integer number",
                "approverId must not be less than 1",
                "approverId must be an integer number",
                "startDate must be a valid ISO 8601 date string",
                "endDate must be a valid ISO 8601 date string",
                "reason should not be empty",
            ]
        );
        let ok = SubmitDto::parse(&obj(json!({
            "leaveTypeId": "3", "approverId": 4, "startDate": "2026-10-05",
            "endDate": "2026-10-06T00:00:00Z", "startDayType": "HALF_DAY_MORNING", "reason": "r"
        })))
        .ok();
        assert_eq!(ok.as_ref().map(|d| d.leave_type_id), Some(3));
        assert_eq!(
            ok.and_then(|d| d.start_day_type),
            Some(LeaveDayType::HALF_DAY_MORNING)
        );
    }

    #[test]
    fn revise_distinguishes_null() {
        let d = ReviseDto::parse(&obj(json!({ "approverId": null }))).ok();
        assert_eq!(d.map(|d| d.approver_id), Some(Opt::Null));
        let d = ReviseDto::parse(&obj(json!({}))).ok();
        assert_eq!(d.map(|d| d.approver_id), Some(Opt::Undefined));
        assert_eq!(
            errs(ReviseDto::parse(&obj(
                json!({ "reason": 5, "endDayType": "X" })
            ))),
            vec![
                "endDayType must be one of the following values: FULL_DAY, HALF_DAY_MORNING, HALF_DAY_AFTERNOON",
                "reason must be a string",
            ]
        );
    }

    #[test]
    fn strict_dates() {
        assert!(strict_calendar("2026-02-28"));
        assert!(!strict_calendar("2026-02-30"));
        assert!(strict_calendar("2024-366"));
        assert!(!strict_calendar("2026-366"));
    }
}
