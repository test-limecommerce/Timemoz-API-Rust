//! DTO route pengajuan lembur karyawan (bunyi pesan class-validator persis):
//! `dto/create-overtime-request.dto.ts`, `dto/list-my-overtime-requests-query.dto.ts`,
//! `dto/overtime-quota-query.dto.ts`, plus `ParseUUIDPipe({ version: '4' })` untuk `:id`.
//!
//! Urutan pesan: properti sesuai urutan deklarasi kelas; dalam satu properti, dekorator
//! dari BAWAH ke atas (urutan registrasi metadata class-validator).

use chrono::NaiveDate;
use mz_core::extract::Validate;
use serde::Deserialize;
use serde_json::Value;

use crate::hr::common::dto::{
    RejectOvertimeDto, as_js_integer, check_query_int, date_key_message, is_date_key, js_trim,
    query_date_key, query_get, query_map, query_number, status_filter, validate_optional_bool,
    validate_optional_int,
};
pub use crate::hr::common::dto::{UUID_V4_MESSAGE, UuidV4Id, parse_uuid_v4_param};
use crate::hr::common::enums::OvertimeRequestStatus;

// ── `ListMyOvertimeRequestsQueryDto` ─────────────────────────────────────────

/// Bawaan `page` (`page?: number = 1`).
pub const DEFAULT_PAGE: i64 = 1;
/// Bawaan `limit` (`DEFAULT_PAGE_LIMIT`).
pub const DEFAULT_PAGE_LIMIT: i64 = 20;
/// Batas atas `limit` (`MAX_PAGE_LIMIT`).
pub const MAX_PAGE_LIMIT: i64 = 100;
/// Batas atas `page` (`@Max(10000)`).
pub const MAX_PAGE: i64 = 10_000;

/// Query `GET /v1/overtime/requests/me` sesudah transform + validasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListMineQuery {
    /// Kosong = semua status.
    pub statuses: Vec<OvertimeRequestStatus>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub page: i64,
    pub limit: i64,
}

/// `TransformStatusFilter @IsOptional @IsArray @IsEnum(each) status`,
/// `@IsOptional @IsDateKey from/to`, `@Type(Number) @IsOptional @IsInt @Min(1) @Max(10000)
/// page`, `… @Max(100) limit`. `@IsOptional` hanya melewati nilai yang tidak dikirim
/// (`?from=` = string kosong → tetap divalidasi).
pub fn parse_list_mine_query(raw: Option<&str>) -> Result<ListMineQuery, Vec<String>> {
    let map = query_map(raw);
    let mut e = Vec::new();
    let statuses = status_filter(&mut e, "status", query_get(&map, "status")).unwrap_or_default();
    let from = query_get(&map, "from").and_then(|v| query_date_key(&mut e, "from", Some(v)));
    let to = query_get(&map, "to").and_then(|v| query_date_key(&mut e, "to", Some(v)));
    let page = match query_get(&map, "page") {
        Some(v) => check_query_int(&mut e, "page", query_number(Some(v)), 1, MAX_PAGE),
        None => Some(DEFAULT_PAGE),
    };
    let limit = match query_get(&map, "limit") {
        Some(v) => check_query_int(&mut e, "limit", query_number(Some(v)), 1, MAX_PAGE_LIMIT),
        None => Some(DEFAULT_PAGE_LIMIT),
    };
    if !e.is_empty() {
        return Err(e);
    }
    Ok(ListMineQuery {
        statuses,
        from,
        to,
        // Lapis kedua service: bukan bilangan bulat ≥ 1 → bawaan; limit dibatasi 100.
        page: page.filter(|p| *p >= 1).unwrap_or(DEFAULT_PAGE),
        limit: limit
            .filter(|l| *l >= 1)
            .map_or(DEFAULT_PAGE_LIMIT, |l| l.min(MAX_PAGE_LIMIT)),
    })
}

// ── `OvertimeQuotaQueryDto` ──────────────────────────────────────────────────

/// Batas `durationMinutes` (`MIN_DURATION_MINUTES`/`MAX_DURATION_MINUTES`).
pub const MIN_DURATION_MINUTES: i64 = 1;
pub const MAX_DURATION_MINUTES: i64 = 1440;
/// `REASON_MAX_LENGTH`.
pub const REASON_MAX_LENGTH: usize = 500;

/// Query `GET /v1/overtime/requests/quota` sesudah validasi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaQuery {
    pub date: NaiveDate,
    pub duration_minutes: i64,
}

/// `@IsDateKey date`, `@Type(Number) @IsInt @Min(1) @Max(1440) durationMinutes` (wajib).
pub fn parse_quota_query(raw: Option<&str>) -> Result<QuotaQuery, Vec<String>> {
    let map = query_map(raw);
    let mut e = Vec::new();
    let date = query_date_key(&mut e, "date", query_get(&map, "date"));
    let duration = check_query_int(
        &mut e,
        "durationMinutes",
        query_number(query_get(&map, "durationMinutes")),
        MIN_DURATION_MINUTES,
        MAX_DURATION_MINUTES,
    );
    match (date, duration) {
        (Some(date), Some(duration_minutes)) if e.is_empty() => Ok(QuotaQuery {
            date,
            duration_minutes,
        }),
        _ => Err(e),
    }
}

// ── `CreateOvertimeRequestDto` ───────────────────────────────────────────────

/// Body `POST /v1/overtime/requests`. Field tak dikenal diabaikan (ValidationPipe tanpa
/// `whitelist`). Body bukan objek diperlakukan sebagai `{}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct CreateOvertimeRequestBody(pub Value);

/// Body yang sudah lolos validasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOvertimeRequestInput {
    pub date: NaiveDate,
    pub duration_minutes: i64,
    /// Sudah di-trim (`@Transform`).
    pub reason: String,
    pub confirm_over_quota: Option<bool>,
}

impl CreateOvertimeRequestBody {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.as_object().and_then(|m| m.get(key))
    }

    /// Nilai masukan service (bermakna sesudah validasi lolos).
    pub fn to_input(&self) -> Option<CreateOvertimeRequestInput> {
        Some(CreateOvertimeRequestInput {
            date: self
                .get("date")
                .and_then(Value::as_str)
                .and_then(crate::zone::parse_date_key)?,
            duration_minutes: self.get("durationMinutes").and_then(as_js_integer)?,
            reason: self
                .get("reason")
                .and_then(Value::as_str)
                .map(|s| js_trim(s).to_owned())?,
            confirm_over_quota: self.get("confirmOverQuota").and_then(Value::as_bool),
        })
    }
}

impl Validate for CreateOvertimeRequestBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        // date: @IsDateKey()
        if !is_date_key(self.get("date")) {
            e.push(date_key_message("date"));
        }
        // durationMinutes: @IsInt() @Min(1) @Max(1440) — wajib (undefined ikut divalidasi).
        validate_optional_int(
            &mut e,
            "durationMinutes",
            Some(self.get("durationMinutes").unwrap_or(&Value::Null)),
            MIN_DURATION_MINUTES,
            MAX_DURATION_MINUTES,
        );
        // reason: @Transform(trim) @IsString() @IsNotEmpty() @MaxLength(500) — dekorator
        // identik dengan `RejectOvertimeDto`, jadi pesannya dipakai ulang.
        e.extend(
            RejectOvertimeDto {
                reason: self.get("reason").cloned(),
            }
            .validate(),
        );
        // confirmOverQuota: @ValidateIf(v !== undefined) @IsBoolean() — null ditolak.
        validate_optional_bool(&mut e, "confirmOverQuota", self.get("confirmOverQuota"));
        e
    }
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-dto.http.spec.ts` untuk route requests (tanpa HTTP/DB).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> CreateOvertimeRequestBody {
        CreateOvertimeRequestBody(v)
    }

    fn valid() -> Value {
        json!({"date": "2026-10-21", "durationMinutes": 120, "reason": "Closing bulanan"})
    }

    fn with(patch: Value) -> Value {
        let mut v = valid();
        for (k, x) in patch.as_object().cloned().unwrap_or_default() {
            v[k] = x;
        }
        v
    }

    #[test]
    fn create_body_valid() {
        let b = body(valid());
        assert!(b.validate().is_empty());
        assert_eq!(
            b.to_input(),
            Some(CreateOvertimeRequestInput {
                date: NaiveDate::from_ymd_opt(2026, 10, 21).unwrap(),
                duration_minutes: 120,
                reason: "Closing bulanan".into(),
                confirm_over_quota: None,
            })
        );
        // reason di-trim; confirmOverQuota boolean diteruskan apa adanya.
        let b = body(with(
            json!({"reason": "  Closing  ", "confirmOverQuota": false}),
        ));
        assert!(b.validate().is_empty());
        let input = b.to_input().unwrap();
        assert_eq!(input.reason, "Closing");
        assert_eq!(input.confirm_over_quota, Some(false));
        // Batas + field tak dikenal lolos.
        for ok in [
            json!({"durationMinutes": 1}),
            json!({"durationMinutes": 1440, "reason": "x".repeat(500)}),
            json!({"employeeId": 99, "kind": "ORDER"}),
            json!({"durationMinutes": 120.0}),
        ] {
            assert!(body(with(ok.clone())).validate().is_empty(), "{ok}");
        }
    }

    #[test]
    fn create_body_invalid() {
        for field in ["date", "durationMinutes", "reason"] {
            let mut v = valid();
            v.as_object_mut().unwrap().remove(field);
            assert!(!body(v).validate().is_empty(), "{field}");
        }
        for bad in [
            json!({"date": "2026-1-5"}),
            json!({"date": "20261021"}),
            json!({"date": "2026-02-30"}),
            json!({"date": "2026-10-21T00:00:00Z"}),
            json!({"date": 20261021}),
            json!({"durationMinutes": 0}),
            json!({"durationMinutes": 1441}),
            json!({"durationMinutes": -5}),
            json!({"durationMinutes": 90.5}),
            json!({"durationMinutes": "120"}),
            json!({"reason": ""}),
            json!({"reason": "   "}),
            json!({"reason": "x".repeat(501)}),
            json!({"reason": 123}),
            json!({"confirmOverQuota": "true"}),
            json!({"confirmOverQuota": 1}),
            json!({"confirmOverQuota": null}),
        ] {
            assert!(!body(with(bad.clone())).validate().is_empty(), "{bad}");
        }
        assert_eq!(
            body(json!({})).validate(),
            vec![
                "date harus tanggal YYYY-MM-DD yang valid",
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number",
                "reason must be shorter than or equal to 500 characters",
                "reason should not be empty",
                "reason must be a string",
            ]
        );
        assert_eq!(
            body(with(json!({"durationMinutes": "120"}))).validate(),
            vec![
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number",
            ]
        );
        assert_eq!(
            body(with(
                json!({"durationMinutes": 0, "confirmOverQuota": null})
            ))
            .validate(),
            vec![
                "durationMinutes must not be less than 1",
                "confirmOverQuota must be a boolean value",
            ]
        );
        // Body bukan objek = `{}`.
        assert_eq!(body(json!([1])).validate().len(), 7);
    }

    #[test]
    fn list_mine_query() {
        let q = parse_list_mine_query(None).unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
        assert!(q.statuses.is_empty());
        let q = parse_list_mine_query(Some(
            "status=APPROVED&from=2026-10-01&to=2026-10-31&page=2&limit=50",
        ))
        .unwrap();
        assert_eq!(q.statuses, vec![OvertimeRequestStatus::APPROVED]);
        assert_eq!(q.from, NaiveDate::from_ymd_opt(2026, 10, 1));
        assert_eq!(q.to, NaiveDate::from_ymd_opt(2026, 10, 31));
        assert_eq!((q.page, q.limit), (2, 50));
        let q = parse_list_mine_query(Some("status=APPROVED&status=REJECTED")).unwrap();
        assert_eq!(
            q.statuses,
            vec![
                OvertimeRequestStatus::APPROVED,
                OvertimeRequestStatus::REJECTED
            ]
        );
        for ok in [
            "limit=1",
            "limit=100",
            "page=10000",
            "page=%202%20",
            "page=2.0",
        ] {
            assert!(parse_list_mine_query(Some(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "status=DONE",
            "status=APPROVED&status=DONE",
            "status=approved",
            "status=",
            "from=2026-1-5",
            "to=2026-02-30",
            "from=2026-10-01&from=2026-10-02",
            "page=0",
            "page=1.5",
            "page=abc",
            "limit=0",
            "limit=101",
            "limit=abc",
            "page=10001",
            "from=",
        ] {
            assert!(parse_list_mine_query(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse_list_mine_query(Some("page=abc&limit=101&to=x")),
            Err(vec![
                "to harus tanggal YYYY-MM-DD yang valid".to_owned(),
                "page must not be greater than 10000".to_owned(),
                "page must not be less than 1".to_owned(),
                "page must be an integer number".to_owned(),
                "limit must not be greater than 100".to_owned(),
            ])
        );
        // `?page=` = Number('') = 0.
        assert_eq!(
            parse_list_mine_query(Some("page=")),
            Err(vec!["page must not be less than 1".to_owned()])
        );
    }

    #[test]
    fn quota_query() {
        assert_eq!(
            parse_quota_query(Some("date=2026-10-22&durationMinutes=180")),
            Ok(QuotaQuery {
                date: NaiveDate::from_ymd_opt(2026, 10, 22).unwrap(),
                duration_minutes: 180
            })
        );
        for ok in [
            "date=2026-10-22&durationMinutes=1",
            "date=2026-10-22&durationMinutes=1440",
        ] {
            assert!(parse_quota_query(Some(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "durationMinutes=60",
            "date=2026-10-22",
            "",
            "date=22-10-2026&durationMinutes=60",
            "date=2026-13-01&durationMinutes=60",
            "date=2026-10-22&durationMinutes=0",
            "date=2026-10-22&durationMinutes=1441",
            "date=2026-10-22&durationMinutes=1.5",
            "date=2026-10-22&durationMinutes=abc",
            "date=2026-10-22&durationMinutes=",
        ] {
            assert!(parse_quota_query(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse_quota_query(None),
            Err(vec![
                "date harus tanggal YYYY-MM-DD yang valid".to_owned(),
                "durationMinutes must not be greater than 1440".to_owned(),
                "durationMinutes must not be less than 1".to_owned(),
                "durationMinutes must be an integer number".to_owned(),
            ])
        );
        assert_eq!(
            parse_quota_query(Some("date=2026-10-22&durationMinutes=")),
            Err(vec!["durationMinutes must not be less than 1".to_owned()])
        );
    }

    #[test]
    fn id_param_requires_uuid_v4() {
        assert!(parse_uuid_v4_param("5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02").is_ok());
        for bad in [
            "not-a-uuid",
            "123",
            "abc",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        ] {
            assert!(parse_uuid_v4_param(bad).is_err(), "{bad}");
        }
    }
}
