//! Port `src/operator-audit/operator-audit-logs.controller.ts` (+ `.service.ts`, `.dto.ts`).
//!
//! * `GET /admin/audit-logs` (`:22`): `OperatorRoles(OWNER, FINANCE, SUPPORT)` + `ADMIN_VALIDATION_PIPE`
//!   (whitelist + forbidNonWhitelisted). Tanpa `OperatorAuditInterceptor`, jadi tidak ada audit baca.
//! * `GET /admin/audit-logs/actions` (`:28`): daftar aksi + label, tanpa DB.

use axum::Json;
use axum::extract::State;
use chrono::{Duration as ChronoDuration, NaiveDateTime};
use mz_core::iso8601::{DateArg, parse_date_arg};
use mz_core::qs::{self, QVal};
use mz_core::time::JsDate;
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{Operator, OperatorRole};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    RawQuery, coded_bad_request, finish, paging_errors, paging_values, query_number, query_val,
    skip_for,
};
use crate::operator_actions::{OPERATOR_ACTION_LABELS, OPERATOR_ACTIONS};

const ROLES: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];

/// `OperatorAuditLogsQueryDto` (urutan deklarasi).
const QUERY_KEYS: &[&str] = &[
    "operatorUuid",
    "action",
    "companyUuid",
    "targetUuid",
    "from",
    "to",
    "pageNumber",
    "itemsPerPage",
];

/// `MAX_RANGE_MS` = 366 hari.
const MAX_RANGE_DAYS: i64 = 366;

/// Hasil validasi query. `from`/`to` = `None` bila tidak dikirim.
struct Filter {
    operator_uuid: Option<uuid::Uuid>,
    action: Option<String>,
    company_uuid: Option<uuid::Uuid>,
    target_uuid: Option<uuid::Uuid>,
    from: Option<DateArg>,
    to: Option<DateArg>,
    page: i64,
    per: i64,
}

/// `@IsOptional() @IsUUID()`: pesan "<key> must be a UUID".
fn uuid_opt(e: &mut Vec<String>, q: &[(String, QVal)], key: &str) -> Option<uuid::Uuid> {
    match query_val(q, key) {
        None => None,
        Some(QVal::One(s)) if is_uuid(s) => uuid::Uuid::parse_str(s).ok(),
        Some(_) => {
            e.push(format!("{key} must be a UUID"));
            None
        }
    }
}

/// `@IsOptional() @IsISO8601({ strict: true })`.
fn date_opt(e: &mut Vec<String>, q: &[(String, QVal)], key: &str) -> Option<DateArg> {
    match query_val(q, key) {
        None => None,
        Some(QVal::One(s)) => match parse_date_arg(s) {
            Some(d) if strict_calendar(s) => Some(d),
            _ => {
                e.push(format!("{key} must be a valid ISO 8601 date string"));
                None
            }
        },
        Some(_) => {
            e.push(format!("{key} must be a valid ISO 8601 date string"));
            None
        }
    }
}

/// `isISO8601(..., { strict: true })` → `isValidDate`: tanggal kalender harus ada (2026-02-30
/// ditolak), dan tanggal ordinal `YYYY-DDD` harus DDD ≤ 365/366.
fn strict_calendar(s: &str) -> bool {
    let head = s.split(['T', ' ']).next().unwrap_or_default();
    let head = head.trim_start_matches(['+', '-']);
    let digits: Vec<u32> = head.chars().filter_map(|c| c.to_digit(10)).collect();
    let num = |from: usize, len: usize| {
        digits
            .get(from..from + len)
            .map(|d| d.iter().fold(0u32, |a, x| a * 10 + x))
    };
    let leap = |y: u32| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
    match digits.len() {
        7 => match (num(0, 4), num(4, 3)) {
            (Some(y), Some(d)) => d <= if leap(y) { 366 } else { 365 },
            _ => false,
        },
        8 => match (num(0, 4), num(4, 2), num(6, 2)) {
            // validator.js membangun tanggal lewat `Date` yang memetakan tahun 0..99 ke 19xx,
            // jadi `getUTCFullYear() !== year` → ditolak (paritas adm-misc: `0001-01-01` = 400).
            (Some(y), Some(_), Some(_)) if y < 100 => false,
            (Some(y), Some(m), Some(d)) => {
                let dim = match m {
                    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                    4 | 6 | 9 | 11 => 30,
                    2 => {
                        if leap(y) {
                            29
                        } else {
                            28
                        }
                    }
                    _ => return false,
                };
                (1..=dim).contains(&d)
            }
            _ => false,
        },
        _ => true,
    }
}

/// `ValidationPipe` (ADMIN) pada query: whitelist, lalu pesan per properti.
fn parse_filter(raw: Option<&str>) -> ApiResult<Filter> {
    let q = qs::parse(raw.unwrap_or_default());
    let mut e = Vec::new();
    for (k, _) in &q {
        if !QUERY_KEYS.contains(&k.as_str()) {
            e.push(format!("property {k} should not exist"));
        }
    }
    let operator_uuid = uuid_opt(&mut e, &q, "operatorUuid");

    let action = match query_val(&q, "action") {
        None => None,
        Some(QVal::One(s)) if OPERATOR_ACTIONS.contains(&s.as_str()) => Some(s.clone()),
        Some(_) => {
            e.push(format!(
                "action must be one of the following values: {}",
                OPERATOR_ACTIONS.join(", ")
            ));
            None
        }
    };
    let company_uuid = uuid_opt(&mut e, &q, "companyUuid");
    let target_uuid = uuid_opt(&mut e, &q, "targetUuid");
    let from = date_opt(&mut e, &q, "from");
    let to = date_opt(&mut e, &q, "to");

    let page = query_number(query_val(&q, "pageNumber"));
    let per = query_number(query_val(&q, "itemsPerPage"));
    paging_errors(&mut e, page, per);
    finish(e)?;
    let (page, per) = paging_values(page, per);

    Ok(Filter {
        operator_uuid,
        action,
        company_uuid,
        target_uuid,
        from,
        to,
        page,
        per,
    })
}

/// Validasi rentang tanggal (service): `from ≤ to` dan selisih ≤ 366 hari. Tanggal yang
/// lolos validator tetapi tak terbaca `new Date` menjadi galat Prisma (500).
fn resolve_range(
    from: Option<DateArg>,
    to: Option<DateArg>,
) -> ApiResult<(Option<NaiveDateTime>, Option<NaiveDateTime>)> {
    let unwrap = |d: Option<DateArg>| -> ApiResult<Option<NaiveDateTime>> {
        match d {
            None => Ok(None),
            Some(DateArg::Ok(t)) => Ok(Some(t)),
            // Invalid Date di V8 (`2020-W01-1`, `20200101`, `2020-366`, …) → galat Prisma, 500 di
            // Node (paritas adm-misc 2026-11-17). `2020-001`/`+2020-01-01` terbaca parser legacy V8
            // tetapi `Unreadable` di `mz_core::iso8601` (batas yang sudah didokumentasikan).
            Some(DateArg::Unreadable) => Err(ApiError::internal(anyhow::anyhow!(
                "tanggal tidak terbaca (Invalid Date)"
            ))),
        }
    };
    let (from, to) = (unwrap(from)?, unwrap(to)?);
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(coded_bad_request(
                "INVALID_DATE_RANGE",
                "`from` must be before or equal to `to`",
            ));
        }
        if t.signed_duration_since(f) > ChronoDuration::days(MAX_RANGE_DAYS) {
            return Err(coded_bad_request(
                "DATE_RANGE_TOO_LARGE",
                "Date range must not exceed 366 days",
            ));
        }
    }
    Ok((from, to))
}

/// Filter Prisma `where` sebagai SQL. Relasi `operator`/`company` dipetakan lewat join.
fn push_filters(
    qb: &mut QueryBuilder<'_, Postgres>,
    f: &Filter,
    from: Option<NaiveDateTime>,
    to: Option<NaiveDateTime>,
) {
    qb.push(" WHERE TRUE");
    if let Some(a) = &f.action {
        qb.push(" AND l.action = ").push_bind(a.clone());
    }
    if let Some(t) = f.target_uuid {
        qb.push(" AND l.\"targetUuid\" = ").push_bind(t);
    }
    if let Some(u) = f.operator_uuid {
        qb.push(" AND o.uuid = ").push_bind(u);
    }
    if let Some(c) = f.company_uuid {
        qb.push(" AND c.uuid = ").push_bind(c);
    }
    if let Some(v) = from {
        qb.push(" AND l.\"createdAt\" >= ").push_bind(v);
    }
    if let Some(v) = to {
        qb.push(" AND l.\"createdAt\" <= ").push_bind(v);
    }
}

const FROM_JOIN: &str = r#" FROM "operatorAuditLog" l
    JOIN operators o ON o.id = l."operatorId"
    LEFT JOIN company c ON c.id = l."companyId""#;

/// Baris `select` Node (urutan kunci JSON mengikuti `select`).
#[derive(Debug, sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
struct AuditRow {
    uuid: String,
    created_at: JsDate,
    action: String,
    target_type: String,
    target_uuid: Option<String>,
    before: Option<sqlx::types::Json<Value>>,
    after: Option<sqlx::types::Json<Value>>,
    reason: Option<String>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    operator_uuid: String,
    operator_name: String,
    operator_role: String,
    company_uuid: Option<String>,
    company_name: Option<String>,
}

impl AuditRow {
    fn into_json(self) -> Value {
        let company = match (self.company_uuid, self.company_name) {
            (Some(uuid), name) => json!({ "uuid": uuid, "name": name }),
            _ => Value::Null,
        };
        json!({
            "uuid": self.uuid,
            "createdAt": self.created_at,
            "action": self.action,
            "targetType": self.target_type,
            "targetUuid": self.target_uuid,
            "before": self.before.map(|j| j.0),
            "after": self.after.map(|j| j.0),
            "reason": self.reason,
            "ipAddress": self.ip_address,
            "userAgent": self.user_agent,
            "operator": {
                "uuid": self.operator_uuid,
                "name": self.operator_name,
                "role": self.operator_role,
            },
            "company": company,
        })
    }
}

/// Node: src/operator-audit/operator-audit-logs.controller.ts:22 (`findAll`).
pub async fn h_list(
    State(state): State<AppState>,
    op: Operator,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    op.require(ROLES)?;
    let f = parse_filter(raw.as_deref())?;
    let (from, to) = resolve_range(f.from.clone(), f.to.clone())?;
    let skip = skip_for(f.page, f.per)?;

    let mut count = QueryBuilder::<Postgres>::new(format!("SELECT COUNT(*){FROM_JOIN}"));
    push_filters(&mut count, &f, from, to);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT l.uuid::text AS "uuid", l."createdAt", l.action, l."targetType",
                  l."targetUuid"::text AS "targetUuid", l.before, l.after, l.reason,
                  l."ipAddress", l."userAgent",
                  o.uuid::text AS "operatorUuid", o.name AS "operatorName",
                  o.role::text AS "operatorRole",
                  c.uuid::text AS "companyUuid", c.name AS "companyName"{FROM_JOIN}"#
    ));
    push_filters(&mut qb, &f, from, to);
    qb.push(r#" ORDER BY l."createdAt" DESC, l.id DESC"#);
    qb.push(" OFFSET ").push_bind(skip);
    qb.push(" LIMIT ").push_bind(f.per);
    let rows: Vec<AuditRow> = qb.build_query_as().fetch_all(&state.db).await?;

    Ok(Json(json!({
        "result": rows.into_iter().map(AuditRow::into_json).collect::<Vec<_>>(),
        "pageNumber": f.page,
        "itemsPerPage": f.per,
        "total": total,
    })))
}

/// Node: src/operator-audit/operator-audit-logs.controller.ts:28 (`actions`).
pub async fn h_actions(op: Operator) -> ApiResult<Json<Value>> {
    op.require(ROLES)?;
    let list: Vec<Value> = OPERATOR_ACTION_LABELS
        .iter()
        .map(|(a, l)| json!({ "action": a, "label": l }))
        .collect();
    Ok(Json(Value::Array(list)))
}
