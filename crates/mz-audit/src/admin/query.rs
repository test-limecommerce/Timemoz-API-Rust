//! Port `src/audit-logs/audit-logs-query.service.ts`: baca audit log (§6.1, §6.2, §6.3, §6.5).
//! Murni baca. `companyId` selalu dari pemanggil (§7.3), uuid company lain selalu 404.

use std::collections::HashMap;

use chrono::NaiveDateTime;
use mz_core::company::require_company_id;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::dto::{BinQuery, DateArg, ListQuery, Paging};
use super::labels::{build_diff, describe_action};

/// Satu baris `auditLog` apa adanya (kolom yang dibutuhkan baca + restore).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct LogRow {
    pub uuid: String,
    #[sqlx(rename = "entityType")]
    pub entity_type: String,
    #[sqlx(rename = "entityId")]
    pub entity_id: i32,
    #[sqlx(rename = "entityUuid")]
    pub entity_uuid: Option<String>,
    #[sqlx(rename = "entityLabel")]
    pub entity_label: Option<String>,
    #[sqlx(rename = "subEntity")]
    pub sub_entity: Option<String>,
    pub action: String,
    #[sqlx(rename = "actorType")]
    pub actor_type: String,
    #[sqlx(rename = "actorId")]
    pub actor_id: Option<i32>,
    #[sqlx(rename = "actorName")]
    pub actor_name: Option<String>,
    #[sqlx(rename = "actorRole")]
    pub actor_role: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    #[sqlx(rename = "changedFields")]
    pub changed_fields: Vec<String>,
    #[sqlx(rename = "isRestorable")]
    pub is_restorable: bool,
    #[sqlx(rename = "restoredAt")]
    pub restored_at: Option<NaiveDateTime>,
    #[sqlx(rename = "restoredByLogId")]
    pub restored_by_log_id: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
}

/// Daftar kolom `SELECT` yang cocok dengan [`LogRow`] (enum & uuid dibaca sebagai teks).
pub(crate) const LOG_COLUMNS: &str = r#"uuid::text AS uuid, "entityType"::text AS "entityType",
    "entityId", "entityUuid"::text AS "entityUuid", "entityLabel", "subEntity", action::text AS action,
    "actorType"::text AS "actorType", "actorId", "actorName", "actorRole", "before", "after",
    "changedFields", "isRestorable", "restoredAt", "restoredByLogId"::text AS "restoredByLogId",
    "createdAt""#;

fn millis(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// Filter `where` gabungan keempat endpoint.
#[derive(Debug, Default, Clone)]
pub(crate) struct Filter {
    pub company_id: i32,
    /// Kotak sampah: `action = delete AND isRestorable AND restoredAt IS NULL`.
    pub recycle_bin: bool,
    pub entity_types: Option<Vec<String>>,
    pub entity_uuid: Option<String>,
    pub actions: Option<Vec<String>>,
    pub actor_ids: Option<Vec<i32>>,
    pub date_from: Option<NaiveDateTime>,
    pub date_to: Option<NaiveDateTime>,
    pub search: Option<String>,
}

fn non_empty(v: &Option<Vec<String>>) -> Option<&Vec<String>> {
    v.as_ref().filter(|l| !l.is_empty())
}

impl Filter {
    fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        qb.push(r#" WHERE "companyId" = "#)
            .push_bind(self.company_id);
        if self.recycle_bin {
            qb.push(r#" AND "action" = 'delete'::"AuditAction" AND "isRestorable" = true AND "restoredAt" IS NULL"#);
        }
        if let Some(types) = non_empty(&self.entity_types) {
            qb.push(r#" AND "entityType" = ANY("#)
                .push_bind(types.clone())
                .push(r#"::text[]::"AuditEntityType"[])"#);
        }
        if let Some(uuid) = &self.entity_uuid {
            qb.push(r#" AND "entityUuid" = "#)
                .push_bind(uuid.clone())
                .push("::uuid");
        }
        if let Some(actions) = non_empty(&self.actions) {
            qb.push(r#" AND "action" = ANY("#)
                .push_bind(actions.clone())
                .push(r#"::text[]::"AuditAction"[])"#);
        }
        if let Some(ids) = self.actor_ids.as_ref().filter(|l| !l.is_empty()) {
            qb.push(r#" AND "actorId" = ANY("#)
                .push_bind(ids.clone())
                .push(")");
        }
        if let Some(from) = self.date_from {
            qb.push(r#" AND "createdAt" >= "#).push_bind(from);
        }
        if let Some(to) = self.date_to {
            qb.push(r#" AND "createdAt" <= "#).push_bind(to);
        }
        // `...(query.search && { OR: [...] })`: string kosong = tanpa filter. `contains` +
        // `mode: 'insensitive'` Prisma = `ILIKE '%…%'` (tanpa escape, sama dengan Node).
        if let Some(search) = self.search.as_ref().filter(|s| !s.is_empty()) {
            let pattern = format!("%{search}%");
            qb.push(r#" AND ("entityLabel" ILIKE "#)
                .push_bind(pattern.clone())
                .push(r#" OR "actorName" ILIKE "#)
                .push_bind(pattern)
                .push(")");
        }
    }
}

/// Hasil terpaginasi ber-`meta`.
async fn page(db: &PgPool, filter: &Filter, paging: Paging) -> ApiResult<Value> {
    let skip = (paging.page_number - 1)
        .checked_mul(paging.items_per_page)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("skip melewati batas")))?;

    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT {LOG_COLUMNS} FROM \"auditLog\""));
    filter.push(&mut qb);
    qb.push(r#" ORDER BY "createdAt" DESC, id DESC LIMIT "#)
        .push_bind(paging.items_per_page)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<LogRow> = qb.build_query_as().fetch_all(db).await?;

    let mut cq = QueryBuilder::<Postgres>::new(r#"SELECT count(*) FROM "auditLog""#);
    filter.push(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(db).await?;

    let actors = resolve_actor_uuids(db, rows.iter().filter_map(|r| r.actor_id).collect()).await?;
    let data: Vec<Value> = rows.iter().map(|r| to_row(r, &actors)).collect();
    let total_pages = (total + paging.items_per_page - 1) / paging.items_per_page;
    Ok(json!({
        "data": data,
        "meta": {
            "total": total,
            "pageNumber": paging.page_number,
            "itemsPerPage": paging.items_per_page,
            "totalPages": total_pages,
        },
    }))
}

fn empty_page(paging: Paging) -> Value {
    json!({
        "data": [],
        "meta": {
            "total": 0,
            "pageNumber": paging.page_number,
            "itemsPerPage": paging.items_per_page,
            "totalPages": 0,
        },
    })
}

/// `resolveActorUuids`: id → uuid user.
async fn resolve_actor_uuids(db: &PgPool, mut ids: Vec<i32>) -> ApiResult<HashMap<i32, String>> {
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT id, uuid::text FROM users WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// `resolveActorIdsFromUuids`.
async fn resolve_actor_ids(db: &PgPool, uuids: &[String]) -> ApiResult<Vec<i32>> {
    if uuids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        sqlx::query_scalar("SELECT id FROM users WHERE uuid = ANY($1::text[]::uuid[])")
            .bind(uuids.to_vec())
            .fetch_all(db)
            .await?,
    )
}

/// Bentuk baris ringkas §6.1 (dipakai apa adanya oleh §6.5).
pub(crate) fn to_row(row: &LogRow, actors: &HashMap<i32, String>) -> Value {
    json!({
        "id": row.uuid,
        "entityType": row.entity_type,
        "entity": { "id": row.entity_uuid, "label": row.entity_label },
        "subEntity": row.sub_entity,
        "action": row.action,
        "actionLabel": describe_action(&row.action, &row.actor_type),
        "actor": {
            "id": row.actor_id.and_then(|id| actors.get(&id)),
            "name": row.actor_name,
            "role": row.actor_role,
            "type": row.actor_type,
        },
        "changedFields": row.changed_fields,
        "isRestorable": row.is_restorable,
        "restoredAt": row.restored_at.map(millis),
        "createdAt": millis(row.created_at),
    })
}

fn date_bound(arg: Option<&DateArg>) -> ApiResult<Option<NaiveDateTime>> {
    match arg {
        None => Ok(None),
        Some(DateArg::Ok(d)) => Ok(Some(*d)),
        // `new Date(x)` Invalid Date → Prisma menolak (500).
        Some(DateArg::Unreadable) => Err(ApiError::internal(anyhow::anyhow!(
            "Invalid Date pada filter audit log"
        ))),
    }
}

/// §6.1.
pub(crate) async fn find_all(db: &PgPool, user_id: i32, query: &ListQuery) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;

    let actor_ids = match query.actor_id.as_deref() {
        Some(uuids) => resolve_actor_ids(db, uuids).await?,
        None => Vec::new(),
    };
    if query.actor_id.as_ref().is_some_and(|l| !l.is_empty()) && actor_ids.is_empty() {
        // Seluruh uuid yang diminta tidak dikenal — daftar kosong, bukan filter yang diabaikan.
        return Ok(empty_page(query.paging));
    }

    let filter = Filter {
        company_id,
        recycle_bin: false,
        entity_types: query.entity_type.clone(),
        entity_uuid: query.entity_uuid.clone(),
        actions: query.action.clone(),
        actor_ids: Some(actor_ids),
        date_from: date_bound(query.date_from.as_ref())?,
        date_to: date_bound(query.date_to.as_ref())?,
        search: query.search.clone(),
    };
    page(db, &filter, query.paging).await
}

/// §6.3 — kotak sampah.
pub(crate) async fn find_recycle_bin(
    db: &PgPool,
    user_id: i32,
    query: &BinQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;
    let filter = Filter {
        company_id,
        recycle_bin: true,
        entity_types: query.entity_type.clone(),
        search: query.search.clone(),
        ..Filter::default()
    };
    page(db, &filter, query.paging).await
}

/// §6.2.
pub(crate) async fn find_one(db: &PgPool, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;
    // `companyId` di `where` sendiri (§7.3 syarat 2): uuid company lain 404.
    let row: Option<LogRow> = sqlx::query_as(&format!(
        r#"SELECT {LOG_COLUMNS} FROM "auditLog" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Err(not_found());
    };
    let actors = resolve_actor_uuids(db, row.actor_id.into_iter().collect()).await?;
    let mut out = to_row(&row, &actors);
    let diff = build_diff(
        &row.entity_type,
        row.sub_entity.as_deref(),
        row.before.as_ref(),
        row.after.as_ref(),
        &row.changed_fields,
    );
    if let Some(obj) = out.as_object_mut() {
        obj.insert("before".to_owned(), row.before.unwrap_or(Value::Null));
        obj.insert("after".to_owned(), row.after.unwrap_or(Value::Null));
        obj.insert("diff".to_owned(), Value::Array(diff));
    }
    Ok(out)
}

/// §6.5.
pub(crate) async fn find_by_entity(
    db: &PgPool,
    user_id: i32,
    entity_type: &str,
    entity_uuid: &str,
    paging: Paging,
) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;
    let filter = Filter {
        company_id,
        entity_types: Some(vec![entity_type.to_owned()]),
        entity_uuid: Some(entity_uuid.to_owned()),
        ..Filter::default()
    };
    page(db, &filter, paging).await
}

/// `NotFoundException({ code: 'AUDIT_LOG_NOT_FOUND', ... })`.
pub(crate) fn not_found() -> ApiError {
    ApiError::coded(
        axum::http::StatusCode::NOT_FOUND,
        "AUDIT_LOG_NOT_FOUND",
        "Audit log not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Execute;

    fn sql_of(f: &Filter) -> String {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM \"auditLog\"");
        f.push(&mut qb);
        qb.build().sql().to_owned()
    }

    #[test]
    fn filter_sql_shapes() {
        let base = Filter {
            company_id: 1,
            ..Filter::default()
        };
        assert_eq!(
            sql_of(&base),
            r#"SELECT 1 FROM "auditLog" WHERE "companyId" = $1"#
        );

        let bin = Filter {
            company_id: 1,
            recycle_bin: true,
            entity_types: Some(vec!["store".into()]),
            search: Some("jogja".into()),
            ..Filter::default()
        };
        let sql = sql_of(&bin);
        assert!(sql.contains(
            r#""action" = 'delete'::"AuditAction" AND "isRestorable" = true AND "restoredAt" IS NULL"#
        ));
        assert!(sql.contains(r#""entityType" = ANY($2::text[]::"AuditEntityType"[])"#));
        assert!(sql.contains(r#"("entityLabel" ILIKE $3 OR "actorName" ILIKE $4)"#));
    }

    #[test]
    fn empty_arrays_and_blank_search_add_no_clause() {
        let f = Filter {
            company_id: 1,
            entity_types: Some(vec![]),
            actions: Some(vec![]),
            actor_ids: Some(vec![]),
            search: Some(String::new()),
            ..Filter::default()
        };
        assert_eq!(
            sql_of(&f),
            r#"SELECT 1 FROM "auditLog" WHERE "companyId" = $1"#
        );
    }

    #[test]
    fn full_list_filter_binds_in_order() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .unwrap();
        let f = Filter {
            company_id: 1,
            entity_uuid: Some("00000000-0000-4000-8000-00000000dead".into()),
            actions: Some(vec!["update".into()]),
            actor_ids: Some(vec![3]),
            date_from: Some(t),
            date_to: Some(t),
            ..Filter::default()
        };
        let sql = sql_of(&f);
        let order = [
            r#""entityUuid" = $2::uuid"#,
            r#""action" = ANY($3::text[]::"AuditAction"[])"#,
            r#""actorId" = ANY($4)"#,
            r#""createdAt" >= $5"#,
            r#""createdAt" <= $6"#,
        ];
        let mut at = 0;
        for needle in order {
            let pos = sql[at..]
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} di {sql}"));
            at += pos;
        }
    }

    #[test]
    fn row_shape_matches_node() {
        let row = LogRow {
            uuid: "u1".into(),
            entity_type: "store".into(),
            entity_id: 5,
            entity_uuid: Some("e1".into()),
            entity_label: Some("JOG01".into()),
            sub_entity: None,
            action: "delete".into(),
            actor_type: "system".into(),
            actor_id: Some(9),
            actor_name: Some("cron".into()),
            actor_role: None,
            before: None,
            after: None,
            changed_fields: vec!["deletedAt".into()],
            is_restorable: false,
            restored_at: None,
            restored_by_log_id: None,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
                .and_then(|d| d.and_hms_milli_opt(0, 0, 0, 250))
                .unwrap(),
        };
        let mut actors = HashMap::new();
        actors.insert(9, "uuid-9".to_owned());
        let v = to_row(&row, &actors);
        assert_eq!(v["id"], "u1");
        assert_eq!(v["actionLabel"], "Deactivated by system");
        assert_eq!(v["actor"]["id"], "uuid-9");
        assert_eq!(v["restoredAt"], Value::Null);
        assert_eq!(v["createdAt"], 1_767_225_600_250_i64);
        assert_eq!(v["entity"], json!({"id": "e1", "label": "JOG01"}));
        // Actor tak ditemukan → null.
        let v = to_row(&row, &HashMap::new());
        assert_eq!(v["actor"]["id"], Value::Null);
    }

    #[test]
    fn unreadable_date_is_internal_error() {
        assert!(date_bound(Some(&DateArg::Unreadable)).is_err());
        assert!(date_bound(None).unwrap().is_none());
    }
}
