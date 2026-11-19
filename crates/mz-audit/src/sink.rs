//! Port `AuditLogsService` sisi tulis: `flush` (`createMany`), `record` (`create`),
//! `recordWithin` (di dalam transaksi pemanggil) — `buildRows`, `resolveActor`,
//! `resolveCompanyId`.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::build::AuditEntry;
use crate::scope::{ActorType, EntityType};

/// Batas kolom §2.1.
const MAX_ENTITY_LABEL: usize = 160;
const MAX_SUB_ENTITY: usize = 40;
pub(crate) const MAX_ACTOR_NAME: usize = 160;
const MAX_ACTOR_ROLE: usize = 32;
pub(crate) const MAX_ROUTE: usize = 160;
pub(crate) const MAX_IP: usize = 45;
pub(crate) const MAX_USER_AGENT: usize = 255;

/// `truncate(value, max)` — `String.prototype.slice` memotong per *code unit* UTF-16.
/// Pasangan surrogate yang terbelah di batas tidak diikutkan (Node akan menulis
/// separuhnya, yang tidak bisa disimpan Postgres).
pub(crate) fn truncate(value: Option<&str>, max: usize) -> Option<String> {
    let value = value?;
    let mut units = 0;
    let mut end = value.len();
    for (idx, ch) in value.char_indices() {
        units += ch.len_utf16();
        if units > max {
            end = idx;
            break;
        }
    }
    Some(value.get(..end).unwrap_or(value).to_owned())
}

/// `AuditWriteContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteContext {
    pub request_id: Option<String>,
    pub actor_type: ActorType,
    pub actor_id: Option<i32>,
    pub actor_name: Option<String>,
    pub route: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

/// `ResolvedActor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedActor {
    pub name: Option<String>,
    pub role: Option<String>,
    pub company_id: Option<i32>,
}

/// Satu baris `auditLog` siap tulis.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertRow {
    pub company_id: i32,
    pub entity_type: EntityType,
    pub entity_id: i32,
    pub entity_uuid: Option<String>,
    pub entity_label: Option<String>,
    pub sub_entity: Option<String>,
    pub action: &'static str,
    pub actor_type: ActorType,
    pub actor_id: Option<i32>,
    pub actor_name: Option<String>,
    pub actor_role: Option<String>,
    pub request_id: Option<String>,
    pub route: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub changed_fields: Vec<String>,
    pub is_restorable: bool,
}

/// Bagian murni `buildRows` untuk satu entri yang `companyId`-nya sudah diketahui.
pub(crate) fn assemble(
    entry: &AuditEntry,
    ctx: &WriteContext,
    actor: &ResolvedActor,
    company_id: i32,
) -> InsertRow {
    InsertRow {
        company_id,
        entity_type: entry.entity_type,
        entity_id: entry.entity_id,
        entity_uuid: entry.entity_uuid.clone(),
        entity_label: truncate(entry.entity_label.as_deref(), MAX_ENTITY_LABEL),
        sub_entity: truncate(entry.sub_entity.as_deref(), MAX_SUB_ENTITY),
        action: entry.action.as_str(),
        actor_type: ctx.actor_type,
        actor_id: ctx.actor_id,
        actor_name: truncate(actor.name.as_deref(), MAX_ACTOR_NAME),
        actor_role: truncate(actor.role.as_deref(), MAX_ACTOR_ROLE),
        request_id: ctx.request_id.clone(),
        route: ctx.route.clone(),
        ip_address: ctx.ip_address.clone(),
        user_agent: ctx.user_agent.clone(),
        before: entry.before.clone().filter(|v| !v.is_null()),
        after: entry.after.clone().filter(|v| !v.is_null()),
        changed_fields: entry.changed_fields.clone(),
        // Syarat 2 §5.1 — hanya perubahan oleh `user` yang bisa dipulihkan.
        is_restorable: entry.is_restorable && ctx.actor_type == ActorType::User,
    }
}

/// Cara menangani galat pembacaan aktor/company saat menyusun baris audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// `buildRows` Node: galat ditelan (`warn`), baris tetap ditulis dengan data seadanya.
    Lenient,
    /// Di dalam transaksi pemanggil: galat DATABASE sudah membatalkan transaksinya, jadi
    /// menelannya hanya memindahkan galat ke statement berikutnya (25P02 "current transaction
    /// is aborted") dan menyembunyikan penyebab aslinya (mis. 40001 yang mestinya di-retry).
    /// Galat database diteruskan; galat lain (decode) tetap ditelan.
    InTransaction,
}

impl Lookup {
    fn swallow(self, err: sqlx::Error) -> sqlx::Result<()> {
        match (self, err) {
            (Self::InTransaction, err @ sqlx::Error::Database(_)) => Err(err),
            _ => Ok(()),
        }
    }
}

/// `resolveActor` — dengan [`Lookup::Lenient`] kegagalan tidak pernah dilempar.
pub(crate) async fn resolve_actor(
    conn: &mut PgConnection,
    ctx: &WriteContext,
    lookup: Lookup,
) -> sqlx::Result<ResolvedActor> {
    let fallback = ResolvedActor {
        name: ctx.actor_name.clone(),
        role: None,
        company_id: None,
    };
    let (ActorType::User, Some(actor_id)) = (ctx.actor_type, ctx.actor_id) else {
        return Ok(fallback);
    };
    let result: sqlx::Result<Option<(String, Option<String>)>> = sqlx::query_as(
        r#"SELECT u.name,
                  (SELECT al.code FROM "userAccessLevel" ual
                     LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                    WHERE ual."userId" = u.id AND ual.status = 1
                    ORDER BY ual.id DESC LIMIT 1) AS role
             FROM users u WHERE u.id = $1"#,
    )
    .bind(actor_id)
    .fetch_optional(&mut *conn)
    .await;
    let user = match result {
        Ok(u) => u,
        Err(err) => {
            tracing::warn!("Gagal membaca identitas pelaku #{actor_id}: {err}");
            lookup.swallow(err)?;
            return Ok(fallback);
        }
    };
    let company_id = match &user {
        Some(_) => match mz_core::company::find_company_id(&mut *conn, actor_id).await {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("Gagal membaca identitas pelaku #{actor_id}: {err}");
                lookup.swallow(err)?;
                return Ok(fallback);
            }
        },
        None => None,
    };
    let (name, role) = user.map_or((None, None), |(n, r)| (Some(n), r));
    Ok(ResolvedActor {
        name: ctx.actor_name.clone().or(name),
        role,
        company_id,
    })
}

/// `resolveCompanyId` — urutan §4.1: company pelaku, keanggotaan tunggal, atau dibuang.
async fn resolve_company_id(
    conn: &mut PgConnection,
    entry: &AuditEntry,
    actor: &ResolvedActor,
    cache: &mut HashMap<i32, Option<i32>>,
    lookup: Lookup,
) -> sqlx::Result<Option<i32>> {
    if actor.company_id.is_some() {
        return Ok(actor.company_id);
    }
    if entry.entity_type != EntityType::Employee {
        return Ok(None);
    }
    if let Some(cached) = cache.get(&entry.entity_id) {
        return Ok(*cached);
    }
    let resolved = match sqlx::query_scalar::<_, i32>(
        r#"SELECT "companyId" FROM "userCompany" WHERE "userId" = $1 LIMIT 2"#,
    )
    .bind(entry.entity_id)
    .fetch_all(&mut *conn)
    .await
    {
        Ok(rows) if rows.len() == 1 => rows.first().copied(),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(
                "Gagal membaca keanggotaan karyawan #{}: {err}",
                entry.entity_id
            );
            lookup.swallow(err)?;
            None
        }
    };
    cache.insert(entry.entity_id, resolved);
    Ok(resolved)
}

/// `buildRows`. Dengan [`Lookup::Lenient`] tidak pernah `Err`.
pub(crate) async fn build_rows(
    conn: &mut PgConnection,
    entries: &[AuditEntry],
    ctx: &WriteContext,
    lookup: Lookup,
) -> sqlx::Result<Vec<InsertRow>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let actor = resolve_actor(conn, ctx, lookup).await?;
    let mut cache = HashMap::new();
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let company_id = match entry.company_id {
            Some(c) => Some(c),
            None => resolve_company_id(conn, entry, &actor, &mut cache, lookup).await?,
        };
        let Some(company_id) = company_id else {
            tracing::warn!(
                "Baris audit dibuang: companyId tidak bisa ditentukan untuk {}#{} ({})",
                entry.entity_type,
                entry.entity_id,
                entry.action
            );
            continue;
        };
        rows.push(assemble(entry, ctx, &actor, company_id));
    }
    Ok(rows)
}

/// Satu `INSERT` multi-baris; mengembalikan `uuid` baris yang lahir.
pub(crate) async fn insert_rows(
    conn: &mut PgConnection,
    rows: &[InsertRow],
) -> sqlx::Result<Vec<String>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO "auditLog" ("companyId", "entityType", "entityId", "entityUuid", "entityLabel", "subEntity",
             "action", "actorType", "actorId", "actorName", "actorRole", "requestId", "route", "ipAddress",
             "userAgent", "before", "after", "changedFields", "isRestorable") "#,
    );
    qb.push_values(rows, |mut b, r| {
        b.push_bind(r.company_id)
            .push_bind(r.entity_type.as_str())
            .push_unseparated(r#"::"AuditEntityType""#)
            .push_bind(r.entity_id)
            .push_bind(r.entity_uuid.clone())
            .push_unseparated("::uuid")
            .push_bind(r.entity_label.clone())
            .push_bind(r.sub_entity.clone())
            .push_bind(r.action)
            .push_unseparated(r#"::"AuditAction""#)
            .push_bind(r.actor_type.as_str())
            .push_unseparated(r#"::"AuditActorType""#)
            .push_bind(r.actor_id)
            .push_bind(r.actor_name.clone())
            .push_bind(r.actor_role.clone())
            .push_bind(r.request_id.clone())
            .push_unseparated("::uuid")
            .push_bind(r.route.clone())
            .push_bind(r.ip_address.clone())
            .push_bind(r.user_agent.clone())
            .push_bind(r.before.clone().map(Json))
            .push_bind(r.after.clone().map(Json))
            .push_bind(r.changed_fields.clone())
            .push_bind(r.is_restorable);
    });
    qb.push(" RETURNING uuid::text");
    qb.build_query_scalar::<String>()
        .fetch_all(&mut *conn)
        .await
}

/// `flush`/`record`: bangun baris lalu tulis. Galat dikembalikan ke pemanggil (yang
/// me-`warn`-nya).
pub(crate) async fn write(
    pool: &sqlx::PgPool,
    entries: &[AuditEntry],
    ctx: &WriteContext,
) -> sqlx::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut conn = pool.acquire().await?;
    let rows = build_rows(&mut conn, entries, ctx, Lookup::Lenient).await?;
    insert_rows(&mut conn, &rows).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Action;
    use serde_json::json;

    /// Galat database tiruan (SQLSTATE 40001).
    #[derive(Debug)]
    struct Conflict;
    impl std::fmt::Display for Conflict {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("could not serialize access")
        }
    }
    impl std::error::Error for Conflict {}
    impl sqlx::error::DatabaseError for Conflict {
        fn message(&self) -> &str {
            "could not serialize access"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some("40001".into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn lookup_swallow_rules() {
        let db = || sqlx::Error::Database(Box::new(Conflict));
        let decode = || sqlx::Error::Decode("kolom aneh".into());
        assert!(Lookup::Lenient.swallow(db()).is_ok());
        assert!(Lookup::Lenient.swallow(decode()).is_ok());
        // Di dalam transaksi: galat database (yang membatalkan transaksi) diteruskan utuh.
        assert!(matches!(
            Lookup::InTransaction.swallow(db()),
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("40001")
        ));
        assert!(Lookup::InTransaction.swallow(decode()).is_ok());
    }

    fn entry() -> AuditEntry {
        AuditEntry {
            entity_type: EntityType::Store,
            entity_id: 9,
            entity_uuid: Some("u".into()),
            entity_label: Some("x".repeat(200)),
            sub_entity: None,
            action: Action::Delete,
            company_id: Some(7),
            before: Some(json!({"deletedAt": null})),
            after: None,
            changed_fields: vec![],
            is_restorable: true,
        }
    }

    fn ctx(actor_type: ActorType) -> WriteContext {
        WriteContext {
            request_id: Some("r".into()),
            actor_type,
            actor_id: (actor_type == ActorType::User).then_some(5),
            actor_name: None,
            route: Some("DELETE /stores/u".into()),
            ip_address: None,
            user_agent: None,
        }
    }

    #[test]
    fn truncates_by_utf16_units() {
        assert_eq!(truncate(Some("abcdef"), 3).as_deref(), Some("abc"));
        assert_eq!(truncate(None, 3), None);
        assert_eq!(truncate(Some("ab😀c"), 3).as_deref(), Some("ab"));
        assert_eq!(truncate(Some("ab😀c"), 4).as_deref(), Some("ab😀"));
    }

    #[test]
    fn restorable_only_for_user_actor_and_labels_truncated() {
        let actor = ResolvedActor {
            name: Some("n".repeat(200)),
            role: Some("r".repeat(40)),
            company_id: None,
        };
        let row = assemble(&entry(), &ctx(ActorType::User), &actor, 7);
        assert!(row.is_restorable);
        assert_eq!(row.entity_label.map(|l| l.len()), Some(160));
        assert_eq!(row.actor_name.map(|l| l.len()), Some(160));
        assert_eq!(row.actor_role.map(|l| l.len()), Some(32));
        assert_eq!(row.after, None);
        let row = assemble(&entry(), &ctx(ActorType::System), &actor, 7);
        assert!(!row.is_restorable);
    }
}
