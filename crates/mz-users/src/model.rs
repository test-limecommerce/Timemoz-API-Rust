//! Tipe input/baris dan logika MURNI (tanpa DB) milik `mz-users`: peringkat efektif, paginasi
//! `GET /users`, filter role admin, format angka JS, klasifikasi galat.

use mz_core::ApiError;
use mz_core::enums::{BaseRole, effective_role_code, rank_of};
use serde_json::{Map, Value, json};

// ── input service ────────────────────────────────────────────────────────────

/// `InviteUserDto` yang sudah tervalidasi.
#[derive(Debug, Clone)]
pub struct InviteInput {
    pub username: String,
    pub name: String,
    pub access_level_code: String,
    pub is_screenshot_active: bool,
    pub generate_password: bool,
}

/// `UpdateUserDto` yang sudah tervalidasi (angka sudah integer).
#[derive(Debug, Clone)]
pub struct UpdateUserInput {
    pub username: String,
    pub name: String,
    pub status: i32,
    pub screenshot_per_hour: i32,
    pub idle_time_in_second: i32,
    pub is_screenshot_active: bool,
    pub is_blur: bool,
    pub access_level: String,
}

/// `CreatePayRateDto` / `UpdatePayRateDto`. `Option<Option<_>>`: luar = kunci ada di body,
/// dalam = nilai (`None` = `null`). Dibutuhkan karena PATCH membedakan `undefined` dan `null`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PayRateInput {
    pub pay_period_type: Option<String>,
    pub timesheet_approval: Option<Option<bool>>,
    pub pay_rate: Option<Option<f64>>,
    pub rate_type: Option<String>,
    pub effective_date: Option<String>,
    pub notes: Option<Option<String>>,
}

// ── angka & status ───────────────────────────────────────────────────────────

/// Angka JSON seperti `JSON.stringify(number)`: `5`, bukan `5.0`.
pub fn js_num(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
        return Value::from(x as i64);
    }
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

pub fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, js_num)
}

/// `isTokenRevokingStatus`: DISABLED (3) atau DELETED (0).
pub fn is_token_revoking_status(status: Option<i32>) -> bool {
    matches!(status, Some(3 | 0))
}

/// Kode access level yang dianggap pengelola company (guard "admin terakhir" self-service).
pub const COMPANY_ADMIN_CODES: [&str; 2] = ["SUPER_ADMIN", "ADMIN"];

/// `builtInBaseRoleOf` / bagian `baseRole` dari `roleCodeFilter` — satu implementasi di `mz_core::roles`.
pub use mz_core::roles::{built_in_base_role_of, role_code_filter_base_roles};

// ── peringkat efektif (GET /users, GET /users/:uuid) ─────────────────────────

/// Baris `userAccessLevel` + `accessLevel` (LEFT JOIN) untuk `withEffectiveRole`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AssignmentRow {
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    #[sqlx(rename = "accessLevelId")]
    pub access_level_id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub status: i32,
    pub al_id: Option<i32>,
    pub al_name: Option<String>,
    pub al_code: Option<String>,
    pub al_is_system: Option<bool>,
    pub al_base_role: Option<String>,
    pub al_status: Option<i32>,
}

/// SELECT untuk [`AssignmentRow`] (alias `ual`, `al`).
pub const ASSIGNMENT_SELECT: &str = r#"SELECT ual."userId", ual."accessLevelId", ual."companyId", ual.status,
    al.id AS al_id, al.name AS al_name, al.code AS al_code, al."isSystem" AS al_is_system,
    al."baseRole"::text AS al_base_role, al.status AS al_status
    FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId""#;

impl AssignmentRow {
    fn effective_code(&self) -> Option<String> {
        let code = self.al_code.as_deref()?;
        let base = self
            .al_base_role
            .as_deref()
            .and_then(|b| b.parse::<BaseRole>().ok());
        Some(effective_role_code(
            code,
            self.al_is_system.unwrap_or(false),
            base,
        ))
    }
}

/// `effectiveRoleInCompany`: hanya baris aktif dengan role aktif di `company_id`; yang tertinggi
/// menang; `None` bila tak ada satu pun yang peringkatnya dikenal (fail closed).
pub fn effective_role_in_company(rows: &[AssignmentRow], company_id: i32) -> Option<String> {
    let mut best: Option<(String, u8)> = None;
    for row in rows {
        if row.company_id != company_id || row.status != 1 || row.al_status != Some(1) {
            continue;
        }
        let Some(code) = row.effective_code() else {
            continue;
        };
        let Some(rank) = rank_of(&code) else { continue };
        if best.as_ref().is_none_or(|(_, r)| rank > *r) {
            best = Some((code, rank));
        }
    }
    best.map(|(code, _)| code)
}

/// `withEffectiveRole(row, companyId)`: `userAccessLevel` dipangkas jadi
/// `{ accessLevelId, accessLevel: { id, name, code } }` + `effectiveRole`.
pub fn with_effective_role(
    mut base: Map<String, Value>,
    rows: &[AssignmentRow],
    company_id: i32,
) -> Value {
    let assignments: Vec<Value> = rows
        .iter()
        .map(|r| {
            let access_level = match (r.al_id, &r.al_name, &r.al_code) {
                (Some(id), Some(name), Some(code)) => {
                    json!({ "id": id, "name": name, "code": code })
                }
                _ => Value::Null,
            };
            json!({ "accessLevelId": r.access_level_id, "accessLevel": access_level })
        })
        .collect();
    base.insert("userAccessLevel".into(), Value::Array(assignments));
    base.insert(
        "effectiveRole".into(),
        effective_role_in_company(rows, company_id).map_or(Value::Null, Value::String),
    );
    Value::Object(base)
}

// ── paginasi GET /users ──────────────────────────────────────────────────────

/// Kolom yang boleh dipakai untuk mengurutkan (`ALLOWED_USER_SORT_COLUMNS`).
pub const ALLOWED_USER_SORT_COLUMNS: [&str; 6] =
    ["id", "name", "username", "createdAt", "updatedAt", "status"];

/// Hasil normalisasi parameter paginasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paging {
    pub page_number: i64,
    pub items_per_page: i64,
    /// Salah satu dari [`ALLOWED_USER_SORT_COLUMNS`].
    pub order_column: &'static str,
    pub desc: bool,
}

fn is_direction(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "asc" | "desc"))
}

/// `take`/`skip` Node bernilai `NaN` (salah satu angka paginasi tidak dikirim): Prisma menolak -> 500.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagingNan;

/// Meniru blok normalisasi di `UsersService.findAll`.
pub fn resolve_paging(
    page_number: Option<i64>,
    items_per_page: Option<i64>,
    order_by: Option<&str>,
    order_column: Option<&str>,
    total: i64,
) -> Result<Paging, PagingNan> {
    let truthy_num = |v: Option<i64>| v.is_some_and(|n| n != 0);
    let truthy_str = |v: Option<&str>| v.is_some_and(|s| !s.is_empty());
    let (page_number, items_per_page, order_by, order_column) = if !truthy_num(page_number)
        && !truthy_num(items_per_page)
        && !truthy_str(order_by)
        && !truthy_str(order_column)
    {
        (Some(1), Some(total), Some("desc"), Some("createdAt"))
    } else {
        (page_number, items_per_page, order_by, order_column)
    };

    // Tahan terhadap case dan terhadap frontend yang menukar orderBy <-> orderColumn.
    let (column, direction) = if is_direction(order_column) && !is_direction(order_by) {
        (order_by, order_column)
    } else {
        (order_column, order_by)
    };
    let desc = if is_direction(direction) {
        direction.is_none_or(|d| d.eq_ignore_ascii_case("desc"))
    } else {
        true
    };
    let order_column = ALLOWED_USER_SORT_COLUMNS
        .iter()
        .copied()
        .find(|c| Some(*c) == column)
        .unwrap_or("createdAt");

    let (Some(page_number), Some(items_per_page)) = (page_number, items_per_page) else {
        return Err(PagingNan);
    };
    Ok(Paging {
        page_number,
        items_per_page,
        order_column,
        desc,
    })
}

impl Paging {
    /// `skip = (pageNumber - 1) * itemsPerPage`; negatif ditolak Prisma.
    pub fn offset(&self) -> Option<i64> {
        let skip = self
            .page_number
            .saturating_sub(1)
            .saturating_mul(self.items_per_page);
        (skip >= 0).then_some(skip)
    }
}

// ── galat ────────────────────────────────────────────────────────────────────

/// `err instanceof HttpException` di Node.
pub fn is_http_exception(err: &ApiError) -> bool {
    !matches!(err, ApiError::UniqueViolation(_) | ApiError::Internal(_))
}

/// `(err.getResponse() as { message?: string })?.message || err.message` untuk `HttpException`.
pub fn http_exception_message(err: &ApiError) -> Option<String> {
    match err {
        ApiError::Message { message, .. } | ApiError::Plain { message, .. } => {
            Some(message.clone())
        }
        ApiError::Bare(status) => Some(status.canonical_reason().unwrap_or("Error").to_owned()),
        ApiError::Body { status, body } => Some(
            body.get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map_or_else(
                    || status.canonical_reason().unwrap_or("Error").to_owned(),
                    str::to_owned,
                ),
        ),
        ApiError::Validation(messages) => Some(messages.join(",")),
        ApiError::BadJson(message) => Some(message.clone()),
        ApiError::UniqueViolation(_) | ApiError::Internal(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn row(
        company: i32,
        status: i32,
        code: &str,
        is_system: bool,
        base: Option<&str>,
        al_status: i32,
    ) -> AssignmentRow {
        AssignmentRow {
            user_id: 1,
            access_level_id: 10,
            company_id: company,
            status,
            al_id: Some(10),
            al_name: Some(code.to_owned()),
            al_code: Some(code.to_owned()),
            al_is_system: Some(is_system),
            al_base_role: base.map(str::to_owned),
            al_status: Some(al_status),
        }
    }

    #[test]
    fn effective_role_picks_highest_active_in_company() {
        let rows = vec![
            row(1, 1, "STAFF", true, None, 1),
            row(1, 1, "c1_lead", false, Some("MANAGER"), 1),
            row(2, 1, "ADMIN", true, None, 1),
            row(1, 0, "SUPER_ADMIN", true, None, 1),
            row(1, 1, "ADMIN", true, None, 0),
        ];
        assert_eq!(
            effective_role_in_company(&rows, 1).as_deref(),
            Some("MANAGER")
        );
        assert_eq!(
            effective_role_in_company(&rows, 2).as_deref(),
            Some("ADMIN")
        );
        assert_eq!(effective_role_in_company(&rows, 3), None);
    }

    #[test]
    fn unknown_rank_fails_closed() {
        let rows = vec![row(1, 1, "WEIRD", true, None, 1)];
        assert_eq!(effective_role_in_company(&rows, 1), None);
        // Role custom kehilangan baseRole -> kode mentah -> tidak dikenal.
        let rows = vec![row(1, 1, "c1_x", false, None, 1)];
        assert_eq!(effective_role_in_company(&rows, 1), None);
    }

    #[test]
    fn with_effective_role_trims_assignment_shape() {
        let rows = vec![row(1, 1, "ADMIN", true, None, 1)];
        let v = with_effective_role(Map::new(), &rows, 1);
        assert_eq!(v["effectiveRole"], json!("ADMIN"));
        assert_eq!(
            v["userAccessLevel"],
            json!([{ "accessLevelId": 10, "accessLevel": { "id": 10, "name": "ADMIN", "code": "ADMIN" } }])
        );
    }

    #[test]
    fn paging_defaults_when_nothing_given() {
        let p = resolve_paging(None, None, None, None, 42);
        assert_eq!(
            p,
            Ok(Paging {
                page_number: 1,
                items_per_page: 42,
                order_column: "createdAt",
                desc: true
            })
        );
        assert_eq!(p.map(|p| p.offset()), Ok(Some(0)));
    }

    #[test]
    fn paging_swaps_direction_and_column() {
        let p = resolve_paging(Some(2), Some(10), Some("name"), Some("ASC"), 100);
        assert_eq!(
            p,
            Ok(Paging {
                page_number: 2,
                items_per_page: 10,
                order_column: "name",
                desc: false
            })
        );
        assert_eq!(p.map(|p| p.offset()), Ok(Some(10)));
    }

    #[test]
    fn paging_unknown_column_falls_back_and_bad_direction_is_desc() {
        let p = resolve_paging(Some(1), Some(5), Some("sideways"), Some("password"), 9);
        assert_eq!(
            p,
            Ok(Paging {
                page_number: 1,
                items_per_page: 5,
                order_column: "createdAt",
                desc: true
            })
        );
    }

    #[test]
    fn paging_missing_half_is_nan_error() {
        assert_eq!(resolve_paging(Some(2), None, None, None, 9), Err(PagingNan));
        assert_eq!(
            resolve_paging(None, Some(5), Some("asc"), None, 9),
            Err(PagingNan)
        );
    }

    #[test]
    fn paging_zero_counts_as_not_given() {
        // `!0` benar: semua falsy -> default.
        let p = resolve_paging(Some(0), Some(0), Some(""), Some(""), 7);
        assert_eq!(p.map(|p| p.items_per_page), Ok(7));
    }

    #[test]
    fn negative_skip_is_rejected() {
        let p = Paging {
            page_number: 0,
            items_per_page: 10,
            order_column: "id",
            desc: false,
        };
        assert_eq!(p.offset(), None);
    }

    #[test]
    fn role_code_filter_maps_built_in_codes_only() {
        assert_eq!(
            role_code_filter_base_roles(&COMPANY_ADMIN_CODES),
            vec!["ADMIN"]
        );
        assert!(role_code_filter_base_roles(&["SYSTEM_ADMIN", "GUEST"]).is_empty());
        assert_eq!(
            role_code_filter_base_roles(&["MANAGER", "ADMIN", "MANAGER"]),
            vec!["MANAGER", "ADMIN"]
        );
    }

    #[test]
    fn token_revoking_statuses() {
        assert!(is_token_revoking_status(Some(3)));
        assert!(is_token_revoking_status(Some(0)));
        assert!(!is_token_revoking_status(Some(1)));
        assert!(!is_token_revoking_status(None));
    }

    #[test]
    fn js_numbers_have_no_trailing_zero() {
        assert_eq!(js_num(5.0).to_string(), "5");
        assert_eq!(js_num(15.5).to_string(), "15.5");
        assert_eq!(opt_num(None), Value::Null);
    }

    #[test]
    fn http_exception_classification() {
        assert!(is_http_exception(&ApiError::forbidden("x")));
        assert!(!is_http_exception(&ApiError::internal(anyhow::anyhow!(
            "boom"
        ))));
        assert_eq!(
            http_exception_message(&ApiError::unprocessable("Access Level 'X' not found"))
                .as_deref(),
            Some("Access Level 'X' not found")
        );
        let seat = ApiError::body(
            StatusCode::CONFLICT,
            json!({ "code": "SEAT_LIMIT_REACHED", "message": "Seat limit reached (1/1)." }),
        );
        assert_eq!(
            http_exception_message(&seat).as_deref(),
            Some("Seat limit reached (1/1).")
        );
        assert_eq!(
            http_exception_message(&ApiError::Bare(StatusCode::UNAUTHORIZED)).as_deref(),
            Some("Unauthorized")
        );
    }
}
