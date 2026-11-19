//! Handler `src/integrations/discord/discord.controller.ts`.

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use mz_audit::AuditHandle;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_int_common::envelope::Envelope;
use mz_int_common::nest_jwt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::dto::{
    LinkChannelDto, LinkUserDto, PostTasksDto, SendNotificationDto, SetDefaultTaskChannelDto,
};
use crate::service::{self as svc};
use crate::util::{QVal, encode_uri_component, query_param};

/// VIEW_CHANNEL + SEND_MESSAGES + EMBED_LINKS + READ_MESSAGE_HISTORY.
pub const BOT_PERMISSIONS: u32 = 84_992;
/// `type` payload state OAuth.
const STATE_TYPE: &str = "discord-connect";

type J<T> = ApiResult<Json<Envelope<T>>>;

#[derive(Deserialize)]
pub struct ProjectPath {
    #[serde(rename = "projectUuid")]
    project_uuid: String,
}

#[derive(Deserialize)]
pub struct UserPath {
    #[serde(rename = "userUuid")]
    user_uuid: String,
}

/// Node: `discord.controller.ts:91` (`GET status`).
pub async fn status(State(state): State<AppState>, ctx: RequestCtx) -> J<svc::DiscordStatus> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::connection_status(&state, uid).await))
}

#[derive(Serialize)]
pub struct ConnectUrl {
    url: String,
}

#[derive(Serialize)]
pub struct ConnectResponse {
    success: bool,
    data: ConnectUrl,
}

fn nonce() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `process.env.X` di template literal (`undefined` bila tidak diset).
fn env_or_undefined(state: &AppState, key: &str) -> String {
    state
        .config
        .var(key)
        .unwrap_or_else(|| "undefined".to_owned())
}

/// URL otorisasi bot Discord (untuk browser; konstanta Node).
pub fn authorize_url(client_id: &str, redirect_uri: &str, signed_state: &str) -> String {
    format!(
        "https://discord.com/oauth2/authorize?response_type=code&client_id={client_id}\
         &scope=bot%20applications.commands&permissions={BOT_PERMISSIONS}\
         &redirect_uri={}&state={}",
        encode_uri_component(redirect_uri),
        encode_uri_component(signed_state)
    )
}

/// Node: `discord.controller.ts:128` (`POST connect`, `@HttpCode(200)`).
pub async fn connect(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<ConnectResponse>> {
    let uid = ctx.require_user_id()?;
    let mut payload = Map::new();
    payload.insert("userId".into(), Value::from(uid));
    payload.insert("nonce".into(), Value::from(nonce()));
    payload.insert("type".into(), Value::from(STATE_TYPE));
    let token = nest_jwt::sign(&state, payload, Some(600)).map_err(ApiError::internal)?;
    let url = authorize_url(
        &env_or_undefined(&state, "DISCORD_CLIENT_ID"),
        &env_or_undefined(&state, "DISCORD_REDIRECT_URI"),
        &token,
    );
    Ok(Json(ConnectResponse {
        success: true,
        data: ConnectUrl { url },
    }))
}

/// `res.redirect(url)` Express (`@Redirect()`), lihat [`mz_core::redirect`].
fn redirect(headers: &HeaderMap, url: &str) -> Response {
    mz_core::redirect::redirect_302(headers, url)
}

/// Node: `discord.controller.ts:156` (`GET callback`, `@Redirect()`, tanpa guard).
pub async fn callback(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let q = q.unwrap_or_default();
    let frontend = state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".to_owned());
    let error_url = |msg: &str| {
        format!(
            "{frontend}/integrations/discord?status=error&message={}",
            encode_uri_component(msg)
        )
    };
    let guild = query_param(&q, "guild_id");
    let st = query_param(&q, "state");
    let error = query_param(&q, "error");
    let has = |v: &Option<QVal>| v.as_ref().is_some_and(QVal::truthy);
    if has(&error) || !has(&guild) || !has(&st) {
        // `error ?? 'Discord authorization was cancelled or failed'` (`''` tetap `''`).
        let msg = error.as_ref().map_or_else(
            || "Discord authorization was cancelled or failed".to_owned(),
            QVal::js_string,
        );
        return redirect(&headers, &error_url(&msg));
    }
    // `jwtService.verify(state)`: array/objek → "jwt must be a string".
    let payload = match &st {
        Some(QVal::One(token)) => nest_jwt::verify(&state, token),
        _ => None,
    };
    let Some(payload) =
        payload.filter(|p| p.get("type").and_then(Value::as_str) == Some(STATE_TYPE))
    else {
        return redirect(
            &headers,
            &error_url("Invalid or expired authorization state"),
        );
    };
    let guild_id = guild.as_ref().map(QVal::js_string).unwrap_or_default();
    let result = svc::handle_oauth_callback(&state, &audit, payload.get("userId"), &guild_id).await;
    if result.success {
        return redirect(
            &headers,
            &format!("{frontend}/integrations/discord?status=connected"),
        );
    }
    redirect(&headers, &error_url(result.message))
}

/// Node: `discord.controller.ts:207` (`DELETE disconnect`).
pub async fn disconnect(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> J<()> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::disconnect(&state, &audit, uid).await))
}

/// Node: `discord.controller.ts:264` (`GET channels`).
pub async fn channels(State(state): State<AppState>, ctx: RequestCtx) -> J<Vec<svc::Channel>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::get_channels(&state, uid).await))
}

/// Node: `discord.controller.ts:321` (`GET linked-channels`).
pub async fn linked_channels(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> J<Vec<svc::LinkedChannel>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::get_linked_channels(&state, uid).await))
}

/// Node: `discord.controller.ts:374` (`POST channels/link`, `@HttpCode(200)`).
pub async fn link_channel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<LinkChannelDto>,
) -> J<svc::LinkChannelData> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::link_channel(&state, uid, &dto).await))
}

/// Node: `discord.controller.ts:398` (`DELETE channels/:projectUuid/link`).
pub async fn unlink_channel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(p): PathParams<ProjectPath>,
) -> J<()> {
    let uid = ctx.require_user_id()?;
    Ok(Json(
        svc::unlink_channel(&state, uid, &p.project_uuid).await,
    ))
}

/// Node: `discord.controller.ts:441` (`POST projects/:projectUuid/notify`).
pub async fn notify(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(p): PathParams<ProjectPath>,
    ValidatedJson(dto): ValidatedJson<SendNotificationDto>,
) -> J<svc::NotifyData> {
    let uid = ctx.require_user_id()?;
    let title = dto
        .title
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = dto
        .message
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(Json(
        svc::notify_project(
            &state,
            uid,
            &p.project_uuid,
            title,
            message,
            dto.color.as_ref(),
        )
        .await,
    ))
}

/// Node: `discord.controller.ts:483` (`POST projects/:projectUuid/test-notify`).
pub async fn test_notify(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(p): PathParams<ProjectPath>,
) -> J<svc::NotifyData> {
    let uid = ctx.require_user_id()?;
    Ok(Json(
        svc::test_notification(&state, uid, &p.project_uuid).await,
    ))
}

/// Node: `discord.controller.ts:525` (`GET settings/default-task-channel`).
pub async fn get_default_task_channel(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> J<svc::DefaultChannel> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::get_default_task_channel(&state, uid).await))
}

/// Node: `discord.controller.ts:580` (`PUT settings/default-task-channel`).
pub async fn set_default_task_channel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<SetDefaultTaskChannelDto>,
) -> J<svc::DefaultChannel> {
    let uid = ctx.require_user_id()?;
    Ok(Json(
        svc::set_default_task_channel(&state, &audit, uid, &dto).await,
    ))
}

/// Node: `discord.controller.ts:698` (`POST post-tasks`, `@HttpCode(200)`).
pub async fn post_tasks(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<PostTasksDto>,
) -> J<svc::PostTasksData> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::post_tasks(&state, uid, &dto.items()).await))
}

/// Node: `discord.controller.ts:763` (`GET linked-users`).
pub async fn linked_users(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> J<Vec<svc::LinkedUser>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::get_linked_users(&state, uid).await))
}

/// Node: `discord.controller.ts:832` (`GET members`).
pub async fn members(State(state): State<AppState>, ctx: RequestCtx) -> J<Vec<svc::Member>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::get_members(&state, uid).await))
}

/// Node: `discord.controller.ts:880` (`POST users/link`, `@HttpCode(200)`).
pub async fn link_user(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<LinkUserDto>,
) -> J<svc::LinkUserData> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::link_user(&state, uid, &dto).await))
}

/// Node: `discord.controller.ts:904` (`DELETE users/:userUuid/link`).
pub async fn unlink_user(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(p): PathParams<UserPath>,
) -> J<()> {
    let uid = ctx.require_user_id()?;
    Ok(Json(svc::unlink_user(&state, uid, &p.user_uuid).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_shape() {
        assert_eq!(
            authorize_url(
                "123",
                "https://api.x/integrations/discord/callback",
                "a.b-c"
            ),
            "https://discord.com/oauth2/authorize?response_type=code&client_id=123\
             &scope=bot%20applications.commands&permissions=84992\
             &redirect_uri=https%3A%2F%2Fapi.x%2Fintegrations%2Fdiscord%2Fcallback&state=a.b-c"
        );
    }

    #[test]
    fn nonce_is_32_hex() {
        let n = nonce();
        assert_eq!(n.len(), 32);
        assert!(n.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
