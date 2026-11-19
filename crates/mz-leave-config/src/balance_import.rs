//! Template & import CSV kuota cuti karyawan (tab "Kuota Karyawan" dashboard):
//! `GET balances/import-template`, `POST balances/import/preview`, `POST balances/import`.
//! Port `leave-balance-import-template.ts`, `leave-balance-import.ts`, dan
//! `leave-policies.service.ts` `buildBalanceImportTemplate` / `previewBalanceImport` /
//! `importBalances` (Node c3082b6).
//!
//! Nilai `Kuota Cuti` MENIMPA `leaveBalance.annualLeaveTotal` tahun target. Karyawan dicocokkan
//! lewat kolom Email (= `users.username`); kolom Nama hanya penanda untuk manusia.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use mz_core::csv_parse;
use mz_core::extract::{Validate, query_from_str};
use mz_core::jsfmt;
use mz_core::time::now_naive;
use mz_core::upload::{assert_csv_file, invalid_file, read_upload};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::balance::{NewAdjustment, create_balance_in_tx, insert_adjustment};
use mz_leave::enums::LeaveBalanceChangeSource;
use mz_leave::js::{ser_f64, ser_opt_f64};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgConnection;

use crate::balances::current_year;
use crate::common::company_of;
use crate::validate::{Num, Tri, check_int, present, tri_int};

/// `MAX_BALANCE_IMPORT_BYTES`.
pub const MAX_BALANCE_IMPORT_BYTES: usize = 1_048_576;
/// `ImportFileExceptionFilter(`${MAX_BALANCE_IMPORT_BYTES / 1_048_576} MB`)`.
const LIMIT_LABEL: &str = "1 MB";
/// `MAX_BALANCE_IMPORT_ROWS`.
pub const MAX_BALANCE_IMPORT_ROWS: usize = 5000;
/// `MAX_BALANCE_IMPORT_QUOTA`.
pub const MAX_BALANCE_IMPORT_QUOTA: f64 = 365.0;
/// `BALANCE_IMPORT_REASON`.
pub const BALANCE_IMPORT_REASON: &str = "Import kuota cuti dari CSV";

const HEADER_EMAIL: &str = "email";
const HEADER_QUOTA: &str = "kuota cuti";
const REQUIRED_HEADERS: [&str; 2] = [HEADER_EMAIL, HEADER_QUOTA];

const TEMPLATE_HEADERS: [&str; 3] = ["Nama", "Email", "Kuota Cuti"];
const TEMPLATE_FILENAME: &str = "template-import-kuota-cuti.csv";

// ── template ─────────────────────────────────────────────────────────────────

/// `csvCell`: sel teks berawalan `=`, `+`, `-`, `@`, tab, atau CR diberi awalan `'` (CSV
/// injection — nama diisi bebas oleh user); lalu dikutip bila berisi `"`, `,`, CR, atau LF.
fn csv_cell(value: &str) -> String {
    let mut s = value.to_owned();
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        s.insert(0, '\'');
    }
    if s.contains(['"', ',', '\r', '\n']) {
        return format!("\"{}\"", s.replace('"', "\"\""));
    }
    s
}

/// `generateLeaveBalanceTemplateCsv(rows)`: BOM UTF-8 + baris CRLF, kuota diisi 0.
pub fn template_csv(rows: &[(String, String)]) -> Vec<u8> {
    let mut lines: Vec<String> = vec![
        TEMPLATE_HEADERS
            .iter()
            .map(|h| csv_cell(h))
            .collect::<Vec<_>>()
            .join(","),
    ];
    for (name, email) in rows {
        lines.push(format!("{},{},0", csv_cell(name), csv_cell(email)));
    }
    format!("\u{FEFF}{}\r\n", lines.join("\r\n")).into_bytes()
}

// ── parse & klasifikasi (murni) ──────────────────────────────────────────────

/// `/\s/` JavaScript (+ `_` untuk header).
fn is_js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}'
}

/// `h.trim().toLowerCase().replace(/[_\s]+/g, ' ')`: `"Kuota_Cuti "` → `"kuota cuti"`.
fn normalize_header(h: &str) -> String {
    let lower = jsfmt::trim(h).to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut in_run = false;
    for c in lower.chars() {
        if c == '_' || is_js_space(c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// `detectDelimiter`: Excel berbahasa Indonesia menyimpan CSV ber-`;` (desimal koma), jadi
/// pemisah dipilih dari baris pertama (4096 byte awal): ada `;` dan tidak ada `,` → `;`.
pub fn detect_delimiter(buffer: &[u8]) -> u8 {
    let head = buffer.get(..4096).unwrap_or(buffer);
    let first_line = head.split(|b| *b == b'\n').next().unwrap_or_default();
    if first_line.contains(&b';') && !first_line.contains(&b',') {
        b';'
    } else {
        b','
    }
}

/// `ParsedBalanceRow`: `row` = nomor baris file (header = baris 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRow {
    pub row: usize,
    pub email: String,
    pub quota: String,
    pub raw: Map<String, Value>,
}

/// `parseBalanceCsv(buffer)`.
pub fn parse_csv(buffer: &[u8]) -> Result<Vec<ParsedRow>, ApiError> {
    let parsed = csv_parse::parse_with(
        buffer,
        csv_parse::Options {
            delimiter: detect_delimiter(buffer),
            header: normalize_header,
        },
    )
    .map_err(invalid_file)?;
    let missing: Vec<&str> = REQUIRED_HEADERS
        .iter()
        .copied()
        .filter(|h| !parsed.headers.iter().any(|x| x == h))
        .collect();
    if !missing.is_empty() {
        return Err(invalid_file(format!(
            "Kolom wajib tidak ada: {}",
            missing.join(", ")
        )));
    }
    if parsed.records.len() > MAX_BALANCE_IMPORT_ROWS {
        return Err(invalid_file(format!(
            "Maksimal {MAX_BALANCE_IMPORT_ROWS} baris data per file"
        )));
    }
    Ok(parsed
        .records
        .into_iter()
        .map(|r| {
            let mut raw = Map::new();
            for (name, value) in parsed.headers.iter().zip(r.fields) {
                // `obj[name] = value`: nama kembar menimpa nilai, posisi kunci tetap.
                raw.insert(name.clone(), Value::String(value));
            }
            let cell = |k: &str| {
                raw.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            ParsedRow {
                row: r.line,
                email: cell(HEADER_EMAIL),
                quota: cell(HEADER_QUOTA),
                raw,
            }
        })
        .collect())
}

/// `normalizeImportEmail`: template menambahkan `'` di depan sel berawalan formula; buang lagi.
pub fn normalize_email(value: &str) -> String {
    let t = jsfmt::trim(value);
    jsfmt::trim(t.strip_prefix('\'').unwrap_or(t)).to_lowercase()
}

/// `parseQuota`: `"12"`, `"12.5"`, `"12,5"` (desimal koma); maksimal 3 digit bulat + 2 desimal,
/// ≤ 365. `None` = tidak valid.
pub fn parse_quota(value: &str) -> Option<f64> {
    let s = jsfmt::trim(value);
    let digits =
        |p: &str, max: usize| (1..=max).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit());
    let ok = match s.find(['.', ',']) {
        None => digits(s, 3),
        Some(i) => digits(s.get(..i)?, 3) && digits(s.get(i + 1..)?, 2),
    };
    if !ok {
        return None;
    }
    let n: f64 = s.replace(',', ".").parse().ok()?;
    (n.is_finite() && n <= MAX_BALANCE_IMPORT_QUOTA).then_some(n)
}

/// `BalanceImportMember`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: i32,
    pub name: String,
    pub email: String,
}

/// `BalanceImportCurrent` (+ `id` baris saldo).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Current {
    pub id: i32,
    pub annual_leave_total: f64,
    pub annual_leave_used: f64,
}

/// `BalanceToUpdate`. Urutan kunci = runtime Node (`{ ...base, previous }`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToUpdate {
    pub row: usize,
    pub user_id: i32,
    pub name: String,
    pub email: String,
    #[serde(serialize_with = "ser_f64")]
    pub quota: f64,
    /// `None` = saldo tahun ini belum ada dan akan dibuat dulu.
    #[serde(serialize_with = "ser_opt_f64")]
    pub previous: Option<f64>,
}

/// `BalanceUnchanged`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unchanged {
    pub row: usize,
    pub user_id: i32,
    pub name: String,
    pub email: String,
    #[serde(serialize_with = "ser_f64")]
    pub quota: f64,
}

/// `BalanceInvalid`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Invalid {
    pub row: usize,
    pub raw: Map<String, Value>,
    pub errors: Vec<String>,
}

/// `BalanceImportClassification`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    pub to_update: Vec<ToUpdate>,
    pub unchanged: Vec<Unchanged>,
    pub invalid: Vec<Invalid>,
}

/// `classifyBalanceRows` — murni. Email duplikat: baris pertama jadi acuan, berikutnya invalid.
pub fn classify(
    rows: &[ParsedRow],
    members_by_email: &HashMap<String, Member>,
    current_by_user: &HashMap<i32, Current>,
) -> Classification {
    let mut result = Classification::default();
    let mut first_row_by_email: HashMap<String, usize> = HashMap::new();

    for r in rows {
        let mut errors: Vec<String> = Vec::new();

        let email = normalize_email(&r.email);
        let member = if email.is_empty() {
            None
        } else {
            members_by_email.get(&email)
        };
        if email.is_empty() {
            errors.push("email wajib diisi".to_owned());
        } else if member.is_none() {
            errors.push("email tidak ditemukan di karyawan aktif company".to_owned());
        }

        if !email.is_empty() {
            match first_row_by_email.get(&email) {
                None => {
                    first_row_by_email.insert(email.clone(), r.row);
                }
                Some(first) => errors.push(format!("email duplikat dengan baris {first}")),
            }
        }

        let quota = parse_quota(&r.quota);
        if quota.is_none() {
            errors.push(format!(
                "kuota cuti harus angka 0–{} (maksimal 2 desimal)",
                jsfmt::num_string(MAX_BALANCE_IMPORT_QUOTA)
            ));
        }

        let current = member.and_then(|m| current_by_user.get(&m.user_id));
        if let (Some(q), Some(c)) = (quota, current)
            && q < c.annual_leave_used
        {
            errors.push(format!(
                "kuota cuti tidak boleh di bawah cuti yang sudah terpakai ({} hari)",
                jsfmt::num_string(c.annual_leave_used)
            ));
        }

        let (Some(member), Some(quota), true) = (member, quota, errors.is_empty()) else {
            result.invalid.push(Invalid {
                row: r.row,
                raw: r.raw.clone(),
                errors,
            });
            continue;
        };

        match current {
            Some(c) if c.annual_leave_total == quota => result.unchanged.push(Unchanged {
                row: r.row,
                user_id: member.user_id,
                name: member.name.clone(),
                email: member.email.clone(),
                quota,
            }),
            _ => result.to_update.push(ToUpdate {
                row: r.row,
                user_id: member.user_id,
                name: member.name.clone(),
                email: member.email.clone(),
                quota,
                previous: current.map(|c| c.annual_leave_total),
            }),
        }
    }
    result
}

// ── query ────────────────────────────────────────────────────────────────────

/// `ImportYearQueryDto`: `@IsInt @Min(2020) @Max(2100) @IsOptional @Type(() => Number) year`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImportYearQuery {
    #[serde(default, deserialize_with = "present")]
    pub year: Option<Value>,
}

impl Validate for ImportYearQuery {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "year",
            &Num::of(self.year.as_ref()),
            true,
            Some(2020.0),
            Some(2100.0),
        );
        e
    }
}

/// `@Query() ImportYearQueryDto` (pipa jalan SESUDAH `FileInterceptor`) → tahun target.
fn import_year(raw_query: &str) -> Result<i32, ApiError> {
    let query: ImportYearQuery = query_from_str(raw_query)?;
    let errors = query.validate();
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    Ok(match tri_int(query.year.as_ref(), "year")? {
        Tri::Val(y) => y,
        _ => current_year(),
    })
}

/// `FileInterceptor` → `@Query()` → `assertBalanceCsvFile` → `parseBalanceCsv`.
async fn read_import(req: Request) -> Result<(i32, Vec<ParsedRow>), ApiError> {
    let raw_query = req.uri().query().unwrap_or_default().to_owned();
    let upload = read_upload(req, MAX_BALANCE_IMPORT_BYTES, LIMIT_LABEL).await?;
    let year = import_year(&raw_query)?;
    assert_csv_file(upload.as_ref())?;
    let rows = parse_csv(upload.as_ref().map_or(&[][..], |u| u.bytes.as_slice()))?;
    Ok((year, rows))
}

/// `activeMembersByEmail`: anggota aktif company, dikunci per email (`users.username`).
async fn active_members_by_email(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<HashMap<String, Member>> {
    let users: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT u.id, u.name, u.username FROM users u
           WHERE EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)
           ORDER BY u.id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut map = HashMap::new();
    for (user_id, name, username) in users {
        map.entry(normalize_email(&username)).or_insert(Member {
            user_id,
            name,
            email: username,
        });
    }
    Ok(map)
}

/// `currentBalancesByUser`.
async fn current_balances(
    conn: &mut PgConnection,
    company_id: i32,
    year: i32,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, Current>> {
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i32, f64, f64)> = sqlx::query_as(
        r#"SELECT id, "userId", "annualLeaveTotal", "annualLeaveUsed" FROM "leaveBalance"
           WHERE "companyId" = $1 AND year = $2 AND "userId" = ANY($3)"#,
    )
    .bind(company_id)
    .bind(year)
    .bind(user_ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, user_id, total, used)| {
            (
                user_id,
                Current {
                    id,
                    annual_leave_total: total,
                    annual_leave_used: used,
                },
            )
        })
        .collect())
}

// ── handler ──────────────────────────────────────────────────────────────────

/// `GET /leave-policies/balances/import-template`. Node: `downloadBalanceImportTemplate` →
/// `buildBalanceImportTemplate`: semua anggota aktif (bukan hanya yang sudah punya saldo),
/// urut nama lalu id, kuota 0.
pub async fn import_template(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Response> {
    let company_id = company_of(&state, &ctx).await?;
    let users: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT u.name, u.username FROM users u
           WHERE EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)
           ORDER BY u.name ASC, u.id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let body = template_csv(&users);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{TEMPLATE_FILENAME}\""),
        )
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .map_err(ApiError::internal)
}

#[derive(Debug, Serialize)]
pub struct PreviewResult {
    pub year: i32,
    #[serde(flatten)]
    pub classification: Classification,
}

/// `POST /leave-policies/balances/import/preview`. Node: `previewBalanceImport`. Tidak menulis.
pub async fn import_preview(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Json<PreviewResult>> {
    let (year, rows) = read_import(req).await?;
    let company_id = company_of(&state, &ctx).await?;
    let mut conn = state.db.acquire().await?;
    let members = active_members_by_email(&mut conn, company_id).await?;
    let ids: Vec<i32> = members.values().map(|m| m.user_id).collect();
    let current = current_balances(&mut conn, company_id, year, &ids).await?;
    Ok(Json(PreviewResult {
        year,
        classification: classify(&rows, &members, &current),
    }))
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub year: i32,
    pub updated: usize,
    pub unchanged: usize,
    pub initialized: usize,
    pub invalid: Vec<Invalid>,
}

/// `POST /leave-policies/balances/import` (201). Node: `importBalances`. Satu transaksi:
/// 1. advisory lock per company → dua import bersamaan berurutan;
/// 2. saldo yang belum ada dibuat dulu (hanya untuk baris yang berpeluang valid);
/// 3. saldo target dikunci `FOR UPDATE` urut id lalu dibaca ulang;
/// 4. klasifikasi dari baris terkunci, jadi cuti yang disetujui di tengah import tidak bisa
///    membuat kuota < terpakai.
///
/// Tiap perubahan dicatat di `leaveBalanceAdjustment` (source ADJUSTMENT).
pub async fn import_balances(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<(StatusCode, Json<ImportResult>)> {
    let (year, rows) = read_import(req).await?;
    let requester_id = ctx.require_user_id()?;
    let company_id = company_of(&state, &ctx).await?;

    let mut tx = state.db.begin().await?;
    mz_core::db::advisory_xact_lock(&mut tx, &format!("leave-balance-import:{company_id}")).await?;
    let members = active_members_by_email(&mut tx, company_id).await?;

    // Urutan sisip (Set JS) dipertahankan: saldo dibuat dalam urutan baris file.
    let mut candidates: Vec<i32> = Vec::new();
    let mut seen = HashSet::new();
    for r in &rows {
        if let Some(m) = members.get(&normalize_email(&r.email))
            && parse_quota(&r.quota).is_some()
            && seen.insert(m.user_id)
        {
            candidates.push(m.user_id);
        }
    }

    let existing = current_balances(&mut tx, company_id, year, &candidates).await?;
    let mut initialized = 0usize;
    for user_id in &candidates {
        if existing.contains_key(user_id) {
            continue;
        }
        create_balance_in_tx(&mut tx, *user_id, company_id, year).await?;
        initialized += 1;
    }

    if !candidates.is_empty() {
        // Urut id: dua jalur yang mengunci banyak saldo tidak saling deadlock.
        sqlx::query(
            r#"SELECT id FROM "leaveBalance"
               WHERE "companyId" = $1 AND year = $2 AND "userId" = ANY($3)
               ORDER BY id FOR UPDATE"#,
        )
        .bind(company_id)
        .bind(year)
        .bind(&candidates)
        .fetch_all(&mut *tx)
        .await?;
    }
    let locked = current_balances(&mut tx, company_id, year, &candidates).await?;
    let classification = classify(&rows, &members, &locked);

    let now = now_naive();
    for r in &classification.to_update {
        let Some(balance) = locked.get(&r.user_id) else {
            return Err(ApiError::internal(anyhow::anyhow!(
                "saldo user {} tidak terkunci",
                r.user_id
            )));
        };
        sqlx::query(
            r#"UPDATE "leaveBalance" SET "annualLeaveTotal" = $2, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(balance.id)
        .bind(r.quota)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    for r in &classification.to_update {
        let Some(balance) = locked.get(&r.user_id) else {
            continue;
        };
        insert_adjustment(
            &mut tx,
            &NewAdjustment {
                balance_id: balance.id,
                user_id: r.user_id,
                company_id,
                adjusted_by_id: Some(requester_id),
                field: "annualLeaveTotal",
                // `previous` selalu terisi di sini: saldo yang belum ada sudah dibuat di langkah 2.
                delta: r.quota - r.previous.unwrap_or(0.0),
                reason: BALANCE_IMPORT_REASON.to_owned(),
                source: LeaveBalanceChangeSource::ADJUSTMENT,
                leave_request_id: None,
                dedupe_key: None,
                holiday_id: None,
                intended_delta: None,
            },
        )
        .await?;
    }
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(ImportResult {
            year,
            updated: classification.to_update.len(),
            unchanged: classification.unchanged.len(),
            initialized,
            invalid: classification.invalid,
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn rows_of(s: &str) -> Vec<ParsedRow> {
        parse_csv(s.as_bytes()).unwrap()
    }

    fn message_of(e: &ApiError) -> String {
        match e {
            ApiError::Body { body, .. } => body["message"].as_str().unwrap_or_default().to_owned(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn template_dibaca_ulang_apa_adanya() {
        let csv = template_csv(&[("Ahmad".into(), "ahmad@x.com".into())]);
        assert!(csv.starts_with("\u{FEFF}Nama,Email,Kuota Cuti\r\n".as_bytes()));
        let rows = parse_csv(&csv).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row, 2);
        assert_eq!(rows[0].email, "ahmad@x.com");
        assert_eq!(rows[0].quota, "0");
        assert_eq!(
            Value::Object(rows[0].raw.clone()),
            serde_json::json!({"nama": "Ahmad", "email": "ahmad@x.com", "kuota cuti": "0"})
        );
    }

    #[test]
    fn template_menjinakkan_formula_dan_mengutip() {
        assert_eq!(csv_cell("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(csv_cell("-x"), "'-x");
        assert_eq!(csv_cell("Santoso, Budi"), "\"Santoso, Budi\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_cell("Budi"), "Budi");
    }

    #[test]
    fn pemisah_titik_koma_dan_header_underscore() {
        let rows = rows_of("Nama;EMAIL;Kuota_Cuti\nAhmad;a@x.com;12,5\n");
        assert_eq!(
            Value::Object(rows[0].raw.clone()),
            serde_json::json!({"nama": "Ahmad", "email": "a@x.com", "kuota cuti": "12,5"})
        );
    }

    #[test]
    fn desimal_koma_tanpa_kutip_di_file_koma_ditolak() {
        let e = parse_csv(b"Nama,Email,Kuota Cuti\nAhmad,a@x.com,12,5\n").unwrap_err();
        assert_eq!(
            message_of(&e),
            "Invalid Record Length: columns length is 3, got 4 on line 2"
        );
    }

    #[test]
    fn kolom_wajib_dan_batas_baris() {
        let e = parse_csv(b"Nama,Email\nA,a@x.com\n").unwrap_err();
        assert_eq!(message_of(&e), "Kolom wajib tidak ada: kuota cuti");
        let body: String = (0..=MAX_BALANCE_IMPORT_ROWS)
            .map(|i| format!("u{i}@x.com,1\n"))
            .collect();
        let e = parse_csv(format!("Email,Kuota Cuti\n{body}").as_bytes()).unwrap_err();
        assert_eq!(message_of(&e), "Maksimal 5000 baris data per file");
    }

    #[test]
    fn header_normal() {
        assert_eq!(normalize_header(" Kuota_Cuti "), "kuota cuti");
        assert_eq!(normalize_header("KUOTA \t__ CUTI"), "kuota cuti");
        assert_eq!(normalize_header("_email"), " email");
    }

    #[test]
    fn kuota() {
        for (input, expected) in [
            ("0", 0.0),
            ("12", 12.0),
            ("12.5", 12.5),
            ("12,25", 12.25),
            ("365", 365.0),
            (" 7 ", 7.0),
        ] {
            assert_eq!(parse_quota(input), Some(expected), "{input}");
        }
        for input in [
            "", "abc", "-1", "366", "1.234", "1e2", " 1 2", "1.", ".5", "1,2,3", "１",
        ] {
            assert_eq!(parse_quota(input), None, "{input}");
        }
    }

    #[test]
    fn email_normal() {
        assert_eq!(normalize_email(" 'A@X.com "), "a@x.com");
        assert_eq!(normalize_email("' b@x.com"), "b@x.com");
    }

    fn row(n: usize, email: &str, quota: &str) -> ParsedRow {
        let mut raw = Map::new();
        raw.insert("email".into(), Value::String(email.into()));
        raw.insert("kuota cuti".into(), Value::String(quota.into()));
        ParsedRow {
            row: n,
            email: email.into(),
            quota: quota.into(),
            raw,
        }
    }

    fn fixture() -> (HashMap<String, Member>, HashMap<i32, Current>) {
        let members = [
            (1, "A", "a@x.com"),
            (2, "B", "b@x.com"),
            (3, "C", "c@x.com"),
        ]
        .into_iter()
        .map(|(id, name, email)| {
            (
                email.to_owned(),
                Member {
                    user_id: id,
                    name: name.into(),
                    email: email.into(),
                },
            )
        })
        .collect();
        let current = [(1, 12.0, 4.0), (2, 10.0, 0.0)]
            .into_iter()
            .map(|(id, total, used)| {
                (
                    id,
                    Current {
                        id: id + 10,
                        annual_leave_total: total,
                        annual_leave_used: used,
                    },
                )
            })
            .collect();
        (members, current)
    }

    #[test]
    fn klasifikasi_update_tetap_dan_saldo_baru() {
        let (m, c) = fixture();
        let res = classify(
            &[
                row(2, "A@X.com", "14"),
                row(3, "b@x.com", "10"),
                row(4, "'c@x.com", "6"),
            ],
            &m,
            &c,
        );
        assert_eq!(
            serde_json::to_value(&res.to_update).unwrap(),
            serde_json::json!([
                {"row": 2, "userId": 1, "name": "A", "email": "a@x.com", "quota": 14, "previous": 12},
                {"row": 4, "userId": 3, "name": "C", "email": "c@x.com", "quota": 6, "previous": null},
            ])
        );
        assert_eq!(
            serde_json::to_value(&res.unchanged).unwrap(),
            serde_json::json!([{"row": 3, "userId": 2, "name": "B", "email": "b@x.com", "quota": 10}])
        );
        assert!(res.invalid.is_empty());
    }

    #[test]
    fn klasifikasi_baris_tidak_valid() {
        let (m, c) = fixture();
        let res = classify(
            &[
                row(2, "zzz@x.com", "5"),
                row(3, "b@x.com", "5"),
                row(4, "b@x.com", "6"),
                row(5, "c@x.com", "abc"),
                row(6, "a@x.com", "3"),
                row(7, "", "1"),
            ],
            &m,
            &c,
        );
        assert_eq!(res.to_update.iter().map(|x| x.row).collect::<Vec<_>>(), [3]);
        let invalid: Vec<(usize, Vec<String>)> = res
            .invalid
            .iter()
            .map(|x| (x.row, x.errors.clone()))
            .collect();
        assert_eq!(
            invalid,
            [
                (
                    2,
                    vec!["email tidak ditemukan di karyawan aktif company".to_owned()]
                ),
                (4, vec!["email duplikat dengan baris 3".to_owned()]),
                (
                    5,
                    vec!["kuota cuti harus angka 0–365 (maksimal 2 desimal)".to_owned()]
                ),
                (
                    6,
                    vec![
                        "kuota cuti tidak boleh di bawah cuti yang sudah terpakai (4 hari)"
                            .to_owned()
                    ]
                ),
                (7, vec!["email wajib diisi".to_owned()]),
            ]
        );
    }

    #[test]
    fn deteksi_pemisah() {
        assert_eq!(detect_delimiter(b"a;b\n1,2"), b';');
        assert_eq!(detect_delimiter(b"a;b,c\n"), b',');
        assert_eq!(detect_delimiter(b"a,b\n1;2"), b',');
        assert_eq!(detect_delimiter(b"email"), b',');
    }
}
