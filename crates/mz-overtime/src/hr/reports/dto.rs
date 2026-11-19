//! DTO query laporan lembur. Node: `dto/overtime-report-query.dto.ts`,
//! `dto/overtime-report-export-query.dto.ts`, `dto/date-key.validator.ts`,
//! `dto/overtime-request-status-filter.ts`.
//!
//! Query diparse manual dari pasangan kunci/nilai karena Nest/`qs` mengubah kunci
//! berulang menjadi array (`?userIds=A&userIds=B`), sesuatu yang `serde_urlencoded` tidak
//! bisa. Urutan pesan mengikuti `class-validator`: properti kelas turunan (`format`) lebih
//! dulu, lalu `from`, `to`, `userIds`, `status`, `dayType`, `exceedsLimit`; di dalam satu
//! properti, dekorator paling bawah lebih dulu.

use mz_core::ApiError;
pub use mz_core::uuid_rules::is_uuid_v4;

use crate::zone::parse_date_key;

/// Query mentah ala `req.query` — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::ParsedQuery as RawQuery;

use mz_core::qs::QVal;

/// Query laporan yang sudah divalidasi.
#[derive(Debug, Clone, Default)]
pub struct ReportQuery {
    pub from: String,
    pub to: String,
    /// Sesudah `toTrimmedList`; `None` = tidak diisi.
    pub user_ids: Option<Vec<String>>,
    pub status: Option<Vec<String>>,
    pub day_type: Option<String>,
    pub exceeds_limit: Option<bool>,
    /// Hanya route ekspor: `xlsx` | `pdf`.
    pub format: Option<String>,
}

pub const STATUS_VALUES: [&str; 6] = [
    "PENDING_APPROVAL",
    "PENDING_EMPLOYEE",
    "APPROVED",
    "REJECTED",
    "CANCELLED",
    "COMPLETED",
];
pub const DAY_TYPE_VALUES: [&str; 2] = ["WORKDAY", "HOLIDAY"];
pub const MAX_USER_IDS: usize = 500;

/// `@IsDateKey()`: `YYYY-MM-DD` ketat dan ada di kalender.
fn is_date_key(v: Option<&QVal>) -> bool {
    matches!(v, Some(QVal::One(s)) if parse_date_key(s).is_some())
}

/// `toTrimmedList`: kosong -> `None`; array dipakai apa adanya, string dipecah koma;
/// spasi dibuang, elemen kosong dibuang.
fn to_trimmed_list(v: Option<&QVal>) -> Option<Vec<String>> {
    let parts: Vec<String> = match v? {
        QVal::One(s) if s.is_empty() => return None,
        QVal::Many(list) => list.clone(),
        other => other.js_string().split(',').map(str::to_owned).collect(),
    };
    Some(
        parts
            .into_iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

/// Parse + validasi. `export` menambah `format` (`@IsIn(['xlsx','pdf'])`).
pub fn parse_query(map: &[(String, QVal)], export: bool) -> Result<ReportQuery, ApiError> {
    let get = |k: &str| mz_core::qs::get(map, k);
    let mut errors: Vec<String> = Vec::new();
    let mut q = ReportQuery::default();

    if export {
        match get("format") {
            Some(QVal::One(f)) if f == "xlsx" || f == "pdf" => q.format = Some(f.clone()),
            _ => errors.push("format must be one of the following values: xlsx, pdf".into()),
        }
    }

    for (prop, slot) in [("from", &mut q.from), ("to", &mut q.to)] {
        let v = get(prop);
        if is_date_key(v) {
            if let Some(QVal::One(s)) = v {
                slot.clone_from(s);
            }
        } else {
            errors.push(format!("{prop} harus tanggal YYYY-MM-DD yang valid"));
        }
    }

    if let Some(ids) = to_trimmed_list(get("userIds")) {
        if !ids.iter().all(|id| is_uuid_v4(id)) {
            errors.push("each value in userIds must be a UUID".into());
        }
        if ids.len() > MAX_USER_IDS {
            errors.push(format!(
                "userIds must contain no more than {MAX_USER_IDS} elements"
            ));
        }
        q.user_ids = Some(ids);
    }

    if let Some(v) = get("status") {
        let list = v.clone().into_vec();
        if !list.iter().all(|s| STATUS_VALUES.contains(&s.as_str())) {
            errors.push(format!(
                "each value in status must be one of the following values: {}",
                STATUS_VALUES.join(", ")
            ));
        }
        q.status = Some(list);
    }

    if let Some(v) = get("dayType") {
        match v {
            QVal::One(s) if DAY_TYPE_VALUES.contains(&s.as_str()) => q.day_type = Some(s.clone()),
            _ => errors.push(format!(
                "dayType must be one of the following values: {}",
                DAY_TYPE_VALUES.join(", ")
            )),
        }
    }

    if let Some(v) = get("exceedsLimit") {
        match v {
            QVal::One(s) if s == "true" => q.exceeds_limit = Some(true),
            QVal::One(s) if s == "false" => q.exceeds_limit = Some(false),
            _ => errors.push("exceedsLimit must be a boolean value".into()),
        }
    }

    if errors.is_empty() {
        Ok(q)
    } else {
        Err(ApiError::Validation(errors))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    const U1: &str = "9f1c2d3e-4a5b-4c6d-8e9f-0a1b2c3d4e5f";
    const U2: &str = "0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d";
    const UUID_V1: &str = "9f1c2d3e-4a5b-1c6d-8e9f-0a1b2c3d4e5f";

    fn pairs(qs: &str) -> Vec<(String, QVal)> {
        mz_core::qs::parse(qs)
    }

    fn ok(qs: &str) -> ReportQuery {
        parse_query(&pairs(qs), false).unwrap()
    }

    fn messages(qs: &str, export: bool) -> Vec<String> {
        match parse_query(&pairs(qs), export) {
            Err(ApiError::Validation(m)) => m,
            other => panic!("diharapkan 400, dapat {other:?}"),
        }
    }

    const BASE: &str = "from=2026-10-01&to=2026-10-31";

    #[test]
    fn minimal_has_no_other_filters() {
        let q = ok(BASE);
        assert_eq!(
            (q.from.as_str(), q.to.as_str()),
            ("2026-10-01", "2026-10-31")
        );
        assert!(q.user_ids.is_none() && q.status.is_none());
        assert!(q.day_type.is_none() && q.exceeds_limit.is_none());
    }

    #[test]
    fn all_filters_are_transformed() {
        let q = ok(&format!(
            "{BASE}&userIds={U1}&userIds={U2}&status=COMPLETED&dayType=HOLIDAY&exceedsLimit=true"
        ));
        assert_eq!(q.user_ids.unwrap(), [U1, U2]);
        assert_eq!(q.status.unwrap(), ["COMPLETED"]);
        assert_eq!(q.day_type.as_deref(), Some("HOLIDAY"));
        assert_eq!(q.exceeds_limit, Some(true));
    }

    #[test]
    fn comma_list_trims_and_empty_is_unset() {
        assert_eq!(
            ok(&format!("{BASE}&userIds={U1}, {U2}")).user_ids.unwrap(),
            [U1, U2]
        );
        assert!(ok(&format!("{BASE}&userIds=")).user_ids.is_none());
        assert_eq!(ok(&format!("{BASE}&userIds=,")).user_ids.unwrap().len(), 0);
        assert_eq!(
            ok(&format!("{BASE}&exceedsLimit=false")).exceeds_limit,
            Some(false)
        );
    }

    #[test]
    fn invalid_queries_are_rejected() {
        for qs in [
            "to=2026-10-31".to_owned(),
            "from=2026-10-01".to_owned(),
            String::new(),
            "from=2026-10-1&to=2026-10-31".to_owned(),
            "from=2026-10-01&to=2026-02-30".to_owned(),
            "from=2026-10-01&to=2026-10-31T00:00:00Z".to_owned(),
            format!("{BASE}&userIds=abc"),
            format!("{BASE}&userIds={U1},abc"),
            format!("{BASE}&userIds={UUID_V1}"),
            format!("{BASE}&status=DONE"),
            format!("{BASE}&status="),
            format!("{BASE}&dayType=WEEKEND"),
            format!("{BASE}&dayType=workday"),
            format!("{BASE}&dayType="),
            format!("{BASE}&exceedsLimit=yes"),
            format!("{BASE}&exceedsLimit=1"),
            format!("{BASE}&exceedsLimit=TRUE"),
            format!("{BASE}&exceedsLimit="),
            "from=2026-10-01&to=2026-10-31&to=2026-10-30".to_owned(),
        ] {
            assert!(
                parse_query(&pairs(&qs), false).is_err(),
                "harus ditolak: {qs}"
            );
        }
    }

    #[test]
    fn messages_follow_class_validator_wording_and_order() {
        assert_eq!(
            messages("", false),
            [
                "from harus tanggal YYYY-MM-DD yang valid",
                "to harus tanggal YYYY-MM-DD yang valid"
            ]
        );
        assert_eq!(
            messages(
                &format!("{BASE}&userIds=x&status=DONE&dayType=A&exceedsLimit=y"),
                false
            ),
            [
                "each value in userIds must be a UUID",
                "each value in status must be one of the following values: PENDING_APPROVAL, PENDING_EMPLOYEE, APPROVED, REJECTED, CANCELLED, COMPLETED",
                "dayType must be one of the following values: WORKDAY, HOLIDAY",
                "exceedsLimit must be a boolean value"
            ]
        );
    }

    #[test]
    fn max_user_ids_is_500() {
        let ids = |n: usize| {
            (0..n)
                .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            ok(&format!("{BASE}&userIds={}", ids(400)))
                .user_ids
                .unwrap()
                .len(),
            400
        );
        assert_eq!(
            messages(&format!("{BASE}&userIds={}", ids(501)), false),
            ["userIds must contain no more than 500 elements"]
        );
    }

    #[test]
    fn export_requires_format_and_lists_it_first() {
        let q = parse_query(&pairs(&format!("{BASE}&format=pdf")), true).unwrap();
        assert_eq!(q.format.as_deref(), Some("pdf"));
        for qs in [
            BASE.to_owned(),
            format!("{BASE}&format=csv"),
            format!("{BASE}&format=XLSX"),
            format!("{BASE}&format="),
        ] {
            assert_eq!(
                messages(&qs, true),
                ["format must be one of the following values: xlsx, pdf"]
            );
        }
        assert_eq!(
            messages("to=x", true),
            [
                "format must be one of the following values: xlsx, pdf",
                "from harus tanggal YYYY-MM-DD yang valid",
                "to harus tanggal YYYY-MM-DD yang valid"
            ]
        );
    }

    #[test]
    fn uuid_v4_check() {
        assert!(is_uuid_v4(U1));
        assert!(is_uuid_v4(&U1.to_uppercase()));
        assert!(!is_uuid_v4(UUID_V1));
        assert!(!is_uuid_v4("abc"));
    }
}
