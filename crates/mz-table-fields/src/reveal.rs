//! Reveal per baris (`table-field-reveal.service.ts`, §7) + riwayat reveal (§8.1).
//!
//! Server memutuskan ulang mode kolom (kebijakan dibaca segar dari DB, bukan cache), barisnya
//! dicari resolver milik modul pemilik tabel (scope + company), kuota atomik, jejak
//! `fieldRevealLog` ditulis SEBELUM nilai dikirim. `@Throttle(20/menit)` Node TIDAK berefek
//! (ThrottlerGuard tidak pernah terdaftar global), jadi tidak ada limiter di sini; yang
//! membatasi adalah kuota harian.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use axum::Json;
use axum::extract::State;
use chrono::{Days, NaiveDate, NaiveDateTime, TimeZone, Utc};
use futures::future::BoxFuture;
use http::StatusCode;
use mz_audit::AuditHandle;
use mz_core::enums::PermissionAction;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::permissions::LEGACY_OPEN_FEATURES;
use mz_core::tz::zone_or_utc;
use mz_core::{ApiError, AppState, RequestCtx};
use serde::Serialize;
use serde_json::{Value, json};

use crate::apply::resolve_response_path;
use crate::config::Data;
use crate::dto::{LogsQuery, RevealBody};
use crate::error::ReqError;
use crate::policy::policy_for_request;
use crate::quota::{DAILY_REVEAL_QUOTA, RevealQuotaKey, consume_reveal_quota};
use crate::registry::{FieldVisibilityMode, TABLES, column_of, table_by_key};

/// `fieldRevealLog.actorName` / `.recordLabel` — VarChar(160).
const LABEL_MAX: usize = 160;
/// `fieldRevealLog.actorRole` — VarChar(32).
const ROLE_MAX: usize = 32;
/// `fieldRevealLog.reason` — VarChar(255).
const REASON_MAX: usize = 255;

// ───────────────────────── resolver (§7.3) ─────────────────────────

/// Permintaan ke resolver satu tabel.
#[derive(Debug, Clone)]
pub struct RevealRequest {
    pub company_id: i32,
    /// User yang MEMINTA reveal — ia yang discope.
    pub actor_user_id: i32,
    /// UUID publik baris yang diminta.
    pub record_uuid: String,
    /// Jabatan efektif pemanggil (`access.effectiveRoleCode`); `None` = tanpa `access`.
    pub actor_role_code: Option<String>,
}

/// Satu baris yang berhasil ditemukan resolver.
#[derive(Debug, Clone)]
pub struct RevealTarget {
    /// PK internal — `fieldRevealLog.recordId`.
    pub record_id: i32,
    /// UUID publik — `fieldRevealLog.recordUuid`.
    pub record_uuid: Option<String>,
    /// Nama/kode objek SAAT kejadian (`fieldRevealLog.recordLabel`).
    pub record_label: Option<String>,
    /// Record yang SUDAH DISERIALISASI persis seperti endpoint detail-nya.
    pub record: Value,
}

/// Resolver satu tabel; hidup di crate pemilik tabelnya dan WAJIB memakai jalur pencarian yang
/// sama dengan endpoint detail-nya (lengkap dengan `companyId`). Baris yang tidak boleh dilihat
/// pemanggil dijawab dengan 404 milik tabel itu sendiri (`STORE_NOT_FOUND`), bukan 403.
pub trait RevealResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        state: &'a AppState,
        request: RevealRequest,
    ) -> BoxFuture<'a, Result<RevealTarget, ReqError>>;
}

type Registry = RwLock<HashMap<&'static str, Arc<dyn RevealResolver>>>;

static RESOLVERS: LazyLock<Registry> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// `TableRevealRegistry.register(token, resolver)`: dipanggil crate pemilik tabel saat
/// `router()`-nya dibangun. Token = `TableDefinition.reveal_resolver`.
pub fn register_resolver(token: &'static str, resolver: Arc<dyn RevealResolver>) {
    RESOLVERS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(token, resolver);
}

fn resolver_for(token: &str) -> Option<Arc<dyn RevealResolver>> {
    RESOLVERS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(token)
        .cloned()
}

/// `TableRevealRegistry.missing()`: tabel yang mendeklarasikan token tetapi belum terdaftar.
pub fn missing_resolvers() -> Vec<&'static str> {
    TABLES
        .iter()
        .filter(|t| {
            t.reveal_resolver
                .is_some_and(|tok| resolver_for(tok).is_none())
        })
        .map(|t| t.key)
        .collect()
}

// ───────────────────────── util ─────────────────────────

fn truncate(v: Option<&str>, max: usize) -> Option<String> {
    let s = v?;
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    Some(out)
}

fn no_company() -> ReqError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "NO_ACTIVE_COMPANY",
        "User has no active company assigned",
    )
}

fn not_revealable(field: &str) -> ReqError {
    ReqError::body(
        StatusCode::FORBIDDEN,
        json!({
            "code": "FIELD_NOT_REVEALABLE",
            "message": "This field is not revealable for your role",
            "details": { "field": field },
        }),
    )
}

fn denied_feature(feature: &str) -> ReqError {
    ReqError::body(
        StatusCode::FORBIDDEN,
        json!({
            "code": "PERMISSION_DENIED",
            "feature": feature,
            "action": "READ",
            "message": format!("Role ini tidak punya izin READ untuk fitur {feature}"),
        }),
    )
}

/// `assertOwningFeatureGranted`: gerbang tambahan untuk tabel yang fitur pemiliknya bukan
/// `stores` (handler ini statis `@RequireFeature('stores')`).
fn assert_owning_feature_granted(ctx: &RequestCtx, feature: &str) -> Result<(), ReqError> {
    let access = ctx.access();
    let is_custom = access.is_some_and(|a| !a.is_system);
    let granted = access.is_some_and(|a| a.is_granted(feature, Some(PermissionAction::READ)));
    if !is_custom {
        if LEGACY_OPEN_FEATURES.contains(&feature) {
            return Ok(());
        }
        let Some(access) = access else { return Ok(()) };
        if access.permissions.is_empty() || granted {
            return Ok(());
        }
        return Err(denied_feature(feature));
    }
    if granted {
        Ok(())
    } else {
        Err(denied_feature(feature))
    }
}

// ───────────────────────── POST /reveal ─────────────────────────

#[derive(Serialize)]
pub struct RevealOut<'a> {
    field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a Value>,
}

/// Node: src/table-fields/table-fields.controller.ts:152 (`POST /v1/admin/table-fields/reveal`, 200)
///
/// Urutan pemeriksaan bukan selera: mode dulu (paling murah), lalu baris (scope), baru kuota.
pub async fn reveal(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<RevealBody>,
) -> Result<Json<Data<serde_json::Value>>, ReqError> {
    let table_key = body.table_key();
    let field = body.field();
    let table = table_by_key(&table_key);
    let column = column_of(&table_key, &field);

    // Kolom di luar registry, `locked`, dan tabel tanpa resolver dijawab sama: tidak revealable.
    let (Some(table), Some(column)) = (table, column) else {
        return Err(not_revealable(&field));
    };
    let Some(token) = table.reveal_resolver.filter(|_| !column.locked) else {
        return Err(not_revealable(&field));
    };

    if table.feature_key != "stores" {
        assert_owning_feature_granted(&ctx, table.feature_key)?;
    }

    // §7.2 — resolusi yang sama dengan penyensoran respons; segar dari DB.
    let censored = policy_for_request(&state, &ctx, true)
        .await?
        .for_table(table.key);
    if censored.get(&field) != Some(&FieldVisibilityMode::Revealable) {
        return Err(not_revealable(&field));
    }

    let actor_user_id = ctx.require_user_id()?;
    let company_id = ctx.company_id(&state).await?.ok_or_else(no_company)?;

    let Some(resolver) = resolver_for(token) else {
        // Bug penyambungan, bukan kesalahan pemanggil — sengaja berisik (500).
        tracing::error!(
            table = table.key,
            "tabel tidak punya resolver reveal (§7.3)"
        );
        return Err(ApiError::internal(anyhow::anyhow!(
            "Tabel '{}' tidak punya resolver reveal (§7.3).",
            table.key
        )));
    };

    // 404 milik tabelnya sendiri untuk baris yang tidak boleh dilihat pemanggil.
    let target = resolver
        .resolve(
            &state,
            RevealRequest {
                company_id,
                actor_user_id,
                record_uuid: body.record_id(),
                actor_role_code: ctx.effective_role_code().map(str::to_owned),
            },
        )
        .await?;

    let actor_name: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(actor_user_id)
        .fetch_optional(&state.db)
        .await?;
    let time_zone: Option<String> =
        sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let tz = zone_or_utc(time_zone.as_deref().filter(|s| !s.is_empty()));
    let day = Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string();

    let allowed = consume_reveal_quota(
        &state.db,
        &RevealQuotaKey {
            company_id,
            actor_id: actor_user_id,
            table_key: table_key.clone(),
            column_key: field.clone(),
            day,
        },
        DAILY_REVEAL_QUOTA,
    )
    .await?;
    if !allowed {
        return Err(ReqError::body(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "code": "REVEAL_QUOTA_EXCEEDED",
                "message": format!(
                    "Daily reveal quota exceeded for this column ({DAILY_REVEAL_QUOTA} per day)."
                ),
                "details": { "field": field, "quota": DAILY_REVEAL_QUOTA },
            }),
        ));
    }

    let value = resolve_response_path(&target.record, column.response_path());

    // §7.5 — jejak ditulis SEBELUM nilai keluar; gagal tulis = 500 dan nilai tidak terkirim.
    let write_ctx = audit.write_context();
    let request_id = write_ctx
        .request_id
        .as_deref()
        .and_then(|r| uuid::Uuid::parse_str(r).ok());
    let record_uuid = target
        .record_uuid
        .as_deref()
        .and_then(|r| uuid::Uuid::parse_str(r).ok());
    sqlx::query(
        r#"INSERT INTO "fieldRevealLog"
             ("companyId", "tableKey", "columnKey", "recordId", "recordUuid", "recordLabel",
              "actorId", "actorName", "actorRole", reason, "requestId", "ipAddress", "userAgent")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"#,
    )
    .bind(company_id)
    .bind(&table_key)
    .bind(&field)
    .bind(target.record_id)
    .bind(record_uuid)
    .bind(truncate(target.record_label.as_deref(), LABEL_MAX))
    .bind(actor_user_id)
    .bind(truncate(actor_name.as_deref(), LABEL_MAX))
    .bind(truncate(ctx.effective_role_code(), ROLE_MAX))
    .bind(truncate(body.reason().as_deref(), REASON_MAX))
    .bind(request_id)
    .bind(write_ctx.ip_address)
    .bind(write_ctx.user_agent)
    .execute(&state.db)
    .await?;

    let out = serde_json::to_value(RevealOut {
        field: field.clone(),
        value,
    })
    .map_err(|e| ApiError::internal(anyhow::Error::from(e)))?;
    Ok(Json(Data { data: out }))
}

// ───────────────────────── GET /reveal-logs ─────────────────────────

/// Tengah malam lokal `date` di `tz` → naive UTC (celah DST: geser maju seperti moment).
fn local_midnight_utc(tz: chrono_tz::Tz, date: NaiveDate) -> Option<NaiveDateTime> {
    let mut local = date.and_hms_opt(0, 0, 0)?;
    for _ in 0..48 {
        if let Some(dt) = tz.from_local_datetime(&local).earliest() {
            return Some(dt.naive_utc());
        }
        local += chrono::Duration::minutes(30);
    }
    None
}

fn invalid_date(field: &str, value: &str) -> ReqError {
    let mut details = serde_json::Map::new();
    details.insert(field.to_owned(), Value::from(value));
    ReqError::body(
        StatusCode::BAD_REQUEST,
        json!({
            "code": "INVALID_DATE",
            "message": format!("{field} must be a valid YYYY-MM-DD date"),
            "details": details,
        }),
    )
}

/// `parseDay`: `YYYY-MM-DD` ketat (tanggal kalender nyata) → awal hari di zona company.
fn parse_day(
    value: &str,
    field: &str,
    tz: chrono_tz::Tz,
) -> Result<(NaiveDate, NaiveDateTime), ReqError> {
    let date =
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| invalid_date(field, value))?;
    let start = local_midnight_utc(tz, date).ok_or_else(|| invalid_date(field, value))?;
    Ok((date, start))
}

#[derive(sqlx::FromRow)]
struct LogRow {
    uuid: String,
    #[sqlx(rename = "tableKey")]
    table_key: String,
    #[sqlx(rename = "columnKey")]
    column_key: String,
    #[sqlx(rename = "recordUuid")]
    record_uuid: Option<String>,
    #[sqlx(rename = "recordLabel")]
    record_label: Option<String>,
    #[sqlx(rename = "actorId")]
    actor_id: i32,
    #[sqlx(rename = "actorName")]
    actor_name: Option<String>,
    #[sqlx(rename = "actorRole")]
    actor_role: Option<String>,
    reason: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: NaiveDateTime,
}

#[derive(Serialize)]
struct RecordRef {
    id: Option<String>,
    label: Option<String>,
}

#[derive(Serialize)]
struct ActorRef {
    id: Option<String>,
    name: Option<String>,
    role: Option<String>,
}

#[derive(Serialize)]
struct LogItem {
    id: String,
    #[serde(rename = "tableKey")]
    table_key: String,
    #[serde(rename = "columnKey")]
    column_key: String,
    record: RecordRef,
    actor: ActorRef,
    reason: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: i64,
}

#[derive(Serialize)]
struct Meta {
    total: i64,
    page: i64,
    limit: i64,
    #[serde(rename = "totalPages")]
    total_pages: i64,
}

#[derive(Serialize)]
pub struct LogsOut {
    data: Vec<LogItem>,
    meta: Meta,
}

/// Node: src/table-fields/table-fields.controller.ts:196 (`GET /v1/admin/table-fields/reveal-logs`)
pub async fn list_logs(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<LogsQuery>,
) -> Result<Json<LogsOut>, ReqError> {
    let company_id = ctx.company_id(&state).await?.ok_or_else(no_company)?;

    // Pembatas `createdAt` (zona company) dihitung dulu: galat tanggal tidak boleh didahului query.
    let mut gte: Option<NaiveDateTime> = None;
    let mut lt: Option<NaiveDateTime> = None;
    let (start, end) = (q.text("startDate"), q.text("endDate"));
    if start.is_some() || end.is_some() {
        let time_zone: Option<String> =
            sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
                .bind(company_id)
                .fetch_optional(&state.db)
                .await?;
        let tz = zone_or_utc(time_zone.as_deref().filter(|s| !s.is_empty()));
        if let Some(s) = &start {
            gte = Some(parse_day(s, "startDate", tz)?.1);
        }
        if let Some(e) = &end {
            // Eksklusif di hari BERIKUTNYA supaya batas akhir inklusif memuat seluruh hari itu.
            let (date, _) = parse_day(e, "endDate", tz)?;
            let next = date.checked_add_days(Days::new(1));
            lt = next.and_then(|d| local_midnight_utc(tz, d));
        }
    }

    let actor_filter: Option<i32> = match q.text("actorId") {
        Some(uuid) => {
            // User yang tidak ada = nol baris, bukan filter yang gugur.
            let id: Option<i32> = match uuid::Uuid::parse_str(&uuid) {
                Ok(u) => {
                    sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
                        .bind(u)
                        .fetch_optional(&state.db)
                        .await?
                }
                Err(_) => None,
            };
            Some(id.unwrap_or(-1))
        }
        None => None,
    };
    let table_key = q.text("tableKey");
    let column_key = q.text("columnKey");

    let filter = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>| {
        qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
        if let Some(t) = &table_key {
            qb.push(r#" AND "tableKey" = "#).push_bind(t.clone());
        }
        if let Some(c) = &column_key {
            qb.push(r#" AND "columnKey" = "#).push_bind(c.clone());
        }
        if let Some(a) = actor_filter {
            qb.push(r#" AND "actorId" = "#).push_bind(a);
        }
        if let Some(g) = gte {
            qb.push(r#" AND "createdAt" >= "#).push_bind(g);
        }
        if let Some(l) = lt {
            qb.push(r#" AND "createdAt" < "#).push_bind(l);
        }
    };

    let (page, limit) = (q.page(), q.limit());
    let mut count_qb = sqlx::QueryBuilder::new(r#"SELECT count(*) FROM "fieldRevealLog""#);
    filter(&mut count_qb);
    let total: i64 = count_qb.build_query_scalar().fetch_one(&state.db).await?;

    let mut rows_qb = sqlx::QueryBuilder::new(
        r#"SELECT uuid::text AS uuid, "tableKey", "columnKey", "recordUuid"::text AS "recordUuid",
                  "recordLabel", "actorId", "actorName", "actorRole", reason, "createdAt"
             FROM "fieldRevealLog""#,
    );
    filter(&mut rows_qb);
    rows_qb
        .push(r#" ORDER BY "createdAt" DESC, id DESC LIMIT "#)
        .push_bind(limit);
    rows_qb.push(" OFFSET ").push_bind((page - 1) * limit);
    let rows: Vec<LogRow> = rows_qb.build_query_as().fetch_all(&state.db).await?;

    // UUID pelaku diambil terpisah (jejak tanpa relasi ke `users`, §3.3).
    let mut actor_ids: Vec<i32> = rows.iter().map(|r| r.actor_id).collect();
    actor_ids.sort_unstable();
    actor_ids.dedup();
    let actors: Vec<(i32, String)> = if actor_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT id, uuid::text FROM users WHERE id = ANY($1)")
            .bind(&actor_ids)
            .fetch_all(&state.db)
            .await?
    };
    let uuid_by_id: HashMap<i32, String> = actors.into_iter().collect();

    Ok(Json(LogsOut {
        data: rows
            .into_iter()
            .map(|r| LogItem {
                id: r.uuid,
                table_key: r.table_key,
                column_key: r.column_key,
                record: RecordRef {
                    id: r.record_uuid,
                    label: r.record_label,
                },
                actor: ActorRef {
                    id: uuid_by_id.get(&r.actor_id).cloned(),
                    name: r.actor_name,
                    role: r.actor_role,
                },
                reason: r.reason,
                created_at: r.created_at.and_utc().timestamp_millis(),
            })
            .collect(),
        meta: Meta {
            total,
            page,
            limit,
            total_pages: ((total + limit - 1) / limit).max(1),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_utf16_units() {
        assert_eq!(truncate(Some("abcdef"), 3).as_deref(), Some("abc"));
        assert_eq!(truncate(None, 3), None);
        assert_eq!(truncate(Some("a😀b"), 2).as_deref(), Some("a"));
    }

    #[test]
    fn day_parsing_is_strict() {
        let tz = chrono_tz::Asia::Jakarta;
        assert!(parse_day("2026-02-30", "startDate", tz).is_err());
        let (d, start) = parse_day("2026-08-01", "startDate", tz).unwrap();
        assert_eq!(d.to_string(), "2026-08-01");
        assert_eq!(start.to_string(), "2026-07-31 17:00:00");
    }

    #[test]
    fn raw_errors_keep_node_key_order() {
        let (_, body) = crate::error::parts(&not_revealable("phone"));
        assert_eq!(
            body,
            r#"{"code":"FIELD_NOT_REVEALABLE","message":"This field is not revealable for your role","details":{"field":"phone"}}"#
        );
        let (_, body) = crate::error::parts(&denied_feature("customers"));
        assert!(
            body.starts_with(
                r#"{"code":"PERMISSION_DENIED","feature":"customers","action":"READ","#
            )
        );
    }
}
