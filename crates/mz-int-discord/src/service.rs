//! Port `src/integrations/discord/discord.service.ts`.
//!
//! Setiap method Node membungkus seluruh isinya dengan `try/catch` dan mengembalikan
//! `{ success: false, message: 'An unexpected error occurred', data: null }` (HTTP 200)
//! untuk lemparan apa pun (Prisma, axios, `TypeError`). Di sini isi method mengembalikan
//! `Result<Envelope<T>, Fail>` dan [`caught`] memetakan `Err` ke balasan itu.

use std::collections::HashMap;

use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::AppState;
use mz_core::enums::Role;
use mz_core::time::{JsDate, now_naive};
use mz_int_common::envelope::Envelope;
use mz_int_common::jsonb;
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::api::{self, DiscordError};
use crate::dto::{LinkChannelDto, LinkUserDto, SetDefaultTaskChannelDto, TaskItem};
use crate::util::{is_snowflake, iso_now, js_len, js_string, nn, opt_get, truthy};

/// `ADMIN_ROLES` Node (bawaan `resolveCompanyId`/`getCompanyCredentials`).
const ADMIN_ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
/// Admin + MANAGER (`postTasksMessage`).
const ADMINS_MANAGER: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

pub const UNEXPECTED: &str = "An unexpected error occurred";
/// Warna hijau bawaan embed (`5763719` = `#57F287`).
pub const GREEN: i64 = 5_763_719;

/// Lemparan di dalam `try` Node.
#[derive(Debug)]
pub enum Fail {
    Discord(DiscordError),
    Db(sqlx::Error),
    /// `TypeError` runtime / galat validasi Prisma.
    Js(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discord(e) => write!(f, "discord: {e}"),
            Self::Db(e) => write!(f, "db: {e}"),
            Self::Js(e) => write!(f, "{e}"),
        }
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl From<DiscordError> for Fail {
    fn from(e: DiscordError) -> Self {
        Self::Discord(e)
    }
}

type Out<T> = Result<Envelope<T>, Fail>;

/// `catch (error) { Sentry.captureException(...); return { success: false, ... } }`.
fn caught<T>(module: &'static str, r: Out<T>) -> Envelope<T> {
    r.unwrap_or_else(|err| {
        tracing::error!(module, error = %err, "discord: lemparan tertangkap");
        Envelope::fail(UNEXPECTED)
    })
}

/// `{ success: true, message: 'Success', data }`.
fn success<T>(data: T) -> Envelope<T> {
    Envelope::ok("Success", data)
}

// ───────────────────────── kredensial ─────────────────────────

pub struct Creds {
    pub company_id: i32,
    pub guild_id: Option<String>,
    pub guild_name: Option<String>,
}

/// `resolveCompanyId(userId, roles = ADMIN_ROLES)`: `userAccessLevel` aktif dengan
/// `roleCodeFilter(roles)` (role sistem ber-code itu ATAU role custom ber-`baseRole` yang
/// setara; hanya ADMIN/MANAGER/STAFF punya `baseRole`). Nol → `None`, satu → company itu,
/// lebih → `findCompanyIdAmong` atas id terurut, cadangan id terkecil.
async fn resolve_company_id(
    state: &AppState,
    user_id: i32,
    roles: &[Role],
) -> sqlx::Result<Option<i32>> {
    let codes: Vec<String> = roles.iter().map(|r| r.as_str().to_owned()).collect();
    let bases: Vec<String> = roles
        .iter()
        .filter(|r| matches!(r, Role::ADMIN | Role::MANAGER | Role::STAFF))
        .map(|r| r.as_str().to_owned())
        .collect();
    let mut ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT ual."companyId" FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual.status = 1
             AND ((al."isSystem" AND al.code = ANY($2))
                  OR (NOT al."isSystem" AND al."baseRole"::text = ANY($3)))"#,
    )
    .bind(user_id)
    .bind(&codes)
    .bind(&bases)
    .fetch_all(&state.db)
    .await?;
    match ids.as_slice() {
        [] => return Ok(None),
        [one] => return Ok(Some(*one)),
        _ => {}
    }
    ids.sort_unstable();
    let among = mz_core::company::find_company_id_among(&state.db, user_id, &ids).await?;
    Ok(among.or_else(|| ids.first().copied()))
}

/// `getCompanyCredentials(userId, roles = ADMIN_ROLES)`.
async fn credentials(state: &AppState, user_id: i32) -> Result<Option<Creds>, Fail> {
    credentials_for(state, user_id, ADMIN_ROLES).await
}

async fn credentials_for(
    state: &AppState,
    user_id: i32,
    roles: &[Role],
) -> Result<Option<Creds>, Fail> {
    let Some(company_id) = resolve_company_id(state, user_id, roles).await? else {
        return Ok(None);
    };
    let row: Option<(i32, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT id, "discordGuildId", "discordGuildName" FROM company WHERE id = $1 LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(id, guild_id, guild_name)| Creds {
        company_id: id,
        guild_id,
        guild_name,
    }))
}

/// `creds?.discordGuildId` truthy → `(creds, guildId)`.
async fn connected(state: &AppState, user_id: i32) -> Result<Option<(Creds, String)>, Fail> {
    connected_for(state, user_id, ADMIN_ROLES).await
}

async fn connected_for(
    state: &AppState,
    user_id: i32,
    roles: &[Role],
) -> Result<Option<(Creds, String)>, Fail> {
    Ok(credentials_for(state, user_id, roles).await?.and_then(|c| {
        let gid = c.guild_id.clone().filter(|g| !g.is_empty())?;
        Some((c, gid))
    }))
}

// ───────────────────────── helper data Discord ─────────────────────────

/// `(res.data ?? [])` yang lalu dipanggil `.filter`/`.find`: selain array → `TypeError`.
fn as_list(data: &Value) -> Result<&Vec<Value>, Fail> {
    match data {
        Value::Array(a) => Ok(a),
        Value::Null => Err(Fail::Js("Cannot read properties of null".into())),
        _ => Err(Fail::Js("res.data.filter is not a function".into())),
    }
}

/// `el.key` dengan `el` elemen array (`null` → `TypeError`, primitif → `undefined`).
fn prop<'a>(el: &'a Value, key: &str) -> Result<Option<&'a Value>, Fail> {
    match el {
        Value::Null => Err(Fail::Js(format!(
            "Cannot read properties of null (reading '{key}')"
        ))),
        Value::Object(m) => Ok(m.get(key)),
        _ => Ok(None),
    }
}

/// `NOTIFICATION_CHANNEL_TYPES.has(ch.type)` (Set SameValueZero: angka 0 atau 5).
fn is_notification_type(t: Option<&Value>) -> bool {
    t.and_then(Value::as_f64)
        .is_some_and(|n| n == 0.0 || n == 5.0)
}

/// `(data ?? []).find((ch) => ch.id === id)`.
fn find_channel<'a>(data: &'a Value, id: &str) -> Result<Option<&'a Value>, Fail> {
    for ch in as_list(data)? {
        if prop(ch, "id")?.and_then(Value::as_str) == Some(id) {
            return Ok(Some(ch));
        }
    }
    Ok(None)
}

/// `GET /guilds/{id}/channels` lalu cari channel teks/pengumuman dengan id itu.
async fn guild_text_channel(
    state: &AppState,
    guild_id: &str,
    channel_id: &str,
) -> Result<Option<Value>, Fail> {
    let data = api::get(state, &format!("/guilds/{guild_id}/channels"), &[]).await?;
    let ch = find_channel(&data, channel_id)?;
    Ok(ch.filter(|c| is_notification_type(c.get("type"))).cloned())
}

// ───────────────────────── status / OAuth / disconnect ─────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordStatus {
    pub connected: bool,
    pub guild_id: Option<String>,
    pub guild_name: Option<String>,
}

/// Node: `discord.service.ts:218` (`getConnectionStatus`). Tidak pernah melempar.
pub async fn connection_status(state: &AppState, user_id: i32) -> Envelope<DiscordStatus> {
    let r: Out<DiscordStatus> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        Ok(success(DiscordStatus {
            connected: c.guild_id.as_deref().is_some_and(|g| !g.is_empty()),
            guild_id: c.guild_id,
            guild_name: c.guild_name,
        }))
    }
    .await;
    caught("discord-status", r)
}

/// `company.update({ where: { id }, data })` tanpa `select` + audit Prisma middleware.
/// `sets` = potongan `SET` (tanpa `updatedAt`); nilai lewat `binds`.
async fn update_company(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    sets: &[(&str, Option<&str>)],
) -> Result<(), sqlx::Error> {
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE company SET ");
    for (col, val) in sets {
        qb.push(format!(r#""{col}" = "#))
            .push_bind(val.map(str::to_owned));
        qb.push(", ");
    }
    qb.push(r#""updatedAt" = "#).push_bind(now_naive());
    qb.push(" WHERE id = ").push_bind(company_id);
    qb.build().execute(&state.db).await?;
    if let Some(row) = audit
        .result_row(&state.db, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(())
}

#[derive(Debug)]
pub struct CallbackResult {
    pub success: bool,
    pub message: &'static str,
}

const GUILD_TAKEN: &str = "This Discord server is already linked to another account";

/// Node: `discord.service.ts:81` (`handleOAuthCallback`). `user_id` = `payload.userId`
/// apa adanya (bukan integer → Prisma melempar → pesan tak terduga).
pub async fn handle_oauth_callback(
    state: &AppState,
    audit: &AuditHandle,
    user_id: Option<&Value>,
    guild_id: &str,
) -> CallbackResult {
    let r: Result<CallbackResult, Fail> = async {
        let uid = user_id
            .and_then(Value::as_f64)
            .filter(|f| f.fract() == 0.0 && *f >= f64::from(i32::MIN) && *f <= f64::from(i32::MAX))
            .ok_or_else(|| Fail::Js("Invalid value for argument `userId`".into()))?;
        #[allow(clippy::cast_possible_truncation)]
        let uid = uid as i32;
        let fail = |message| CallbackResult {
            success: false,
            message,
        };
        let Some(company_id) = resolve_company_id(state, uid, ADMIN_ROLES).await? else {
            return Ok(fail("Company not found"));
        };
        if !is_snowflake(guild_id) {
            return Ok(fail("Invalid Discord server id"));
        }
        let taken: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM company WHERE "discordGuildId" = $1 AND id <> $2 LIMIT 1"#,
        )
        .bind(guild_id)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        if taken.is_some() {
            return Ok(fail(GUILD_TAKEN));
        }
        let data = api::get(state, &format!("/guilds/{guild_id}"), &[]).await?;
        // `res.data.name ?? guildId` (data `null` → TypeError; nama non-string ditolak Prisma).
        if data.is_null() {
            return Err(Fail::Js(
                "Cannot read properties of null (reading 'name')".into(),
            ));
        }
        let guild_name = match nn(opt_get(Some(&data), "name")) {
            None => guild_id.to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(Fail::Js(
                    "Invalid value for argument `discordGuildName`".into(),
                ));
            }
        };
        match update_company(
            state,
            audit,
            company_id,
            &[
                ("discordGuildId", Some(guild_id)),
                ("discordGuildName", Some(&guild_name)),
            ],
        )
        .await
        {
            Ok(()) => {}
            Err(e) if mz_core::db::is_unique_violation(&e) => return Ok(fail(GUILD_TAKEN)),
            Err(e) => return Err(e.into()),
        }
        Ok(CallbackResult {
            success: true,
            message: "Discord connected successfully",
        })
    }
    .await;
    r.unwrap_or_else(|err| {
        tracing::error!(module = "discord-oauth-callback", error = %err, "discord: lemparan tertangkap");
        CallbackResult {
            success: false,
            message: UNEXPECTED,
        }
    })
}

/// Node: `discord.service.ts:162` (`disconnect`).
pub async fn disconnect(state: &AppState, audit: &AuditHandle, user_id: i32) -> Envelope<()> {
    let r: Out<()> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        if let Some(gid) = c.guild_id.as_deref().filter(|g| !g.is_empty())
            && let Err(e) = api::delete(state, &format!("/users/@me/guilds/{gid}")).await
        {
            tracing::warn!(
                "Bot leave guild failed for company {}: {}",
                c.company_id,
                e.message
            );
        }
        // `$transaction([company.update, projectDiscordMapping.updateMany])`.
        let pre = audit.pre_read(Model::Company, Lookup::Id(c.company_id)).await;
        let now = now_naive();
        let mut tx = state.db.begin().await?;
        sqlx::query(
            r#"UPDATE company SET "discordGuildId" = NULL, "discordGuildName" = NULL, "updatedAt" = $2
               WHERE id = $1"#,
        )
        .bind(c.company_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let after = audit
            .result_row(&mut *tx, Model::Company, Lookup::Id(c.company_id))
            .await;
        sqlx::query(
            r#"UPDATE "projectDiscordMapping" SET status = 0, "updatedAt" = $2
               WHERE "companyId" = $1 AND status = 1"#,
        )
        .bind(c.company_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if let Some(row) = after {
            audit.record_update(Model::Company, pre, &row).await;
        }
        Ok(Envelope {
            success: true,
            message: "Discord disconnected successfully".into(),
            data: None,
        })
    }
    .await;
    caught("discord-disconnect", r)
}

// ───────────────────────── channels ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRef {
    pub uuid: String,
    pub name: String,
}

/// Item `getChannels`. `id`/`name` disalin apa adanya (tidak ada = kunci hilang,
/// seperti `JSON.stringify` untuk `undefined`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Value>,
    #[serde(rename = "type")]
    pub kind: Value,
    pub position: Value,
    pub parent_id: Value,
    pub linked_project: Option<ProjectRef>,
}

/// Node: `discord.service.ts:243` (`getChannels`).
pub async fn get_channels(state: &AppState, user_id: i32) -> Envelope<Vec<Channel>> {
    let r: Out<Vec<Channel>> = async {
        let Some((c, gid)) = connected(state, user_id).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        let data = api::get(state, &format!("/guilds/{gid}/channels"), &[]).await?;
        let mut text = Vec::new();
        for ch in as_list(&data)? {
            if is_notification_type(prop(ch, "type")?) {
                text.push(ch);
            }
        }
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT m."discordChannelId", p.uuid::text, p.name
               FROM "projectDiscordMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        let linked: HashMap<String, ProjectRef> = rows
            .into_iter()
            .map(|(cid, uuid, name)| (cid, ProjectRef { uuid, name }))
            .collect();
        let mut channels: Vec<(f64, Channel)> = text
            .into_iter()
            .map(|ch| {
                let get = |k: &str| ch.get(k);
                let pos = nn(get("position")).cloned().unwrap_or(Value::Null);
                let key = pos.as_f64().unwrap_or(0.0);
                let linked_project = get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| linked.get(id).cloned());
                (
                    key,
                    Channel {
                        id: get("id").cloned(),
                        name: get("name").cloned(),
                        kind: get("type").cloned().unwrap_or(Value::Null),
                        position: pos,
                        parent_id: nn(get("parent_id")).cloned().unwrap_or(Value::Null),
                        linked_project,
                    },
                )
            })
            .collect();
        // `sort((a, b) => (a.position ?? 0) - (b.position ?? 0))` — stabil.
        channels.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(success(channels.into_iter().map(|(_, c)| c).collect()))
    }
    .await;
    caught("discord-get-channels", r)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkChannelData {
    pub uuid: String,
    pub project_uuid: String,
    pub project_name: String,
    pub discord_channel_id: String,
    pub discord_channel_name: String,
    pub notification_types: Option<Box<RawValue>>,
}

/// `project.findFirst({ where: { uuid, projectCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_project(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> Result<Option<(i32, String, String)>, sqlx::Error> {
    sqlx::query_as(
        r#"SELECT p.id, p.uuid::text, p.name FROM project p
           WHERE p.uuid = $1::uuid AND p.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

/// Node: `discord.service.ts:306` (`linkChannel`).
pub async fn link_channel(
    state: &AppState,
    user_id: i32,
    dto: &LinkChannelDto,
) -> Envelope<LinkChannelData> {
    let r: Out<LinkChannelData> = async {
        let Some((c, gid)) = connected(state, user_id).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        let Some((project_id, project_uuid, project_name)) =
            company_project(state, c.company_id, dto.project_uuid()).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let channel_id = dto.discord_channel_id();
        let Some(ch) = guild_text_channel(state, &gid, channel_id).await? else {
            return Ok(Envelope::fail(
                "Channel not found in your connected Discord server",
            ));
        };
        let conflicting: Option<String> = sqlx::query_scalar(
            r#"SELECT p.name FROM "projectDiscordMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."discordChannelId" = $1 AND m."companyId" = $2 AND m.status = 1
                 AND m."projectId" <> $3
               LIMIT 1"#,
        )
        .bind(channel_id)
        .bind(c.company_id)
        .bind(project_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(name) = conflicting {
            return Ok(Envelope::fail(format!(
                "This Discord channel is already linked to project '{name}'"
            )));
        }
        // `guildChannel.name ?? dto.discordChannelName` (bukan string → Prisma menolak).
        let channel_name = match nn(ch.get("name")) {
            None => dto.discord_channel_name().to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(Fail::Js(
                    "Invalid value for argument `discordChannelName`".into(),
                ));
            }
        };
        // `notificationTypes: dto.notificationTypes ?? null`: Prisma 5 menulis `null` ke
        // kolom `Json?` sebagai JSON null (`'null'::jsonb`), BUKAN NULL SQL (diverifikasi
        // dengan klien Prisma Mudah-API).
        let types = dto
            .notification_types()
            .map_or_else(|| "null".to_owned(), Value::to_string);
        let row: (String, String, String, Option<String>) = sqlx::query_as(
            r#"INSERT INTO "projectDiscordMapping"
                 ("projectId", "companyId", "discordChannelId", "discordChannelName", "notificationTypes", status)
               VALUES ($1, $2, $3, $4, $5::jsonb, 1)
               ON CONFLICT ("projectId", "companyId") DO UPDATE SET
                 "discordChannelId" = EXCLUDED."discordChannelId",
                 "discordChannelName" = EXCLUDED."discordChannelName",
                 "notificationTypes" = EXCLUDED."notificationTypes",
                 status = 1,
                 "updatedAt" = $6
               RETURNING uuid::text, "discordChannelId", "discordChannelName", "notificationTypes"::text"#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .bind(channel_id)
        .bind(&channel_name)
        .bind(types)
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "Project linked to Discord channel successfully",
            LinkChannelData {
                uuid: row.0,
                project_uuid,
                project_name,
                discord_channel_id: row.1,
                discord_channel_name: row.2,
                notification_types: jsonb::raw_opt(row.3.as_deref()),
            },
        ))
    }
    .await;
    caught("discord-link-channel", r)
}

/// Node: `discord.service.ts:416` (`unlinkChannel`).
pub async fn unlink_channel(state: &AppState, user_id: i32, project_uuid: &str) -> Envelope<()> {
    let r: Out<()> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((project_id, _, _)) = company_project(state, c.company_id, project_uuid).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let mapping: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectDiscordMapping"
               WHERE "projectId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some(id) = mapping else {
            return Ok(Envelope::fail(
                "No active Discord channel link found for this project",
            ));
        };
        sqlx::query(
            r#"UPDATE "projectDiscordMapping" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
        Ok(Envelope {
            success: true,
            message: "Project unlinked from Discord channel successfully".into(),
            data: None,
        })
    }
    .await;
    caught("discord-unlink-channel", r)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedChannel {
    pub uuid: String,
    pub project_uuid: String,
    pub project_name: String,
    pub discord_channel_id: String,
    pub discord_channel_name: String,
    pub notification_types: Option<Box<RawValue>>,
    pub linked_at: JsDate,
}

type LinkedChannelRow = (
    String,
    String,
    String,
    Option<String>,
    JsDate,
    String,
    String,
);

/// Node: `discord.service.ts:469` (`getLinkedChannels`).
pub async fn get_linked_channels(state: &AppState, user_id: i32) -> Envelope<Vec<LinkedChannel>> {
    let r: Out<Vec<LinkedChannel>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let rows: Vec<LinkedChannelRow> = sqlx::query_as(
            r#"SELECT m.uuid::text, m."discordChannelId", m."discordChannelName",
                      m."notificationTypes"::text, m."createdAt", p.uuid::text, p.name
               FROM "projectDiscordMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m."createdAt" DESC"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        Ok(success(
            rows.into_iter()
                .map(
                    |(uuid, cid, cname, types, created, puuid, pname)| LinkedChannel {
                        uuid,
                        project_uuid: puuid,
                        project_name: pname,
                        discord_channel_id: cid,
                        discord_channel_name: cname,
                        notification_types: jsonb::raw_opt(types.as_deref()),
                        linked_at: created,
                    },
                )
                .collect(),
        ))
    }
    .await;
    caught("discord-linked-channels", r)
}

// ───────────────────────── users / members ─────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedUser {
    pub uuid: String,
    pub user_uuid: String,
    pub user_name: String,
    pub user_username: String,
    /// `users` tidak punya kolom `email`; `username` adalah email login.
    pub user_email: String,
    pub discord_user_id: String,
    pub discord_username: Option<String>,
    pub discord_display_name: Option<String>,
    pub discord_avatar_url: Option<String>,
    pub linked_at: JsDate,
}

type LinkedUserRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    JsDate,
    String,
    String,
    String,
);

/// Node: `discord.service.ts:513` (`getLinkedUsers`). `userEmail` diisi dari `users.username`.
pub async fn get_linked_users(state: &AppState, user_id: i32) -> Envelope<Vec<LinkedUser>> {
    let r: Out<Vec<LinkedUser>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let rows: Vec<LinkedUserRow> = sqlx::query_as(
            r#"SELECT m.uuid::text, m."discordUserId", m."discordUsername", m."discordDisplayName",
                      m."discordAvatarUrl", m."createdAt", u.uuid::text, u.name, u.username
               FROM "userDiscordMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m."createdAt" DESC"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        Ok(success(
            rows.into_iter()
                .map(
                    |(uuid, did, dname, ddisplay, avatar, created, uuuid, uname, uusername)| {
                        LinkedUser {
                            uuid,
                            user_uuid: uuuid,
                            user_name: uname,
                            user_email: uusername.clone(),
                            user_username: uusername,
                            discord_user_id: did,
                            discord_username: dname,
                            discord_display_name: ddisplay,
                            discord_avatar_url: avatar,
                            linked_at: created,
                        }
                    },
                )
                .collect(),
        ))
    }
    .await;
    caught("discord-linked-users", r)
}

#[derive(Debug, Clone, Serialize)]
pub struct UserRef {
    pub uuid: String,
    pub name: String,
    pub username: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub discord_user_id: Value,
    pub discord_username: Value,
    pub discord_display_name: Value,
    pub discord_avatar_url: Option<String>,
    pub linked_user: Option<UserRef>,
}

const BOT_NO_ACCESS: &str = "Bot cannot access the connected Discord server. Make sure the bot is still a member of the server.";

/// Node: `discord.service.ts:561` (`getMembers`).
pub async fn get_members(state: &AppState, user_id: i32) -> Envelope<Vec<Member>> {
    let r: Out<Vec<Member>> = async {
        let Some((c, gid)) = connected(state, user_id).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        match api::get(state, &format!("/guilds/{gid}"), &[]).await {
            Ok(g) => tracing::debug!(
                "discord-get-members: guildId={gid} guildName={}",
                js_string(opt_get(Some(&g), "name"))
            ),
            Err(e) => {
                tracing::error!(
                    "discord-get-members: cannot access guild {gid} — HTTP {} {}",
                    e.status
                        .map_or_else(|| "undefined".to_owned(), |s| s.to_string()),
                    e.data_message.as_deref().unwrap_or("undefined")
                );
                return Ok(Envelope::fail(BOT_NO_ACCESS));
            }
        }
        let data = api::get(
            state,
            &format!("/guilds/{gid}/members"),
            &[("limit", "1000")],
        )
        .await?;
        let mut members = Vec::new();
        for m in as_list(&data)? {
            let user = prop(m, "user")?;
            if !truthy(opt_get(user, "bot")) {
                members.push(m);
            }
        }
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            r#"SELECT m."discordUserId", u.uuid::text, u.name, u.username
               FROM "userDiscordMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."companyId" = $1 AND m.status = 1"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        let linked: HashMap<String, UserRef> = rows
            .into_iter()
            .map(|(did, uuid, name, username)| {
                (
                    did,
                    UserRef {
                        uuid,
                        name,
                        username,
                    },
                )
            })
            .collect();
        let data = members
            .into_iter()
            .map(|m| {
                let user = opt_get(Some(m), "user");
                let id = opt_get(user, "id");
                let display = nn(opt_get(Some(m), "nick"))
                    .or_else(|| nn(opt_get(user, "global_name")))
                    .or_else(|| nn(opt_get(user, "username")));
                let avatar = opt_get(user, "avatar");
                Member {
                    discord_user_id: nn(id).cloned().unwrap_or(Value::Null),
                    discord_username: nn(opt_get(user, "username"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    discord_display_name: display.cloned().unwrap_or(Value::Null),
                    discord_avatar_url: truthy(avatar).then(|| {
                        format!(
                            "https://cdn.discordapp.com/avatars/{}/{}.png",
                            js_string(id),
                            js_string(avatar)
                        )
                    }),
                    linked_user: if truthy(id) {
                        id.and_then(Value::as_str)
                            .and_then(|i| linked.get(i).cloned())
                    } else {
                        None
                    },
                }
            })
            .collect();
        Ok(success(data))
    }
    .await;
    r.unwrap_or_else(|err| {
        let status = match &err {
            Fail::Discord(e) => e.status,
            _ => None,
        };
        tracing::error!(
            "discord-get-members failed: HTTP {} — {}",
            status.map_or_else(|| "N/A".to_owned(), |s| s.to_string()),
            match &err {
                Fail::Discord(e) => e.data_message.clone().unwrap_or_else(|| e.message.clone()),
                other => other.to_string(),
            }
        );
        match status {
            Some(403) => Envelope::fail(
                "Bot does not have permission to list members. Enable the \"Server Members Intent\" in the Discord Developer Portal for your bot.",
            ),
            Some(401) => Envelope::fail("Discord bot token is invalid or missing"),
            _ => Envelope::fail(UNEXPECTED),
        }
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkUserData {
    pub uuid: String,
    pub user_uuid: String,
    pub user_name: String,
    pub discord_user_id: String,
    pub discord_username: Option<String>,
    pub discord_display_name: Option<String>,
}

/// `users.findFirst({ where: { uuid, userCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_user(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> Result<Option<(i32, String, String)>, sqlx::Error> {
    sqlx::query_as(
        r#"SELECT u.id, u.uuid::text, u.name FROM users u
           WHERE u.uuid = $1::uuid AND u.status = 1
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1)
           LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

/// Node: `discord.service.ts:672` (`linkUser`).
pub async fn link_user(
    state: &AppState,
    user_id: i32,
    dto: &LinkUserDto,
) -> Envelope<LinkUserData> {
    let r: Out<LinkUserData> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((target_id, target_uuid, target_name)) =
            company_user(state, c.company_id, dto.user_uuid()).await?
        else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let discord_user_id = dto.discord_user_id();
        let conflicting: Option<String> = sqlx::query_scalar(
            r#"SELECT u.name FROM "userDiscordMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."discordUserId" = $1 AND m."companyId" = $2 AND m.status = 1
                 AND m."userId" <> $3
               LIMIT 1"#,
        )
        .bind(discord_user_id)
        .bind(c.company_id)
        .bind(target_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(name) = conflicting {
            return Ok(Envelope::fail(format!(
                "This Discord member is already linked to user '{name}'"
            )));
        }
        let row: (String, String, Option<String>, Option<String>) = sqlx::query_as(
            r#"INSERT INTO "userDiscordMapping"
                 ("userId", "companyId", "discordUserId", "discordUsername", "discordDisplayName",
                  "discordAvatarUrl", status)
               VALUES ($1, $2, $3, $4, $5, $6, 1)
               ON CONFLICT ("userId", "companyId") DO UPDATE SET
                 "discordUserId" = EXCLUDED."discordUserId",
                 "discordUsername" = EXCLUDED."discordUsername",
                 "discordDisplayName" = EXCLUDED."discordDisplayName",
                 "discordAvatarUrl" = EXCLUDED."discordAvatarUrl",
                 status = 1,
                 "updatedAt" = $7
               RETURNING uuid::text, "discordUserId", "discordUsername", "discordDisplayName""#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .bind(discord_user_id)
        .bind(LinkUserDto::opt(dto.discord_username.as_ref()))
        .bind(LinkUserDto::opt(dto.discord_display_name.as_ref()))
        .bind(LinkUserDto::opt(dto.discord_avatar_url.as_ref()))
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "User linked to Discord member successfully",
            LinkUserData {
                uuid: row.0,
                user_uuid: target_uuid,
                user_name: target_name,
                discord_user_id: row.1,
                discord_username: row.2,
                discord_display_name: row.3,
            },
        ))
    }
    .await;
    caught("discord-link-user", r)
}

/// Node: `discord.service.ts:767` (`unlinkUser`).
pub async fn unlink_user(state: &AppState, user_id: i32, user_uuid: &str) -> Envelope<()> {
    let r: Out<()> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((target_id, _, _)) = company_user(state, c.company_id, user_uuid).await? else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let mapping: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "userDiscordMapping"
               WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some(id) = mapping else {
            return Ok(Envelope::fail("No active Discord link found for this user"));
        };
        sqlx::query(
            r#"UPDATE "userDiscordMapping" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
        Ok(Envelope {
            success: true,
            message: "User unlinked from Discord successfully".into(),
            data: None,
        })
    }
    .await;
    caught("discord-unlink-user", r)
}

// ───────────────────────── notifikasi ─────────────────────────

#[derive(Debug, Serialize)]
struct Embed<'a> {
    title: &'a str,
    description: &'a str,
    color: &'a Value,
    timestamp: String,
}

#[derive(Debug, Serialize)]
struct MessagePayload<'a> {
    embeds: [Embed<'a>; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
}

/// Node: `discord.service.ts:822` (`sendNotification`). Galat ditelan → `false`.
async fn send_notification(
    state: &AppState,
    company_id: i32,
    channel_id: &str,
    title: &str,
    message: &str,
    color: &Value,
) -> bool {
    let payload = MessagePayload {
        embeds: [Embed {
            title,
            description: message,
            color,
            timestamp: iso_now(),
        }],
        content: None,
    };
    match api::post(state, &format!("/channels/{channel_id}/messages"), &payload).await {
        Ok(_) => true,
        Err(e) => {
            tracing::error!(
                "Discord send notification failed for company {company_id}: {}",
                e.message
            );
            false
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyData {
    pub channel_id: String,
    pub channel_name: String,
}

/// Node: `discord.service.ts:872` (`notifyProject`). `color`: `None` = argumen
/// `undefined` (bawaan 5763719), selain itu nilai apa adanya (termasuk `null`).
pub async fn notify_project(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    title: &str,
    message: &str,
    color: Option<&Value>,
) -> Envelope<NotifyData> {
    let r: Out<NotifyData> = async {
        let Some((c, _)) = connected(state, user_id).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        let mapping: Option<(String, String)> = sqlx::query_as(
            r#"SELECT m."discordChannelId", m."discordChannelName"
               FROM "projectDiscordMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1 AND p.uuid = $2::uuid AND p.status = 1
               LIMIT 1"#,
        )
        .bind(c.company_id)
        .bind(project_uuid)
        .fetch_optional(&state.db)
        .await?;
        let Some((channel_id, channel_name)) = mapping else {
            return Ok(Envelope::fail(
                "Project not found or not linked to a Discord channel",
            ));
        };
        let green = Value::from(GREEN);
        let color = color.map_or(&green, |v| v);
        let color = normalize_number(color);
        if !send_notification(state, c.company_id, &channel_id, title, message, &color).await {
            return Ok(Envelope::fail("Failed to send notification to Discord"));
        }
        Ok(Envelope::ok(
            "Notification sent successfully",
            NotifyData {
                channel_id,
                channel_name,
            },
        ))
    }
    .await;
    caught("discord-notify-project", r)
}

/// Angka JSON bulat dikirim tanpa `.0` (`JSON.stringify(5)` = `5`).
fn normalize_number(v: &Value) -> Value {
    match v.as_f64() {
        Some(f) if v.is_f64() && f.fract() == 0.0 && f.abs() < 9.0e15 =>
        {
            #[allow(clippy::cast_possible_truncation)]
            Value::from(f as i64)
        }
        _ => v.clone(),
    }
}

/// Node: `discord.service.ts:930` (`testNotification`).
pub async fn test_notification(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
) -> Envelope<NotifyData> {
    let green = Value::from(GREEN);
    notify_project(
        state,
        user_id,
        project_uuid,
        "Discord Integration Test",
        "This is a test notification from Mudah. Your Discord integration is working correctly.",
        Some(&green),
    )
    .await
}

// ───────────────────────── default task channel ─────────────────────────

/// `{ channelId, channelName }`. `channel_name: None` = kunci hilang (`undefined`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultChannel {
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<Value>,
}

/// Node: `discord.service.ts:940` (`getDefaultTaskChannel`).
pub async fn get_default_task_channel(state: &AppState, user_id: i32) -> Envelope<DefaultChannel> {
    let r: Out<DefaultChannel> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let (id, name) = default_channel(state, c.company_id).await?;
        Ok(success(DefaultChannel {
            channel_id: id,
            channel_name: Some(name.map_or(Value::Null, Value::String)),
        }))
    }
    .await;
    caught("discord-get-default-task-channel", r)
}

async fn default_channel(
    state: &AppState,
    company_id: i32,
) -> Result<(Option<String>, Option<String>), sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "discordDefaultTaskChannelId", "discordDefaultTaskChannelName"
           FROM company WHERE id = $1 LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.unwrap_or((None, None)))
}

/// Node: `discord.service.ts:974` (`setDefaultTaskChannel`).
pub async fn set_default_task_channel(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &SetDefaultTaskChannelDto,
) -> Envelope<DefaultChannel> {
    let r: Out<DefaultChannel> = async {
        let Some((c, gid)) = connected(state, user_id).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        let channel_id = dto
            .channel_id
            .as_ref()
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(channel_id) = channel_id else {
            update_company(
                state,
                audit,
                c.company_id,
                &[
                    ("discordDefaultTaskChannelId", None),
                    ("discordDefaultTaskChannelName", None),
                ],
            )
            .await?;
            return Ok(Envelope::ok(
                "Default task channel unset successfully",
                DefaultChannel {
                    channel_id: None,
                    channel_name: Some(Value::Null),
                },
            ));
        };
        if !is_snowflake(channel_id) {
            return Ok(Envelope::fail("Invalid Discord channel id"));
        }
        let Some(ch) = guild_text_channel(state, &gid, channel_id).await? else {
            return Ok(Envelope::fail(
                "Channel not found in your connected Discord server",
            ));
        };
        // `guildChannel.name as string`: tidak ada → `undefined` (Prisma melewatkan kolom,
        // kunci hilang di respons); `null` → NULL; selain string → Prisma menolak.
        let name = ch.get("name");
        match name {
            None => {
                update_company(
                    state,
                    audit,
                    c.company_id,
                    &[("discordDefaultTaskChannelId", Some(channel_id))],
                )
                .await?;
            }
            Some(Value::Null) | Some(Value::String(_)) => {
                update_company(
                    state,
                    audit,
                    c.company_id,
                    &[
                        ("discordDefaultTaskChannelId", Some(channel_id)),
                        (
                            "discordDefaultTaskChannelName",
                            name.and_then(Value::as_str),
                        ),
                    ],
                )
                .await?;
            }
            Some(_) => {
                return Err(Fail::Js(
                    "Invalid value for argument `discordDefaultTaskChannelName`".into(),
                ));
            }
        }
        Ok(Envelope::ok(
            "Default task channel set successfully",
            DefaultChannel {
                channel_id: Some(channel_id.to_owned()),
                channel_name: name.cloned(),
            },
        ))
    }
    .await;
    caught("discord-set-default-task-channel", r)
}

// ───────────────────────── post tasks ─────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostResult {
    pub project_uuid: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub using_default_channel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mentioned_user_count: Option<u8>,
}

impl PostResult {
    fn failed(project_uuid: &str, message: &'static str) -> Self {
        Self {
            project_uuid: project_uuid.to_owned(),
            success: false,
            message: Some(message),
            channel_id: None,
            channel_name: None,
            using_default_channel: None,
            task_count: None,
            mentioned_user_count: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostTasksData {
    pub results: Vec<PostResult>,
    pub total_posted: usize,
    pub total_failed: usize,
}

struct PostCtx<'a> {
    company_id: i32,
    default_id: Option<&'a str>,
    default_name: Option<&'a str>,
    mention: Option<&'a str>,
}

async fn post_one(
    state: &AppState,
    ctx: &PostCtx<'_>,
    item: &TaskItem,
) -> Result<PostResult, Fail> {
    let project: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT p.id, p.name FROM project p
           WHERE p.uuid = $1::uuid AND p.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           LIMIT 1"#,
    )
    .bind(&item.project_uuid)
    .bind(ctx.company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((project_id, project_name)) = project else {
        return Ok(PostResult::failed(&item.project_uuid, "Project not found"));
    };
    let (channel_id, channel_name) = match ctx.default_id {
        Some(id) => (id.to_owned(), ctx.default_name.unwrap_or(id).to_owned()),
        None => {
            let mapping: Option<(String, String)> = sqlx::query_as(
                r#"SELECT "discordChannelId", "discordChannelName" FROM "projectDiscordMapping"
                   WHERE "projectId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
            )
            .bind(project_id)
            .bind(ctx.company_id)
            .fetch_optional(&state.db)
            .await?;
            match mapping {
                Some(m) => m,
                None => {
                    return Ok(PostResult::failed(
                        &item.project_uuid,
                        "Project is not linked to a Discord channel",
                    ));
                }
            }
        }
    };
    let task_list = item
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {t}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    if js_len(&task_list) > 4000 {
        return Ok(PostResult::failed(
            &item.project_uuid,
            "Total task list text is too long for Discord (max ~4000 characters)",
        ));
    }
    let title = format!("📋 {project_name}");
    let green = Value::from(GREEN);
    let payload = MessagePayload {
        embeds: [Embed {
            title: &title,
            description: &task_list,
            color: &green,
            timestamp: iso_now(),
        }],
        content: ctx.mention,
    };
    if let Err(e) = api::post(state, &format!("/channels/{channel_id}/messages"), &payload).await {
        tracing::error!(
            "Discord post tasks failed for project {}: {}",
            item.project_uuid,
            e.message
        );
        return Ok(PostResult::failed(
            &item.project_uuid,
            "Failed to send message to Discord",
        ));
    }
    Ok(PostResult {
        project_uuid: item.project_uuid.clone(),
        success: true,
        message: None,
        channel_id: Some(channel_id),
        channel_name: Some(channel_name),
        using_default_channel: Some(ctx.default_id.is_some()),
        task_count: Some(item.tasks.len()),
        mentioned_user_count: Some(u8::from(ctx.mention.is_some())),
    })
}

/// Node: `discord.service.ts:1054` (`postTasksMessage`). Item diproses paralel
/// (`Promise.all`); satu lemparan (DB) menggagalkan semuanya → pesan tak terduga.
pub async fn post_tasks(
    state: &AppState,
    user_id: i32,
    items: &[TaskItem],
) -> Envelope<PostTasksData> {
    let r: Out<PostTasksData> = async {
        let Some((c, _)) = connected_for(state, user_id, ADMINS_MANAGER).await? else {
            return Ok(Envelope::fail("Discord not connected"));
        };
        let (default_id, default_name) = default_channel(state, c.company_id).await?;
        let caller: Option<String> = sqlx::query_scalar(
            r#"SELECT "discordUserId" FROM "userDiscordMapping"
               WHERE "companyId" = $1 AND "userId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(c.company_id)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
        let mention = caller.filter(|d| !d.is_empty()).map(|d| format!("<@{d}>"));
        let ctx = PostCtx {
            company_id: c.company_id,
            default_id: default_id.as_deref().filter(|d| !d.is_empty()),
            default_name: default_name.as_deref(),
            mention: mention.as_deref(),
        };
        let outcomes =
            futures::future::join_all(items.iter().map(|item| post_one(state, &ctx, item))).await;
        let results = outcomes.into_iter().collect::<Result<Vec<_>, _>>()?;
        let total_posted = results.iter().filter(|r| r.success).count();
        let total_failed = results.len() - total_posted;
        Ok(Envelope {
            success: total_failed == 0,
            message: if total_failed == 0 {
                "Task lists posted to Discord successfully".to_owned()
            } else {
                format!("{total_posted} posted, {total_failed} failed")
            },
            data: Some(PostTasksData {
                results,
                total_posted,
                total_failed,
            }),
        })
    }
    .await;
    caught("discord-post-tasks", r)
}
