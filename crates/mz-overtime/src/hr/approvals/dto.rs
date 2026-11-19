//! DTO route approvals lembur (pesan class-validator persis; urutan properti = deklarasi
//! kelas, di dalam satu properti dekorator dari BAWAH ke atas):
//! `dto/list-overtime-approvals-query.dto.ts`, `dto/overtime-approvals-quota-query.dto.ts`,
//! `dto/create-overtime-order.dto.ts`, plus `ParseUUIDPipe({ version: '4' })` untuk `:id`.
//! `RejectOvertimeDto` & `ConfirmOverQuotaDto` dipakai dari `hr::common::dto`.

use chrono::NaiveDate;
use mz_core::extract::Validate;
use serde::Deserialize;
use serde_json::Value;

use crate::hr::common::dto::{
    QVal, check_query_int, date_key_message, is_date_key, is_uuid_v4, js_trim, query_date_key,
    query_get, query_map, query_number, status_filter, validate_optional_int,
};
pub use crate::hr::common::dto::{UUID_V4_MESSAGE, UuidV4Id, parse_uuid_v4_param};
use crate::hr::common::enums::OvertimeRequestStatus;

// ── `ListOvertimeApprovalsQueryDto` ──────────────────────────────────────────

pub const DEFAULT_PAGE: i64 = 1;
pub const DEFAULT_LIMIT: i64 = 20;
pub const MAX_PAGE: i64 = 10_000;
pub const MAX_LIMIT: i64 = 100;

/// Query `GET /v1/overtime/approvals` dan `/history` sesudah validasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListApprovalsQuery {
    /// `None` = tidak diisi (default ada di service).
    pub status: Option<Vec<OvertimeRequestStatus>>,
    /// UUID v4 karyawan, apa adanya.
    pub user_id: Option<String>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub page: i64,
    pub limit: i64,
}

/// `status` (TransformStatusFilter + IsOptional + IsArray + IsEnum each), `userId`
/// (IsOptional + IsUUID('4')), `from`/`to` (IsOptional + IsDateKey), `page`
/// (Type Number + IsOptional + IsInt + Min 1 + Max 10000, default 1), `limit` (… Max 100,
/// default 20).
pub fn parse_list_query(raw: Option<&str>) -> Result<ListApprovalsQuery, Vec<String>> {
    let map = query_map(raw);
    let mut e = Vec::new();

    let status = status_filter(&mut e, "status", query_get(&map, "status"));

    let user_id = match query_get(&map, "userId") {
        None => None,
        Some(QVal::One(s)) if is_uuid_v4(s) => Some(s.clone()),
        Some(_) => {
            e.push("userId must be a UUID".to_owned());
            None
        }
    };

    let from = query_get(&map, "from").and_then(|v| query_date_key(&mut e, "from", Some(v)));
    let to = query_get(&map, "to").and_then(|v| query_date_key(&mut e, "to", Some(v)));

    let page = match query_get(&map, "page") {
        None => Some(DEFAULT_PAGE),
        Some(v) => check_query_int(&mut e, "page", query_number(Some(v)), 1, MAX_PAGE),
    };
    let limit = match query_get(&map, "limit") {
        None => Some(DEFAULT_LIMIT),
        Some(v) => check_query_int(&mut e, "limit", query_number(Some(v)), 1, MAX_LIMIT),
    };

    if !e.is_empty() {
        return Err(e);
    }
    Ok(ListApprovalsQuery {
        status,
        user_id,
        from,
        to,
        page: page.unwrap_or(DEFAULT_PAGE),
        limit: limit.unwrap_or(DEFAULT_LIMIT),
    })
}

// ── `OvertimeApprovalsQuotaQueryDto` ─────────────────────────────────────────

/// `APPROVAL_QUOTA_MAX_USER_IDS`.
pub const MAX_ORDER_USERS: usize = 100;
/// `MIN_ORDER_USERS`.
pub const MIN_ORDER_USERS: usize = 1;
pub const MIN_DURATION_MINUTES: i64 = 1;
pub const MAX_DURATION_MINUTES: i64 = 1440;
pub const REASON_MAX_LENGTH: usize = 500;

/// Query `GET /v1/overtime/approvals/quota` sesudah validasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalsQuotaQuery {
    /// Hasil `splitUserIds` (unik tanpa beda huruf, urutan input).
    pub user_ids: Vec<String>,
    pub date: NaiveDate,
    pub duration_minutes: i64,
}

/// `splitUserIds(value)`: `a,b` / kunci berulang → array uuid unik (tanpa beda huruf,
/// ejaan pertama, urutan input). Potongan kosong tetap ikut (ditolak `@IsUUID`).
pub fn split_user_ids(parts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for part in parts {
        for piece in part.split(',') {
            let id = js_trim(piece);
            let key = id.to_lowercase();
            if !id.is_empty() && seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(id.to_owned());
        }
    }
    out
}

/// `userIds` (Transform splitUserIds + IsArray + ArrayMinSize 1 + ArrayMaxSize 100 +
/// IsUUID('4', each)), `date` (IsDateKey), `durationMinutes` (Type Number + IsInt + Min 1 +
/// Max 1440). Tanpa `@IsOptional`: kunci yang tidak dikirim gagal semua pemeriksanya.
pub fn parse_quota_query(raw: Option<&str>) -> Result<ApprovalsQuotaQuery, Vec<String>> {
    let map = query_map(raw);
    let mut e = Vec::new();

    let user_ids = match query_get(&map, "userIds") {
        // Transform tak dipanggil untuk kunci yang tak ada → undefined.
        None => {
            e.push("each value in userIds must be a UUID".to_owned());
            e.push(format!(
                "userIds must contain no more than {MAX_ORDER_USERS} elements"
            ));
            e.push(format!(
                "userIds must contain at least {MIN_ORDER_USERS} elements"
            ));
            e.push("userIds must be an array".to_owned());
            Vec::new()
        }
        Some(v) => {
            let ids = split_user_ids(&v.clone().into_vec());
            if !ids.iter().all(|id| is_uuid_v4(id)) {
                e.push("each value in userIds must be a UUID".to_owned());
            }
            if ids.len() > MAX_ORDER_USERS {
                e.push(format!(
                    "userIds must contain no more than {MAX_ORDER_USERS} elements"
                ));
            }
            if ids.len() < MIN_ORDER_USERS {
                e.push(format!(
                    "userIds must contain at least {MIN_ORDER_USERS} elements"
                ));
            }
            ids
        }
    };

    let date = query_date_key(&mut e, "date", query_get(&map, "date"));
    let duration = check_query_int(
        &mut e,
        "durationMinutes",
        query_number(query_get(&map, "durationMinutes")),
        MIN_DURATION_MINUTES,
        MAX_DURATION_MINUTES,
    );

    match (date, duration) {
        (Some(date), Some(duration_minutes)) if e.is_empty() => Ok(ApprovalsQuotaQuery {
            user_ids,
            date,
            duration_minutes,
        }),
        _ => Err(e),
    }
}

// ── `CreateOvertimeOrderDto` ─────────────────────────────────────────────────

pub use crate::hr::common::dto::validator_js_length;

/// Body `POST /v1/overtime/approvals/orders`. Field tak dikenal diabaikan (tanpa
/// `whitelist`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrderBody {
    #[serde(default)]
    pub user_ids: Option<Value>,
    #[serde(default)]
    pub date: Option<Value>,
    #[serde(default)]
    pub duration_minutes: Option<Value>,
    #[serde(default)]
    pub reason: Option<Value>,
}

impl CreateOrderBody {
    /// `reason` sesudah `@Transform(trim)` (nilai non-string apa adanya).
    fn trimmed_reason(&self) -> Option<Value> {
        self.reason.as_ref().map(|v| match v.as_str() {
            Some(s) => Value::String(js_trim(s).to_owned()),
            None => v.clone(),
        })
    }

    /// Nilai yang diteruskan ke service (dipanggil sesudah validasi DTO lolos).
    pub fn to_input(&self) -> OrderInput {
        OrderInput {
            user_ids: self.user_ids.clone(),
            date: self.date.clone(),
            duration_minutes: self.duration_minutes.clone(),
            reason: self.trimmed_reason(),
        }
    }
}

impl Validate for CreateOrderBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();

        // userIds: @IsArray @ArrayMinSize(1) @ArrayMaxSize(100) @ArrayUnique @IsUUID('4', each)
        let ids = self.user_ids.as_ref().and_then(Value::as_array);
        let uuid_ok = match (ids, self.user_ids.as_ref()) {
            (Some(items), _) => items.iter().all(|i| i.as_str().is_some_and(is_uuid_v4)),
            (None, Some(v)) => v.as_str().is_some_and(is_uuid_v4),
            (None, None) => false,
        };
        if !uuid_ok {
            e.push("each value in userIds must be a UUID".to_owned());
        }
        let unique = ids.is_some_and(|items| {
            items
                .iter()
                .enumerate()
                .all(|(i, a)| items.iter().skip(i + 1).all(|b| a != b))
        });
        if !unique {
            e.push("All userIds's elements must be unique".to_owned());
        }
        if !ids.is_some_and(|a| a.len() <= MAX_ORDER_USERS) {
            e.push(format!(
                "userIds must contain no more than {MAX_ORDER_USERS} elements"
            ));
        }
        if !ids.is_some_and(|a| a.len() >= MIN_ORDER_USERS) {
            e.push(format!(
                "userIds must contain at least {MIN_ORDER_USERS} elements"
            ));
        }
        if ids.is_none() {
            e.push("userIds must be an array".to_owned());
        }

        // date: @IsDateKey
        if !is_date_key(self.date.as_ref()) {
            e.push(date_key_message("date"));
        }

        // durationMinutes: @IsInt @Min(1) @Max(1440) (tanpa @Type: string ditolak)
        match self.duration_minutes.as_ref() {
            Some(v) => validate_optional_int(
                &mut e,
                "durationMinutes",
                Some(v),
                MIN_DURATION_MINUTES,
                MAX_DURATION_MINUTES,
            ),
            None => {
                e.push(format!(
                    "durationMinutes must not be greater than {MAX_DURATION_MINUTES}"
                ));
                e.push(format!(
                    "durationMinutes must not be less than {MIN_DURATION_MINUTES}"
                ));
                e.push("durationMinutes must be an integer number".to_owned());
            }
        }

        // reason: @Transform(trim) @IsString @IsNotEmpty @MaxLength(500)
        let reason = self.trimmed_reason();
        let s = reason.as_ref().and_then(Value::as_str);
        if s.is_none_or(|s| validator_js_length(s) > REASON_MAX_LENGTH) {
            e.push(format!(
                "reason must be shorter than or equal to {REASON_MAX_LENGTH} characters"
            ));
        }
        if matches!(&reason, None | Some(Value::Null)) || s == Some("") {
            e.push("reason should not be empty".to_owned());
        }
        if s.is_none() {
            e.push("reason must be a string".to_owned());
        }
        e
    }
}

/// `CreateOvertimeOrderDto` apa adanya (sesudah transform) untuk lapis kedua service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderInput {
    pub user_ids: Option<Value>,
    pub date: Option<Value>,
    pub duration_minutes: Option<Value>,
    pub reason: Option<Value>,
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-dto.http.spec.ts` (approvals) tanpa HTTP/DB.
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::zone::parse_date_key;
    use mz_core::ApiError;
    use serde_json::json;

    const U1: &str = "5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02";
    const U2: &str = "9a1f3c52-6b7d-4e8a-8c4f-1d2e3f4a5b03";
    const UUID_V1: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    fn ids(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
            .collect()
    }

    #[test]
    fn uuid_pipe() {
        assert_eq!(parse_uuid_v4_param(U1).ok(), Some(U1.to_owned()));
        assert!(parse_uuid_v4_param(&U1.to_uppercase()).is_ok());
        for bad in ["abc", "123", UUID_V1, "orders", "quota"] {
            let err = parse_uuid_v4_param(bad).err().unwrap();
            assert!(matches!(
                err,
                ApiError::Message { ref message, .. } if message == UUID_V4_MESSAGE
            ));
        }
    }

    #[test]
    fn list_query_defaults_and_full() {
        let q = parse_list_query(None).unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
        assert_eq!(q.status, None);
        let q = parse_list_query(Some(&format!(
            "status=PENDING_APPROVAL&status=APPROVED&userId={U1}&from=2026-10-01&to=2026-10-31&page=3&limit=10"
        )))
        .unwrap();
        assert_eq!(
            q.status,
            Some(vec![
                OvertimeRequestStatus::PENDING_APPROVAL,
                OvertimeRequestStatus::APPROVED
            ])
        );
        assert_eq!(q.user_id.as_deref(), Some(U1));
        assert_eq!(q.from, parse_date_key("2026-10-01"));
        assert_eq!(q.to, parse_date_key("2026-10-31"));
        assert_eq!((q.page, q.limit), (3, 10));
        assert_eq!(parse_list_query(Some("page=10000")).unwrap().page, 10_000);
        assert_eq!(parse_list_query(Some("limit=100")).unwrap().limit, 100);
    }

    #[test]
    fn list_query_invalid() {
        for qs in [
            "status=NOPE".to_owned(),
            "userId=12".to_owned(),
            format!("userId={UUID_V1}"),
            "from=2026/10/01".to_owned(),
            "to=2026-04-31".to_owned(),
            "page=0".to_owned(),
            "page=10001".to_owned(),
            "limit=101".to_owned(),
            "limit=0".to_owned(),
            "limit=x".to_owned(),
            "page=".to_owned(),
            "from=".to_owned(),
        ] {
            assert!(parse_list_query(Some(&qs)).is_err(), "{qs}");
        }
        assert_eq!(
            parse_list_query(Some("limit=x")).err().unwrap(),
            vec![
                "limit must not be greater than 100",
                "limit must not be less than 1",
                "limit must be an integer number"
            ]
        );
        assert_eq!(
            parse_list_query(Some("page=1.5&userId=12&from=x"))
                .err()
                .unwrap(),
            vec![
                "userId must be a UUID",
                "from harus tanggal YYYY-MM-DD yang valid",
                "page must be an integer number"
            ]
        );
    }

    #[test]
    fn quota_query() {
        let q = parse_quota_query(Some(&format!(
            "userIds={U1},%20{},{U2}&userIds={U2}&date=2026-10-24&durationMinutes=180",
            U1.to_uppercase()
        )))
        .unwrap();
        assert_eq!(q.user_ids, vec![U1.to_owned(), U2.to_owned()]);
        assert_eq!(q.duration_minutes, 180);
        assert_eq!(
            parse_quota_query(None).err().unwrap(),
            vec![
                "each value in userIds must be a UUID",
                "userIds must contain no more than 100 elements",
                "userIds must contain at least 1 elements",
                "userIds must be an array",
                "date harus tanggal YYYY-MM-DD yang valid",
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number"
            ]
        );
        assert_eq!(
            parse_quota_query(Some("userIds=&date=2026-10-24&durationMinutes=1"))
                .err()
                .unwrap(),
            vec!["each value in userIds must be a UUID"]
        );
        let hundred = ids(100).join(",");
        assert!(
            parse_quota_query(Some(&format!(
                "userIds={hundred}&date=2026-10-24&durationMinutes=1440"
            )))
            .is_ok()
        );
        let many = ids(101).join(",");
        assert_eq!(
            parse_quota_query(Some(&format!(
                "userIds={many}&date=2026-10-24&durationMinutes=1"
            )))
            .err()
            .unwrap(),
            vec!["userIds must contain no more than 100 elements"]
        );
        for bad in ["0", "1441", "1.5", "x"] {
            assert!(
                parse_quota_query(Some(&format!(
                    "userIds={U1}&date=2026-10-24&durationMinutes={bad}"
                )))
                .is_err()
            );
        }
        assert!(
            parse_quota_query(Some(&format!(
                "userIds={UUID_V1}&date=2026-10-24&durationMinutes=1"
            )))
            .is_err()
        );
    }

    fn order(v: Value) -> CreateOrderBody {
        serde_json::from_value(v).unwrap_or_default()
    }

    fn valid() -> Value {
        json!({"userIds": [U1], "date": "2026-10-22", "durationMinutes": 90, "reason": "Stock opname"})
    }

    fn with(patch: Value) -> Value {
        let mut v = valid();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    #[test]
    fn order_body_valid() {
        assert!(order(valid()).validate().is_empty());
        let body = order(with(json!({"userIds": [U2, U1], "reason": " x "})));
        assert!(body.validate().is_empty());
        assert_eq!(body.to_input().reason, Some(json!("x")));
        assert_eq!(body.to_input().user_ids, Some(json!([U2, U1])));
        assert!(
            order(with(json!({"userIds": ids(100)})))
                .validate()
                .is_empty()
        );
        assert!(
            order(with(json!({"userIds": [U1.to_uppercase()], "extra": 1})))
                .validate()
                .is_empty()
        );
        assert!(
            order(with(
                json!({"durationMinutes": 1440, "reason": "x".repeat(500)})
            ))
            .validate()
            .is_empty()
        );
        // 300 emoji = 300 karakter bagi validator.js (UTF-16: 600).
        assert!(
            order(with(json!({"reason": "😀".repeat(300)})))
                .validate()
                .is_empty()
        );
    }

    #[test]
    fn order_body_invalid() {
        for field in ["userIds", "date", "durationMinutes", "reason"] {
            let mut v = valid();
            v.as_object_mut().unwrap().remove(field);
            assert!(!order(v).validate().is_empty(), "{field}");
        }
        for patch in [
            json!({"userIds": []}),
            json!({"userIds": U1}),
            json!({"userIds": [U1, "abc"]}),
            json!({"userIds": [1]}),
            json!({"userIds": [UUID_V1]}),
            json!({"userIds": [U1, U1]}),
            json!({"userIds": ids(101)}),
            json!({"date": "22-10-2026"}),
            json!({"date": "2026-02-30"}),
            json!({"durationMinutes": 0}),
            json!({"durationMinutes": 1441}),
            json!({"durationMinutes": 1.5}),
            json!({"durationMinutes": "90"}),
            json!({"reason": " "}),
            json!({"reason": "x".repeat(501)}),
        ] {
            assert!(!order(with(patch.clone())).validate().is_empty(), "{patch}");
        }
        assert_eq!(
            order(json!({})).validate(),
            vec![
                "each value in userIds must be a UUID",
                "All userIds's elements must be unique",
                "userIds must contain no more than 100 elements",
                "userIds must contain at least 1 elements",
                "userIds must be an array",
                "date harus tanggal YYYY-MM-DD yang valid",
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number",
                "reason must be shorter than or equal to 500 characters",
                "reason should not be empty",
                "reason must be a string"
            ]
        );
        assert_eq!(
            order(with(json!({"userIds": U1}))).validate(),
            vec![
                "All userIds's elements must be unique",
                "userIds must contain no more than 100 elements",
                "userIds must contain at least 1 elements",
                "userIds must be an array"
            ]
        );
        assert_eq!(
            order(with(json!({"userIds": []}))).validate(),
            vec!["userIds must contain at least 1 elements"]
        );
        assert_eq!(
            order(with(json!({"durationMinutes": "90"}))).validate(),
            vec![
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number"
            ]
        );
    }

    #[test]
    fn validator_length() {
        assert_eq!(validator_js_length("abc"), 3);
        assert_eq!(validator_js_length("😀😀"), 2);
        assert_eq!(validator_js_length("❤\u{FE0F}"), 1);
        assert_eq!(validator_js_length(""), 0);
    }
}
