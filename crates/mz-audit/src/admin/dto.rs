//! DTO query §6.1/§6.3/§6.5 — meniru `ListAuditLogsQueryDto`, `RecycleBinQueryDto`,
//! `EntityAuditLogsQueryDto` (class-transformer `@Transform` + class-validator) di atas
//! parser query `qs` Express (`a=1&a=2`, `a[]=1`, `a[0]=1`).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mz_core::ApiError;
pub(crate) use mz_core::iso8601::{DateArg, parse_date_arg};
use mz_core::qs::{QVal, get};
use mz_core::uuid_rules::is_uuid;

use crate::scope::{Action, EntityType};

/// `req.query` Express (`qs`, lihat `mz_core::qs`).
pub(crate) struct RawQuery(pub Vec<(String, QVal)>);

impl<S: Send + Sync> FromRequestParts<S> for RawQuery {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(mz_core::qs::parse(
            parts.uri.query().unwrap_or_default(),
        )))
    }
}

/// `isBlank(value)`: `undefined`/`null`/`''`.
fn is_blank(v: &QVal) -> bool {
    matches!(v, QVal::One(s) if s.is_empty())
}

/// `toArray`: kosong → `undefined`; array apa adanya; selain itu `String(v).split(',')`.
fn to_array(v: Option<&QVal>) -> Option<Vec<String>> {
    match v {
        None => None,
        Some(v) if is_blank(v) => None,
        Some(QVal::Many(list)) => Some(list.clone()),
        Some(v) => Some(v.js_string().split(',').map(str::to_owned).collect()),
    }
}

/// `parseInt(String(value), 10)`; NaN → `f64::NAN`.
fn parse_int(s: &str) -> f64 {
    let t = s.trim_start();
    let mut chars = t.char_indices().peekable();
    let mut end = 0;
    if let Some((_, c)) = chars.peek()
        && (*c == '-' || *c == '+')
    {
        chars.next();
        end = 1;
    }
    let digits_start = end;
    for (i, c) in chars {
        if c.is_ascii_digit() {
            end = i + 1;
        } else {
            break;
        }
    }
    if end == digits_start {
        return f64::NAN;
    }
    t.get(..end)
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

/// `toInt(params, fallback)`.
fn to_int(v: Option<&QVal>, fallback: f64) -> f64 {
    match v {
        None => fallback,
        Some(v) if is_blank(v) => fallback,
        Some(v) => parse_int(&v.js_string()),
    }
}

fn push_enum_error(errors: &mut Vec<String>, field: &str, all: &[&str]) {
    errors.push(format!(
        "each value in {field} must be one of the following values: {}",
        all.join(", ")
    ));
}

fn enum_values<T: Copy>(all: &[T], name: fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|v| name(*v)).collect()
}

fn enum_list(
    map: &[(String, QVal)],
    field: &str,
    all: &[&str],
    errors: &mut Vec<String>,
) -> Option<Vec<String>> {
    let list = to_array(get(map, field))?;
    if list.iter().any(|v| !all.contains(&v.as_str())) {
        push_enum_error(errors, field, all);
    }
    Some(list)
}

fn validate_int(errors: &mut Vec<String>, field: &str, value: f64, min: f64, max: Option<f64>) {
    // Urutan = dekorator dari bawah ke atas: Max, Min, IsInt.
    if let Some(max) = max
        && (value.is_nan() || value > max)
    {
        errors.push(format!("{field} must not be greater than {max}"));
    }
    if value.is_nan() || value < min {
        errors.push(format!("{field} must not be less than {min}"));
    }
    if !(value.is_finite() && value.fract() == 0.0) {
        errors.push(format!("{field} must be an integer number"));
    }
}

fn validate_search(errors: &mut Vec<String>, value: Option<&QVal>) -> Option<String> {
    let v = value?;
    match v {
        QVal::One(s) => {
            if s.chars().count() > 160 {
                errors.push("search must be shorter than or equal to 160 characters".to_owned());
            }
            Some(s.clone())
        }
        QVal::Many(_) | QVal::Obj => {
            errors.push("search must be shorter than or equal to 160 characters".to_owned());
            errors.push("search must be a string".to_owned());
            None
        }
    }
}

fn date_arg(errors: &mut Vec<String>, field: &str, value: Option<&QVal>) -> Option<DateArg> {
    let v = value?;
    let parsed = match v {
        QVal::One(s) => parse_date_arg(s),
        QVal::Many(_) | QVal::Obj => None,
    };
    if parsed.is_none() {
        errors.push(format!("{field} must be a valid ISO 8601 date string"));
    }
    parsed
}

/// Paginasi (`pageNumber`, `itemsPerPage`): `Math.min(itemsPerPage, 100)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Paging {
    pub page_number: i64,
    pub items_per_page: i64,
}

#[allow(clippy::cast_possible_truncation)]
fn paging(map: &[(String, QVal)], errors: &mut Vec<String>) -> (f64, f64) {
    let page = to_int(get(map, "pageNumber"), 1.0);
    let per = to_int(get(map, "itemsPerPage"), 20.0);
    validate_int(errors, "pageNumber", page, 1.0, None);
    validate_int(errors, "itemsPerPage", per, 1.0, Some(100.0));
    (page, per)
}

#[allow(clippy::cast_possible_truncation)]
fn finish_paging(page: f64, per: f64) -> Paging {
    Paging {
        page_number: page.clamp(1.0, 9.0e15) as i64,
        items_per_page: per.clamp(1.0, 100.0) as i64,
    }
}

/// `ListAuditLogsQueryDto` tervalidasi.
#[derive(Debug, Clone)]
pub(crate) struct ListQuery {
    pub entity_type: Option<Vec<String>>,
    pub entity_uuid: Option<String>,
    pub action: Option<Vec<String>>,
    pub actor_id: Option<Vec<String>>,
    pub date_from: Option<DateArg>,
    pub date_to: Option<DateArg>,
    pub search: Option<String>,
    pub paging: Paging,
}

fn entity_names() -> Vec<&'static str> {
    enum_values(EntityType::ALL, EntityType::as_str)
}

fn action_names() -> Vec<&'static str> {
    enum_values(Action::ALL, Action::as_str)
}

impl ListQuery {
    /// Pesan galat berurutan seperti `ValidationPipe`: properti sesuai urutan deklarasi.
    pub(crate) fn parse(map: &[(String, QVal)]) -> Result<Self, ApiError> {
        let mut errors = Vec::new();

        let entity_type = enum_list(map, "entityType", &entity_names(), &mut errors);
        let entity_uuid = match get(map, "entityUuid") {
            None => None,
            Some(QVal::One(s)) if is_uuid(s) => Some(s.clone()),
            Some(_) => {
                errors.push("entityUuid must be a UUID".to_owned());
                None
            }
        };
        let action = enum_list(map, "action", &action_names(), &mut errors);
        let actor_id = to_array(get(map, "actorId"));
        if let Some(list) = &actor_id
            && list.iter().any(|u| !is_uuid(u))
        {
            errors.push("each value in actorId must be a UUID".to_owned());
        }
        let date_from = date_arg(&mut errors, "dateFrom", get(map, "dateFrom"));
        let date_to = date_arg(&mut errors, "dateTo", get(map, "dateTo"));
        let search = validate_search(&mut errors, get(map, "search"));
        let (page, per) = paging(map, &mut errors);

        if !errors.is_empty() {
            return Err(ApiError::Validation(errors));
        }
        Ok(Self {
            entity_type,
            entity_uuid,
            action,
            actor_id,
            date_from,
            date_to,
            search,
            paging: finish_paging(page, per),
        })
    }
}

/// `RecycleBinQueryDto` tervalidasi.
#[derive(Debug, Clone)]
pub(crate) struct BinQuery {
    pub entity_type: Option<Vec<String>>,
    pub search: Option<String>,
    pub paging: Paging,
}

impl BinQuery {
    pub(crate) fn parse(map: &[(String, QVal)]) -> Result<Self, ApiError> {
        let mut errors = Vec::new();
        let entity_type = enum_list(map, "entityType", &entity_names(), &mut errors);
        let search = validate_search(&mut errors, get(map, "search"));
        let (page, per) = paging(map, &mut errors);
        if !errors.is_empty() {
            return Err(ApiError::Validation(errors));
        }
        Ok(Self {
            entity_type,
            search,
            paging: finish_paging(page, per),
        })
    }
}

/// `EntityAuditLogsQueryDto` tervalidasi.
pub(crate) fn parse_entity_query(map: &[(String, QVal)]) -> Result<Paging, ApiError> {
    let mut errors = Vec::new();
    let (page, per) = paging(map, &mut errors);
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    Ok(finish_paging(page, per))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pasangan → query mentah ter-encode → `mz_core::qs::parse` (jalur yang sama dengan route).
    fn pairs(q: &[(&str, &str)]) -> Vec<(String, QVal)> {
        let enc = |s: &str| {
            s.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() {
                        char::from(b).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        };
        let raw = q
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&");
        mz_core::qs::parse(&raw)
    }

    fn msgs(r: Result<ListQuery, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => panic!("harusnya validasi gagal: {other:?}"),
        }
    }

    #[test]
    fn defaults_and_clamping() {
        let q = ListQuery::parse(&[]).unwrap();
        assert_eq!(q.paging.page_number, 1);
        assert_eq!(q.paging.items_per_page, 20);
        assert!(q.entity_type.is_none() && q.search.is_none());
        // Kosong → fallback.
        let q = ListQuery::parse(&pairs(&[("pageNumber", ""), ("itemsPerPage", "")])).unwrap();
        assert_eq!((q.paging.page_number, q.paging.items_per_page), (1, 20));
        // parseInt: awalan angka.
        let q =
            ListQuery::parse(&pairs(&[("pageNumber", "3abc"), ("itemsPerPage", "50.9")])).unwrap();
        assert_eq!((q.paging.page_number, q.paging.items_per_page), (3, 50));
    }

    #[test]
    fn array_forms_match_qs_and_to_array() {
        let q = ListQuery::parse(&pairs(&[("entityType", "store,employee")])).unwrap();
        assert_eq!(q.entity_type, Some(vec!["store".into(), "employee".into()]));
        let q = ListQuery::parse(&pairs(&[("action", "create"), ("action", "delete")])).unwrap();
        assert_eq!(q.action, Some(vec!["create".into(), "delete".into()]));
        let q = ListQuery::parse(&pairs(&[
            ("entityType[]", "store"),
            ("entityType[]", "geofence"),
        ]))
        .unwrap();
        assert_eq!(q.entity_type, Some(vec!["store".into(), "geofence".into()]));
        let q = ListQuery::parse(&pairs(&[("entityType[0]", "store")])).unwrap();
        assert_eq!(q.entity_type, Some(vec!["store".into()]));
        // Kosong = tidak ada filter.
        let q = ListQuery::parse(&pairs(&[("entityType", ""), ("actorId", "")])).unwrap();
        assert!(q.entity_type.is_none() && q.actor_id.is_none());
    }

    #[test]
    fn validation_messages_and_order() {
        let m = msgs(ListQuery::parse(&pairs(&[
            ("entityType", "store,bogus"),
            ("entityUuid", "nope"),
            ("action", "zap"),
            ("actorId", "x"),
            ("dateFrom", "yesterday"),
            ("dateTo", ""),
            ("pageNumber", "0"),
            ("itemsPerPage", "101"),
        ])));
        assert_eq!(m.len(), 8);
        assert!(m[0].starts_with("each value in entityType must be one of the following values: company, employee, store,"));
        assert_eq!(m[1], "entityUuid must be a UUID");
        assert_eq!(
            m[2],
            "each value in action must be one of the following values: create, update, delete, restore"
        );
        assert_eq!(m[3], "each value in actorId must be a UUID");
        assert_eq!(m[4], "dateFrom must be a valid ISO 8601 date string");
        assert_eq!(m[5], "dateTo must be a valid ISO 8601 date string");
        assert_eq!(m[6], "pageNumber must not be less than 1");
        assert_eq!(m[7], "itemsPerPage must not be greater than 100");
    }

    #[test]
    fn nan_paging_reports_min_and_int() {
        let m = msgs(ListQuery::parse(&pairs(&[
            ("pageNumber", "abc"),
            ("itemsPerPage", "x"),
        ])));
        assert_eq!(
            m,
            vec![
                "pageNumber must not be less than 1",
                "pageNumber must be an integer number",
                "itemsPerPage must not be greater than 100",
                "itemsPerPage must not be less than 1",
                "itemsPerPage must be an integer number",
            ]
        );
    }

    #[test]
    fn search_rules() {
        let long = "x".repeat(161);
        let m = msgs(ListQuery::parse(&pairs(&[("search", &long)])));
        assert_eq!(
            m,
            vec!["search must be shorter than or equal to 160 characters"]
        );
        let m = msgs(ListQuery::parse(&pairs(&[
            ("search", "a"),
            ("search", "b"),
        ])));
        assert_eq!(m.len(), 2);
        assert_eq!(m[1], "search must be a string");
        let q = ListQuery::parse(&pairs(&[("search", "")])).unwrap();
        assert_eq!(q.search.as_deref(), Some(""));
        assert!(ListQuery::parse(&pairs(&[("search", &"x".repeat(160))])).is_ok());
    }

    #[test]
    fn uuid_check() {
        assert!(is_uuid("00000000-0000-4000-8000-00000000dead"));
        assert!(is_uuid("9F1C2D3E-4A5B-6C7D-8E9F-0A1B2C3D4E5F"));
        assert!(!is_uuid("9f1c2d3e4a5b6c7d8e9f0a1b2c3d4e5f"));
        assert!(!is_uuid("9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5"));
        assert!(!is_uuid(""));
    }

    fn ms(a: &DateArg) -> i64 {
        match a {
            DateArg::Ok(d) => d.and_utc().timestamp_millis(),
            DateArg::Unreadable => panic!("tak terbaca"),
        }
    }

    #[test]
    fn iso8601_forms() {
        let z = parse_date_arg("2026-08-01T00:00:00.000Z").unwrap();
        assert_eq!(ms(&z), 1_785_542_400_000);
        // Tanggal saja = UTC tengah malam.
        assert_eq!(
            ms(&parse_date_arg("2026-08-01").unwrap()),
            1_785_542_400_000
        );
        // Offset.
        assert_eq!(
            ms(&parse_date_arg("2026-08-01T07:00:00+07:00").unwrap()),
            1_785_542_400_000
        );
        assert_eq!(
            ms(&parse_date_arg("2026-08-01T07:00:00.5+0700").unwrap()),
            1_785_542_400_500
        );
        // Tanpa zona = dibaca sebagai UTC (server ber-TZ UTC).
        assert_eq!(
            ms(&parse_date_arg("2026-08-01T00:00").unwrap()),
            1_785_542_400_000
        );
        // Akhir hari.
        assert_eq!(
            ms(&parse_date_arg("2026-08-31T23:59:59.999Z").unwrap()),
            1_788_220_799_999
        );
        // Lolos validator tapi bukan Date JS.
        assert_eq!(parse_date_arg("20260801"), Some(DateArg::Unreadable));
        // Gagal validasi.
        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-08-32",
            "2026-08-01T25:00",
            "2026-08-01T10:61",
            "2026-08-01T10:00:00+25:00",
            "2026-08-01x",
        ] {
            assert!(parse_date_arg(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn entity_query_only_paging() {
        let p = parse_entity_query(&pairs(&[("pageNumber", "2"), ("zzz", "1")])).unwrap();
        assert_eq!((p.page_number, p.items_per_page), (2, 20));
        assert!(parse_entity_query(&pairs(&[("itemsPerPage", "0")])).is_err());
    }

    #[test]
    fn bin_query_ignores_list_only_filters() {
        let q = BinQuery::parse(&pairs(&[
            ("entityType", "store"),
            ("action", "bogus"),
            ("dateFrom", "x"),
        ]))
        .unwrap();
        assert_eq!(q.entity_type, Some(vec!["store".into()]));
        assert!(BinQuery::parse(&pairs(&[("entityType", "zzz")])).is_err());
    }
}
