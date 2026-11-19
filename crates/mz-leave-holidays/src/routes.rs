//! Port `src/leave-holidays/leave-holidays.{controller,service}.ts` +
//! `holiday-import.service.ts` (bagian DB): 10 route `/leave-holidays*`.
//!
//! Efek samping: tiap perubahan libur memasang job `leave.holiday-sync` (potongan saldo
//! libur + hitung ulang cuti disetujui) SESUDAH commit lewat `mz_leave::jobs`.
//! `companyHoliday` tidak ber-audit dan tidak punya cache.

use std::collections::HashMap;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_admin_jobs::validate::{RawBody, RawQuery};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_leave::jobs::{HolidaySyncPayload, enqueue_holiday_sync};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::dto::{self, CreateDto, UpdateDto};
use crate::holiday::{
    COLS, HolidayRow, HolidayWithJoint, NewHoliday, attach_joint_leaves, date_key, find_with_joint,
    insert_row, joint_leave_name, sort_by_date,
};
use crate::import::{
    self, Classification, ParsedRow, assert_csv_file, classify, read_upload, real_date,
    valid_date_keys,
};

/// Isi `holiday-import.template.csv` (dengan BOM).
const TEMPLATE: &[u8] = include_bytes!("../assets/holiday-import.template.csv");

fn not_found() -> ApiError {
    ApiError::not_found("Holiday not found")
}

/// `resolveCompanyId`: 404 (bukan 403 resolver bersama) — kontrak endpoint cuti.
async fn company_of(state: &AppState, ctx: &RequestCtx) -> ApiResult<i32> {
    ctx.company_id(state)
        .await?
        .ok_or_else(|| ApiError::not_found("User is not associated with any company"))
}

/// `parseInt(id, 10)` yang diteruskan ke Prisma: NaN / di luar Int32 → galat Prisma → 500.
fn parse_id(raw: &str) -> ApiResult<i32> {
    let t = raw.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: Option<i64> = digits.parse().ok();
    n.map(|n| if neg { -n } else { n })
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("holiday id is not a valid Int")))
}

/// `parseDate(value)`: `new Date(value)`; tidak terbaca → 400. Kolom `@db.Date` = tanggal UTC.
fn parse_date(value: &str) -> ApiResult<NaiveDate> {
    mz_core::time::v8_date(value)
        .map(|d| d.date())
        .ok_or_else(|| ApiError::bad_request("Invalid date format"))
}

/// `assertJointDates(keys, holidayKey)`.
fn assert_joint_dates(keys: &[String], holiday_key: &str) -> ApiResult<()> {
    for key in keys {
        parse_date(key)?;
    }
    if keys.iter().any(|k| k == holiday_key) {
        return Err(ApiError::bad_request(
            "Tanggal cuti bersama tidak boleh sama dengan tanggal liburnya",
        ));
    }
    Ok(())
}

fn sync_payload(
    company_id: i32,
    holiday_id: i32,
    previous: Option<String>,
    current: String,
) -> HolidaySyncPayload {
    HolidaySyncPayload {
        company_id,
        holiday_id,
        previous_date: previous,
        current_date: Some(current),
    }
}

/// `enqueueSync(holiday, previousDate, updatedAtMs)` (sesudah commit).
async fn enqueue_sync(state: &AppState, h: &HolidayRow, previous: Option<String>, ms: i64) {
    enqueue_holiday_sync(
        state,
        &sync_payload(h.company_id, h.id, previous, date_key(h.date)),
        ms,
    )
    .await;
}

/// `enqueueRemoval(companyId, holiday, updatedAtMs)`.
async fn enqueue_removal(state: &AppState, company_id: i32, h: &HolidayRow, ms: i64) {
    let key = date_key(h.date);
    enqueue_holiday_sync(
        state,
        &sync_payload(company_id, h.id, Some(key.clone()), key),
        ms,
    )
    .await;
}

fn now_ms(now: NaiveDateTime) -> i64 {
    now.and_utc().timestamp_millis()
}

// ── GET /leave-holidays ─────────────────────────────────────────────────────

/// Node: src/leave-holidays/leave-holidays.controller.ts:117 (`list`) +
/// leave-holidays.service.ts:60.
pub async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Vec<HolidayWithJoint>>> {
    let query = dto::validate_query(&q)?;
    let company_id = company_of(&state, &ctx).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        r#"SELECT {COLS} FROM "companyHoliday" WHERE "companyId" = "#
    ));
    qb.push_bind(company_id);
    if let Some(v) = query.is_active {
        qb.push(r#" AND "isActive" = "#).push_bind(v);
    }
    if let Some(v) = query.deducts_leave {
        qb.push(r#" AND "deductsLeave" = "#).push_bind(v);
    }
    if let Some(t) = &query.ty {
        qb.push(" AND type = ")
            .push_bind(t.clone())
            .push(r#"::"HolidayType""#);
    }
    if let Some(y) = query.year
        && let (Some(from), Some(to)) = (
            NaiveDate::from_ymd_opt(y, 1, 1),
            NaiveDate::from_ymd_opt(y, 12, 31),
        )
    {
        qb.push(" AND date >= ").push_bind(from);
        qb.push(" AND date <= ").push_bind(to);
    }
    qb.push(" ORDER BY date ASC, id ASC");

    let mut conn = state.db.acquire().await?;
    let rows: Vec<HolidayRow> = qb.build_query_as().fetch_all(&mut *conn).await?;
    // Baris tetap datar; tiap induk membawa cuti bersamanya (bisa di tahun lain).
    Ok(Json(attach_joint_leaves(&mut conn, rows).await?))
}

// ── GET /leave-holidays/:id ─────────────────────────────────────────────────

pub(crate) async fn find_one(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
) -> ApiResult<Json<HolidayWithJoint>> {
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    let mut conn = state.db.acquire().await?;
    find_with_joint(&mut conn, id, company_id)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:229 (`findOne`).
pub async fn h_get(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<HolidayWithJoint>> {
    find_one(&state, &ctx, &id).await
}

/// `GET /leave-holidays/import` dan `/next-year-reminder` yang tidak terdaftar sebagai GET
/// jatuh ke `GET :id` di Nest (`parseInt` → NaN → 500).
pub async fn h_get_import(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<HolidayWithJoint>> {
    find_one(&state, &ctx, "import").await
}

// ── POST /leave-holidays ────────────────────────────────────────────────────

/// Node: src/leave-holidays/leave-holidays.controller.ts:137 (`create`) +
/// leave-holidays.service.ts:98.
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<HolidayWithJoint>)> {
    let dto: CreateDto = dto::validate_create(&body)?;
    let company_id = company_of(&state, &ctx).await?;

    let date = parse_date(&dto.date)?;
    let ty = dto
        .ty
        .clone()
        .unwrap_or_else(|| "NATIONAL_HOLIDAY".to_owned());
    let joint = dto.joint_leave_dates.clone().unwrap_or_default();
    if !joint.is_empty() && ty == "JOINT_LEAVE" {
        return Err(ApiError::bad_request(
            "Cuti bersama tidak bisa memiliki cuti bersama sendiri",
        ));
    }
    assert_joint_dates(&joint, &dto.date)?;

    let mut tx = state.db.begin().await?;
    let parent = insert_row(
        &mut tx,
        &NewHoliday {
            company_id,
            parent_holiday_id: None,
            name: &dto.name,
            date,
            ty: &ty,
            is_active: None,
            deducts_leave: dto.deducts_leave.unwrap_or(ty == "JOINT_LEAVE"),
        },
    )
    .await?;
    let mut children = Vec::with_capacity(joint.len());
    for key in &joint {
        children.push(
            insert_row(
                &mut tx,
                &NewHoliday {
                    company_id,
                    parent_holiday_id: Some(parent.id),
                    name: &joint_leave_name(&parent.name),
                    date: parse_date(key)?,
                    ty: "JOINT_LEAVE",
                    is_active: Some(parent.is_active),
                    deducts_leave: dto.joint_leave_deducts_leave.unwrap_or(true),
                },
            )
            .await?,
        );
    }
    tx.commit().await?;

    let ms = now_ms(mz_core::time::now_naive());
    enqueue_sync(&state, &parent, None, ms).await;
    for c in &children {
        enqueue_sync(&state, c, None, ms).await;
    }
    Ok((
        StatusCode::CREATED,
        Json(HolidayWithJoint {
            holiday: parent,
            joint_leaves: sort_by_date(children),
        }),
    ))
}

// ── PUT /leave-holidays/:id ─────────────────────────────────────────────────

/// Node: src/leave-holidays/leave-holidays.service.ts:195 (`update`).
pub(crate) async fn update(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
    dto: UpdateDto,
) -> ApiResult<Json<HolidayWithJoint>> {
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    let mut conn = state.db.acquire().await?;
    let holiday = find_with_joint(&mut conn, id, company_id)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    let (parent, existing) = (holiday.holiday, holiday.joint_leaves);

    let touches_joint = dto.joint_leave_dates.is_some() || dto.joint_leave_deducts_leave.is_some();
    let next_type = dto.ty.clone().unwrap_or_else(|| parent.ty.clone());
    if touches_joint && (parent.parent_holiday_id.is_some() || next_type == "JOINT_LEAVE") {
        return Err(ApiError::bad_request(
            "Cuti bersama tidak bisa memiliki cuti bersama sendiri",
        ));
    }
    if next_type == "JOINT_LEAVE" && !existing.is_empty() {
        return Err(ApiError::bad_request(
            "Hapus dulu cuti bersama libur ini sebelum mengubah jenisnya",
        ));
    }

    let new_date = dto.date.as_deref().map(parse_date).transpose()?;
    let parent_key = dto.date.clone().unwrap_or_else(|| date_key(parent.date));
    let next_dates: Vec<String> = dto
        .joint_leave_dates
        .clone()
        .unwrap_or_else(|| existing.iter().map(|j| date_key(j.date)).collect());
    assert_joint_dates(&next_dates, &parent_key)?;

    // Satu baris = satu hari cuti bersama. Tanggal yang dipertahankan tidak dihapus-buat
    // ulang supaya potongan saldo APPLIED tetap menunjuk baris yang sama.
    let existing_keys: Vec<String> = existing.iter().map(|j| date_key(j.date)).collect();
    let removed: Vec<&HolidayRow> = existing
        .iter()
        .filter(|j| !next_dates.contains(&date_key(j.date)))
        .collect();
    let added: Vec<&String> = next_dates
        .iter()
        .filter(|k| !existing_keys.contains(k))
        .collect();
    let kept: Vec<&HolidayRow> = existing
        .iter()
        .filter(|j| next_dates.contains(&date_key(j.date)))
        .collect();

    let mut tx = state.db.begin().await?;
    let stamp = mz_core::time::now_naive();
    let updated: HolidayRow = sqlx::query_as(&format!(
        r#"UPDATE "companyHoliday" SET
              name = COALESCE($2, name),
              date = COALESCE($3, date),
              type = COALESCE($4::"HolidayType", type),
              "deductsLeave" = COALESCE($5, "deductsLeave"),
              "updatedAt" = $6
            WHERE id = $1 RETURNING {COLS}"#
    ))
    .bind(id)
    .bind(dto.name.as_deref())
    .bind(new_date)
    .bind(dto.ty.as_deref())
    .bind(dto.deducts_leave)
    .bind(stamp)
    .fetch_one(&mut *tx)
    .await?;
    if !removed.is_empty() {
        let ids: Vec<i32> = removed.iter().map(|j| j.id).collect();
        sqlx::query(r#"DELETE FROM "companyHoliday" WHERE id = ANY($1)"#)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }
    let mut changed: Vec<HolidayRow> = Vec::new();
    for j in &kept {
        let new_name = dto
            .name
            .as_deref()
            .map(joint_leave_name)
            .filter(|n| *n != j.name);
        let new_deducts = dto
            .joint_leave_deducts_leave
            .filter(|d| *d != j.deducts_leave);
        if new_name.is_none() && new_deducts.is_none() {
            continue;
        }
        changed.push(
            sqlx::query_as(&format!(
                r#"UPDATE "companyHoliday" SET
                      name = COALESCE($2, name),
                      "deductsLeave" = COALESCE($3, "deductsLeave"),
                      "updatedAt" = $4
                    WHERE id = $1 RETURNING {COLS}"#
            ))
            .bind(j.id)
            .bind(new_name)
            .bind(new_deducts)
            .bind(mz_core::time::now_naive())
            .fetch_one(&mut *tx)
            .await?,
        );
    }
    let deducts = dto
        .joint_leave_deducts_leave
        .or_else(|| kept.first().map(|j| j.deducts_leave))
        .or_else(|| removed.first().map(|j| j.deducts_leave))
        .unwrap_or(true);
    let mut created: Vec<HolidayRow> = Vec::new();
    for key in &added {
        created.push(
            insert_row(
                &mut tx,
                &NewHoliday {
                    company_id: updated.company_id,
                    parent_holiday_id: Some(updated.id),
                    name: &joint_leave_name(&updated.name),
                    date: parse_date(key)?,
                    ty: "JOINT_LEAVE",
                    is_active: Some(updated.is_active),
                    deducts_leave: deducts,
                },
            )
            .await?,
        );
    }
    tx.commit().await?;

    let now = now_ms(mz_core::time::now_naive());
    let previous_key = date_key(parent.date);
    let moved = previous_key != date_key(updated.date);
    let updated_ms = updated.updated_at.map_or(now, now_ms);
    enqueue_sync(state, &updated, moved.then_some(previous_key), updated_ms).await;
    for j in &removed {
        enqueue_removal(state, company_id, j, now).await;
    }
    for j in changed.iter().chain(created.iter()) {
        enqueue_sync(state, j, None, now).await;
    }

    let changed_by_id: HashMap<i32, &HolidayRow> = changed.iter().map(|j| (j.id, j)).collect();
    let mut joint_leaves: Vec<HolidayRow> = kept
        .iter()
        .map(|j| {
            changed_by_id
                .get(&j.id)
                .map_or_else(|| (*j).clone(), |c| (*c).clone())
        })
        .collect();
    joint_leaves.extend(created);
    Ok(Json(HolidayWithJoint {
        holiday: updated,
        joint_leaves: sort_by_date(joint_leaves),
    }))
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:247 (`update`).
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<HolidayWithJoint>> {
    let dto = dto::validate_update(&body)?;
    update(&state, &ctx, &id, dto).await
}

/// `PUT /leave-holidays/import` (jatuh ke `PUT :id`).
pub async fn h_update_import(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawBody(body): RawBody,
) -> ApiResult<Json<HolidayWithJoint>> {
    let dto = dto::validate_update(&body)?;
    update(&state, &ctx, "import", dto).await
}

/// `PUT /leave-holidays/next-year-reminder` (jatuh ke `PUT :id`).
pub async fn h_update_reminder(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawBody(body): RawBody,
) -> ApiResult<Json<HolidayWithJoint>> {
    let dto = dto::validate_update(&body)?;
    update(&state, &ctx, "next-year-reminder", dto).await
}

// ── DELETE /leave-holidays/:id ──────────────────────────────────────────────

/// Node: src/leave-holidays/leave-holidays.service.ts:290 (`remove`).
pub(crate) async fn remove(
    state: &AppState,
    ctx: &RequestCtx,
    raw_id: &str,
) -> ApiResult<Json<Value>> {
    let company_id = company_of(state, ctx).await?;
    let id = parse_id(raw_id)?;
    let mut conn = state.db.acquire().await?;
    let holiday = find_with_joint(&mut conn, id, company_id)
        .await?
        .ok_or_else(not_found)?;
    // FK ON DELETE CASCADE ikut menghapus cuti bersamanya.
    let res = sqlx::query(r#"DELETE FROM "companyHoliday" WHERE id = $1"#)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    drop(conn);
    mz_core::db::expect_one(res.rows_affected(), "companyHoliday")?;
    let now = now_ms(mz_core::time::now_naive());
    enqueue_removal(state, company_id, &holiday.holiday, now).await;
    for j in &holiday.joint_leaves {
        enqueue_removal(state, company_id, j, now).await;
    }
    Ok(Json(json!({ "success": true })))
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:267 (`remove`).
pub async fn h_delete(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    remove(&state, &ctx, &id).await
}

pub async fn h_delete_import(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    remove(&state, &ctx, "import").await
}

pub async fn h_delete_reminder(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    remove(&state, &ctx, "next-year-reminder").await
}

// ── PATCH /leave-holidays/:id/toggle ────────────────────────────────────────

/// Node: src/leave-holidays/leave-holidays.service.ts:306 (`toggleActive`).
pub async fn h_toggle(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<HolidayRow>> {
    let company_id = company_of(&state, &ctx).await?;
    let id = parse_id(&raw_id)?;
    let mut conn = state.db.acquire().await?;
    let holiday = find_with_joint(&mut conn, id, company_id)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    let is_active = !holiday.holiday.is_active;
    // Cuti bersama ikut status induknya.
    let mut tx = state.db.begin().await?;
    let set_active = format!(
        r#"UPDATE "companyHoliday" SET "isActive" = $2, "updatedAt" = $3 WHERE id = $1 RETURNING {COLS}"#
    );
    let toggled: HolidayRow = sqlx::query_as(&set_active)
        .bind(id)
        .bind(is_active)
        .bind(mz_core::time::now_naive())
        .fetch_one(&mut *tx)
        .await?;
    let mut children: Vec<HolidayRow> = Vec::new();
    for j in &holiday.joint_leaves {
        if j.is_active == is_active {
            continue;
        }
        children.push(
            sqlx::query_as(&set_active)
                .bind(j.id)
                .bind(is_active)
                .bind(mz_core::time::now_naive())
                .fetch_one(&mut *tx)
                .await?,
        );
    }
    tx.commit().await?;
    let now = now_ms(mz_core::time::now_naive());
    enqueue_sync(
        &state,
        &toggled,
        None,
        toggled.updated_at.map_or(now, now_ms),
    )
    .await;
    for j in &children {
        enqueue_sync(&state, j, None, now).await;
    }
    Ok(Json(toggled))
}

// ── impor CSV ───────────────────────────────────────────────────────────────

/// `existingActiveByDate(client, companyId, dateKeys)`: libur AKTIF per tanggal (nama baris
/// pertama menurut id).
async fn existing_active_by_date(
    conn: &mut PgConnection,
    company_id: i32,
    rows: &[ParsedRow],
) -> sqlx::Result<HashMap<String, String>> {
    let mut result = HashMap::new();
    let dates = valid_date_keys(rows);
    if dates.is_empty() {
        return Ok(result);
    }
    let found: Vec<(NaiveDate, String)> = sqlx::query_as(
        r#"SELECT date, name FROM "companyHoliday"
            WHERE "companyId" = $1 AND "isActive" = true AND date = ANY($2)
            ORDER BY id ASC"#,
    )
    .bind(company_id)
    .bind(&dates)
    .fetch_all(&mut *conn)
    .await?;
    for (date, name) in found {
        result.entry(date_key(date)).or_insert(name);
    }
    Ok(result)
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:166 (`downloadImportTemplate`).
pub async fn h_template() -> ApiResult<Response> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"contoh-impor-libur.csv\"",
        )
        .body(Body::from(TEMPLATE))
        .map_err(ApiError::internal)
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:195 (`previewImport`) +
/// holiday-import.service.ts `preview`. Tidak menulis apa pun.
pub async fn h_preview(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Json<Value>> {
    let upload = read_upload(req).await?;
    let company_id = company_of(&state, &ctx).await?;
    assert_csv_file(upload.as_ref())?;
    let rows = import::parse(upload.as_ref().map_or(&[][..], |u| u.bytes.as_slice()))?;
    let mut conn = state.db.acquire().await?;
    let existing = existing_active_by_date(&mut conn, company_id, &rows).await?;
    let result: Classification = classify(&rows, &existing);
    Ok(Json(json!({ "data": result })))
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:226 (`importHolidays`) +
/// leave-holidays.service.ts:146. Klasifikasi diulang DI DALAM transaksi (kunci advisory
/// per company sama dengan `POST /admin/national-holidays/apply`).
pub async fn h_import(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let upload = read_upload(req).await?;
    assert_csv_file(upload.as_ref())?;
    let rows = import::parse(upload.as_ref().map_or(&[][..], |u| u.bytes.as_slice()))?;
    let company_id = company_of(&state, &ctx).await?;

    let mut tx = state.db.begin().await?;
    mz_core::db::advisory_xact_lock(&mut tx, &format!("holiday-import:{company_id}")).await?;
    let existing = existing_active_by_date(&mut tx, company_id, &rows).await?;
    let classification = classify(&rows, &existing);
    let created: Vec<HolidayRow> = if classification.to_add.is_empty() {
        Vec::new()
    } else {
        let names: Vec<&str> = classification
            .to_add
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        let dates: Vec<NaiveDate> = classification
            .to_add
            .iter()
            .filter_map(|r| real_date(&r.date))
            .collect();
        let types: Vec<&str> = classification.to_add.iter().map(|r| r.ty).collect();
        let deducts: Vec<bool> = classification
            .to_add
            .iter()
            .map(|r| r.deducts_leave)
            .collect();
        sqlx::query_as(&format!(
            r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
               SELECT $1, u.name, u.date, u.type::"HolidayType", u.deducts
                 FROM UNNEST($2::text[], $3::date[], $4::text[], $5::bool[])
                   AS u(name, date, type, deducts)
               RETURNING {COLS}"#
        ))
        .bind(company_id)
        .bind(&names)
        .bind(&dates)
        .bind(&types)
        .bind(&deducts)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;

    // Jalur yang sama dengan create: cuti bersama yang memotong cuti menggeser saldo.
    let now = now_ms(mz_core::time::now_naive());
    for h in &created {
        enqueue_sync(&state, h, None, now).await;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": {
            "created": created.len(),
            "skippedExisting": classification.skipped_existing.len(),
            "invalid": classification.invalid.len(),
            "holidays": sort_by_date(created),
        }})),
    ))
}

/// `nextYearReminder(requesterId, now)`.
pub async fn next_year_reminder(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
) -> ApiResult<Value> {
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?
        .flatten();
    let local = mz_leave::policy::local_now(now, tz.as_deref().unwrap_or("UTC"));
    let year = local.year() + 1;
    if local.month() != 12 {
        return Ok(json!({ "show": false, "year": year, "reason": null }));
    }
    let count: i64 = match (
        NaiveDate::from_ymd_opt(year, 1, 1),
        NaiveDate::from_ymd_opt(year, 12, 31),
    ) {
        (Some(from), Some(to)) => {
            sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM "companyHoliday"
                    WHERE "companyId" = $1 AND "isActive" = true AND date >= $2 AND date <= $3"#,
            )
            .bind(company_id)
            .bind(from)
            .bind(to)
            .fetch_one(&state.db)
            .await?
        }
        _ => 0,
    };
    let show = count == 0;
    Ok(json!({
        "show": show,
        "year": year,
        "reason": if show { Value::from("NEXT_YEAR_EMPTY") } else { Value::Null },
    }))
}

/// Node: src/leave-holidays/leave-holidays.controller.ts:252 (`nextYearReminder`).
pub async fn h_reminder(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let company_id = company_of(&state, &ctx).await?;
    let data = next_year_reminder(&state, company_id, mz_core::time::now_naive()).await?;
    Ok(Json(json!({ "data": data })))
}
