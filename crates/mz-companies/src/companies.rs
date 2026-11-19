//! Port `src/companies/companies.{controller,service}.ts` — 7 route `/companies*` (`POST /companies/:uuid/switch` tidak di-port, tetap dilayani Node).
//!
//! Model ber-audit yang ditulis di sini: `company` (PATCH, DELETE, kode undangan),
//! `companyWorkPattern` (pendamping `company`, lewat PATCH).
//! Semuanya lewat `AuditHandle`.

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Columns, Lookup, Model, Row, read_rows};
use mz_core::extract::{PathParams, Validate, ValidatedJson, ValidatedQuery, query_from_str};
use mz_core::time::now_naive;
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use rand::Rng;
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::common::{PageData, PageQuery, envelope, failure, node_500, paginate};
use crate::dto::{SetVal, SignUrlQuery, UpdateBody, UpdateInput};
use crate::work_pattern::{self, Clock, PatternOp, WorkPatternView};

/// `CompaniesEntity.STATUS_ACTIVE` / `STATUS_DELETED`.
const STATUS_ACTIVE: i32 = 1;
const STATUS_DELETED: i32 = 0;
/// `ACTIVE_MEMBERSHIP` (`resolve-company-id.ts`).
const ACTIVE_MEMBERSHIP: i32 = 1;

/// 55 karakter tanpa yang mirip (I, O, l, 0, 1) — `invite-code.ts`.
const INVITE_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
const INVITE_CODE_LENGTH: usize = 10;

/// `generateInviteCode`: kode acak CSPRNG (`ThreadRng` = ChaCha, `random_range` tanpa modulo bias).
pub fn generate_invite_code() -> String {
    let mut rng = rand::rng();
    (0..INVITE_CODE_LENGTH)
        .map(|_| {
            let i = rng.random_range(0..INVITE_CODE_ALPHABET.len());
            INVITE_CODE_ALPHABET.get(i).map_or('A', |b| char::from(*b))
        })
        .collect()
}

/// `{ ...toCompanyResponse(row), workPattern? }` dengan urutan kolom Prisma.
struct CompanyData {
    row: Row,
    work_pattern: Option<WorkPatternView>,
}

impl Serialize for CompanyData {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        for (k, v) in self.row.iter() {
            map.serialize_entry(k, v)?;
        }
        if let Some(wp) = &self.work_pattern {
            map.serialize_entry("workPattern", wp)?;
        }
        map.end()
    }
}

/// `toCompanyResponse`: buang kolom yang tidak boleh keluar.
fn to_company_response(mut row: Row) -> Row {
    row.remove("profilePictureFullPath");
    row
}

async fn read_company(conn: &mut PgConnection, lookup: Lookup) -> sqlx::Result<Option<Row>> {
    Ok(read_rows(
        &mut *conn,
        Model::Company,
        Columns::All,
        &lookup,
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next())
}

/// `userBelongsToCompany`: keanggotaan berstatus apa pun.
async fn user_belongs_to_company(state: &AppState, user_id: i32, uuid: &str) -> sqlx::Result<bool> {
    let hit: Option<i32> = sqlx::query_scalar(
        r#"SELECT uc.id FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
           WHERE uc."userId" = $1 AND c.uuid = $2::uuid LIMIT 1"#,
    )
    .bind(user_id)
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    Ok(hit.is_some())
}

fn no_access() -> ApiError {
    ApiError::unauthorized("You do not have access to this company")
}

// ───────────────────────── GET /companies ─────────────────────────

/// Node: src/companies/companies.controller.ts:88, service :199 (`findAll`).
pub async fn find_all(
    State(state): State<AppState>,
    ValidatedQuery(query): ValidatedQuery<PageQuery>,
) -> ApiResult<Response> {
    let args = query.args();
    let search = args.search.clone().filter(|s| !s.is_empty());
    let page = paginate(
        &state,
        Model::Company,
        move |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push("status = ").push_bind(STATUS_ACTIVE);
            if let Some(s) = &search {
                let pattern = format!("%{s}%");
                qb.push(" AND (name ILIKE ").push_bind(pattern.clone());
                qb.push(" OR telp ILIKE ").push_bind(pattern);
                qb.push(")");
            }
        },
        &args,
    )
    .await?;
    let data = PageData {
        total: page.total,
        items_per_page: page.items_per_page,
        page_number: page.page_number,
        result: page.rows.into_iter().map(to_company_response).collect(),
    };
    Ok(Json(envelope("Success get all data company", data)).into_response())
}

// ───────────────────────── GET /companies/:uuid ─────────────────────────

/// Node: src/companies/companies.controller.ts:102, service :267 (`findOne`).
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    let user_id = ctx.require_user_id()?;
    if !user_belongs_to_company(&state, user_id, &uuid).await? {
        return Err(no_access());
    }
    let mut conn = state.db.acquire().await?;
    let row = read_rows(
        &mut *conn,
        Model::Company,
        Columns::All,
        &Lookup::filter({
            let uuid = uuid.clone();
            move |qb| {
                qb.push("uuid = ")
                    .push_bind(uuid.clone())
                    .push("::uuid AND status = ");
                qb.push_bind(STATUS_ACTIVE);
            }
        }),
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next();
    let Some(row) = row else {
        return Ok(Json(failure(format!("Data company by uuid {uuid} not found"))).into_response());
    };
    let id = row
        .int("id")
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or_default();
    let data = CompanyData {
        row: to_company_response(row),
        work_pattern: Some(work_pattern::view(&state, id).await?),
    };
    Ok(Json(envelope(
        format!("Success get data company by uuid {uuid}"),
        data,
    ))
    .into_response())
}

// ───────────────────────── PATCH /companies/:uuid ─────────────────────────

/// `UPDATE company SET ... WHERE uuid = $` — `data` Prisma (selalu memuat `updatedAt`).
async fn write_update(
    conn: &mut PgConnection,
    uuid: &str,
    sets: &[(&'static str, SetVal)],
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE company SET ");
    for (col, val) in sets {
        qb.push(format!(r#""{col}" = "#));
        match val {
            SetVal::Text(v) => qb.push_bind(v.clone()),
            SetVal::Int(v) => qb.push_bind(*v),
            SetVal::Bool(v) => qb.push_bind(*v),
            SetVal::Time(v) => qb.push_bind(*v),
            SetVal::TextList(v) => qb.push_bind(v.clone()),
            SetVal::Decimal(v) => {
                qb.push_bind(v.clone());
                qb.push("::numeric")
            }
        };
        qb.push(", ");
    }
    qb.push(r#""updatedAt" = "#).push_bind(now);
    qb.push(" WHERE uuid = ")
        .push_bind(uuid.to_owned())
        .push("::uuid");
    Ok(qb.build().execute(conn).await?.rows_affected())
}

/// `invalidateModuleCaches`: modul-fitur company (lokal + pesan `company-modules`) dan
/// `user-me-<id>` SELURUH anggota company. Di-`await` (dashboard memanggil ulang
/// `GET /auth/me` begitu PATCH menjawab); galat hanya dicatat.
async fn invalidate_module_caches(state: &AppState, company_id: i32) {
    mz_core::feature_modules::invalidate_company(state, company_id);
    let members: Result<Vec<i32>, _> =
        sqlx::query_scalar(r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1"#)
            .bind(company_id)
            .fetch_all(&state.db)
            .await;
    match members {
        Ok(ids) => {
            for id in ids {
                if let Err(e) = mz_core::invalidation::try_forget_user_me(state, id).await {
                    tracing::warn!(company_id, error = %e, "Gagal membuang cache user-me");
                }
            }
        }
        Err(e) => {
            tracing::warn!(company_id, error = %e, "Gagal membuang cache user-me untuk company");
        }
    }
}

/// Node: src/companies/companies.controller.ts:123, service :288 (`update`).
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<UpdateBody>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    let user_id = ctx.require_user_id()?;
    if !user_belongs_to_company(&state, user_id, &uuid).await? {
        return Err(no_access());
    }
    let input = body.to_input()?;
    let now = now_naive();

    // Validasi pola SEBELUM menulis apa pun: 422 tidak boleh meninggalkan perubahan.
    let pattern_op =
        work_pattern::validate(input.work_pattern.as_ref(), input.cancel_scheduled.as_ref())?;
    let has = |col: &str| input.sets.iter().any(|(c, _)| *c == col);
    let touches_duration = has("storeVisitMinDurationMinutes") || has("storeVisitMaxDurationHours");
    let touches_week_start = has("workweekStartDay");

    let updated = if !touches_duration && pattern_op.is_none() && !touches_week_start {
        let mut conn = state.db.acquire().await?;
        update_row(&mut conn, &audit, &uuid, &input, now).await?
    } else {
        // §9.1 — transaksi yang rollback tidak boleh meninggalkan baris audit yang berbohong.
        let result = update_in_tx(
            &state,
            &audit,
            &uuid,
            &input,
            pattern_op.as_ref(),
            user_id,
            now,
        )
        .await;
        match result {
            Ok(row) => row,
            Err(e) => {
                audit.discard();
                return Err(e);
            }
        }
    };

    let company_id = updated
        .int("id")
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or_default();
    if input.modules.is_some() {
        invalidate_module_caches(&state, company_id).await;
    }
    let data = CompanyData {
        row: to_company_response(updated),
        work_pattern: Some(work_pattern::view(&state, company_id).await?),
    };
    Ok(Json(envelope("update company success", data)).into_response())
}

/// `company.update({ where: { uuid }, data })` + audit; mengembalikan baris hasil (semua kolom).
async fn update_row(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    uuid: &str,
    input: &UpdateInput,
    now: NaiveDateTime,
) -> ApiResult<Row> {
    let pre = audit
        .pre_read(Model::Company, Lookup::Uuid(uuid.to_owned()))
        .await;
    let affected = write_update(&mut *conn, uuid, &input.sets, now).await?;
    if affected == 0 {
        // Prisma P2025 (`Record to update not found`) → 500.
        return Err(node_500("Record to update not found"));
    }
    let row = read_company(&mut *conn, Lookup::Uuid(uuid.to_owned()))
        .await?
        .ok_or_else(|| node_500("Record to update not found"))?;
    audit.record_update(Model::Company, pre, &row).await;
    Ok(row)
}

#[allow(clippy::too_many_arguments)]
async fn update_in_tx(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    input: &UpdateInput,
    pattern_op: Option<&PatternOp>,
    actor_id: i32,
    now: NaiveDateTime,
) -> ApiResult<Row> {
    let mut tx = state.db.begin().await?;
    // TOCTOU T7: baris company dikunci sebelum dibaca.
    let current: Option<(i32, i32, i32, i32)> = sqlx::query_as(
        r#"SELECT id, "workweekStartDay", "storeVisitMinDurationMinutes", "storeVisitMaxDurationHours"
           FROM company WHERE uuid = $1::uuid FOR UPDATE"#,
    )
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, week_start_before, min_before, max_before)) = current else {
        return Err(ApiError::not_found("company not found"));
    };

    let int_of = |col: &str| {
        input.sets.iter().find_map(|(c, v)| match v {
            SetVal::Int(n) if *c == col => Some(*n),
            _ => None,
        })
    };
    if input.sets.iter().any(|(c, _)| {
        matches!(
            *c,
            "storeVisitMinDurationMinutes" | "storeVisitMaxDurationHours"
        )
    }) {
        let min_minutes = int_of("storeVisitMinDurationMinutes").unwrap_or(min_before);
        let max_hours = int_of("storeVisitMaxDurationHours").unwrap_or(max_before);
        if min_minutes > max_hours * 60 {
            let mut body = serde_json::Map::new();
            body.insert("code".into(), json!("STORE_VISIT_MIN_DURATION_TOO_LONG"));
            body.insert(
                "message".into(),
                json!("storeVisitMinDurationMinutes must not exceed storeVisitMaxDurationHours"),
            );
            body.insert(
                "details".into(),
                json!({ "minDurationMinutes": min_minutes, "maxDurationHours": max_hours }),
            );
            return Err(ApiError::body(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                Value::Object(body),
            ));
        }
    }

    let updated = update_row(&mut tx, audit, uuid, input, now).await?;

    // Baris PASCA-update: week start baru menentukan tanggal berlaku pola.
    let clock = Clock {
        id: updated
            .int("id")
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or_default(),
        work_week_start_day: updated
            .int("workweekStartDay")
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(week_start_before),
        time_zone: updated.text("timeZone").map(str::to_owned),
    };
    match pattern_op {
        Some(op) => {
            work_pattern::schedule(audit, &mut tx, &clock, op, Some(actor_id), now).await?;
        }
        None if clock.work_week_start_day != week_start_before => {
            work_pattern::realign_scheduled(audit, &mut tx, &clock, Some(actor_id), now).await?;
        }
        None => {}
    }
    tx.commit().await?;
    Ok(updated)
}

// ───────────────────────── DELETE /companies/:uuid ─────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Deleted {
    is_deleted: bool,
}

/// Node: src/companies/companies.controller.ts:155, service :433 (`remove`) — soft delete
/// (`status = 0`) lewat satu `update` (tanpa transaksi, tanpa `updatedAt`).
pub async fn remove(
    State(state): State<AppState>,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    let mut conn = state.db.acquire().await?;
    let pre = audit
        .pre_read(Model::Company, Lookup::Uuid(uuid.clone()))
        .await;
    let affected = sqlx::query("UPDATE company SET status = $2 WHERE uuid = $1::uuid")
        .bind(&uuid)
        .bind(STATUS_DELETED)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(node_500("Record to update not found"));
    }
    if let Some(row) = read_company(&mut conn, Lookup::Uuid(uuid)).await? {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(Json(envelope(
        "delete company success",
        Deleted { is_deleted: true },
    ))
    .into_response())
}

// ───────────────────────── invite link ─────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InviteLink {
    invite_code: String,
    invite_link: String,
}

/// `getInviteLink` / `regenerateInviteLink` (isi keduanya identik di Node: tidak ada yang
/// benar-benar membuat kode baru bila sudah ada).
async fn invite_link(state: &AppState, audit: &AuditHandle, user_id: i32) -> ApiResult<Response> {
    let membership: Option<(i32, Option<String>)> = sqlx::query_as(
        r#"SELECT uc."companyId", c."inviteCode" FROM "userCompany" uc
           JOIN company c ON c.id = uc."companyId"
           WHERE uc."userId" = $1 AND uc.status = $2
           ORDER BY uc."isDefault" DESC, uc.id ASC LIMIT 1"#,
    )
    .bind(user_id)
    .bind(ACTIVE_MEMBERSHIP)
    .fetch_optional(&state.db)
    .await?;
    let Some((company_id, current)) = membership else {
        return Err(ApiError::not_found("Company tidak ditemukan."));
    };

    let code = match current.filter(|c| !c.is_empty()) {
        Some(code) => code,
        None => {
            let code = loop {
                let candidate = generate_invite_code();
                let taken: Option<i32> =
                    sqlx::query_scalar(r#"SELECT id FROM company WHERE "inviteCode" = $1"#)
                        .bind(&candidate)
                        .fetch_optional(&state.db)
                        .await?;
                if taken.is_none() {
                    break candidate;
                }
            };
            let mut conn = state.db.acquire().await?;
            let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
            sqlx::query(r#"UPDATE company SET "inviteCode" = $1 WHERE id = $2"#)
                .bind(&code)
                .bind(company_id)
                .execute(&mut *conn)
                .await?;
            if let Some(row) = read_company(&mut conn, Lookup::Id(company_id)).await? {
                audit.record_update(Model::Company, pre, &row).await;
            }
            code
        }
    };
    let base = state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "undefined".to_owned());
    Ok(Json(InviteLink {
        invite_link: format!("{base}/join/{code}"),
        invite_code: code,
    })
    .into_response())
}

/// Node: src/companies/companies.controller.ts (`regenerateInviteLink`).
///
/// `GET /companies/invite-link` (handler kembar tanpa `@Roles`) sudah dihapus di Node: route itu
/// selalu tertutup `GET :uuid`, dan menghidupkannya akan membuka kode undangan ke STAFF/GUEST.
pub async fn regenerate_invite_link(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Response> {
    invite_link(&state, &audit, ctx.require_user_id()?).await
}

// ───────────────────────── GET /companies/:uuid/profile-picture/presigned-url ─────────────────────────

/// Node: src/companies/companies.controller.ts:187 (`signUrl`).
pub async fn sign_url(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    // `@Max(parseInt(process.env.MAX_SIZE_FILE_UPLOAD))` dibaca dari konfigurasi, jadi
    // validasi dijalankan di sini (bukan di extractor).
    let query =
        query_from_str::<SignUrlQuery>(raw.as_deref().unwrap_or_default())?.with_env(&state);
    let errors = query.validate();
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    let key = format!("/companies/{uuid}/{}", query.file_name());
    let content_type = if query.extension() == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let signed = mz_files::get_signed_file_url(&state, &key, 3600, content_type).await?;
    Ok(Json(signed).into_response())
}
