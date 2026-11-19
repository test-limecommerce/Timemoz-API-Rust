//! Handler `src/auth/auth.controller.ts`. Semua POST memakai `@HttpCode(200)`.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mz_account::invite_token::is_invite_token_format;
use mz_account::user_ops;
use mz_audit::AuditHandle;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value};

use crate::dto::{
    ChangePasswordDto, ConfirmRegisterDto, CreateOrganizationDto, ForgotPasswordDto, LoginDto,
    RegisterDto, RegisterInviteDto, TokenDto, UsernameDto, text,
};
use crate::js;
use crate::service::oauth::{self, Action, CallbackAuth};
use crate::service::oauth_csrf;
use crate::service::{invite, login, organization, password, register, session};

type Pairs = Query<Vec<(String, String)>>;

/// Node: src/auth/auth.controller.ts:85 (`login`).
pub async fn login(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<LoginDto>,
) -> ApiResult<Json<Value>> {
    let username = text(&dto.username).to_lowercase();
    let result = login::login(
        &state,
        &audit,
        login::LoginInput {
            username: &username,
            password: text(&dto.password),
            mac_address: dto.mac_address.as_ref().and_then(Value::as_str),
            os: dto.os.as_ref().and_then(Value::as_str),
            platform: dto.platform.as_ref().and_then(Value::as_str),
        },
    )
    .await?;
    Ok(Json(result))
}

/// Node: src/auth/auth.controller.ts:103 (`confirm`).
pub async fn confirm(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<TokenDto>,
) -> ApiResult<Json<Value>> {
    let token = text(&dto.token);
    let password = text(&dto.password);
    // Token undangan memasang password BARU: minimum 8 (H3).
    if !token.is_empty() && is_invite_token_format(token) && js::utf16_len(password) < 8 {
        return Err(ApiError::Validation(vec![
            "password must be longer than or equal to 8 characters".into(),
        ]));
    }
    let username = text(&dto.username).to_lowercase();
    let claim = user_ops::find_token(&state, &audit, token, &username, password).await?;
    Ok(Json(
        user_ops::confirm(&state, claim.id, claim.activated).await?,
    ))
}

/// Node: src/auth/auth.controller.ts:124 (`register`).
pub async fn register(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<RegisterDto>,
) -> Json<Value> {
    Json(register::register(&state, &audit, &dto).await)
}

/// Node: src/auth/auth.controller.ts:132 (`findMe`).
pub async fn me(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(session::find_me(&state, user_id).await?))
}

/// Node: src/auth/auth.controller.ts:164 (`logout`).
pub async fn logout(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user = ctx.require_user()?;
    let user_id = ctx.require_user_id()?;
    if user.raw_token.is_empty() {
        return Err(ApiError::Bare(StatusCode::UNAUTHORIZED));
    }
    // Hanya device pemanggil sendiri yang diputus.
    let scope = session::device_scope(user.claims.get("sid"), &user.mac_address);
    Ok(Json(
        session::logout(&state, &user.raw_token, user_id, scope).await?,
    ))
}

/// Node: src/auth/auth.controller.ts:186 (`webSession`).
pub async fn web_session(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user = ctx.require_user()?;
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        session::create_web_session(&state, user_id, user.claims.get("sid"), &user.raw_token)
            .await?,
    ))
}

/// Node: src/auth/auth.controller.ts:217 (`changePassword`).
pub async fn change_password(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ChangePasswordDto>,
) -> ApiResult<Json<Value>> {
    let user = ctx.require_user()?;
    let user_id = ctx.require_user_id()?;
    let result = user_ops::change_password(
        &state,
        &audit,
        user_id,
        text(&dto.old_password),
        text(&dto.new_password),
        text(&dto.confirm_new_password),
    )
    .await?;
    let Value::Object(mut result) = result else {
        return Ok(Json(result));
    };
    // `{ ...result, data: {...} }`: posisi kunci `data` tetap (urutan objek JS).
    let Some(slot) = result.get_mut("data") else {
        return Ok(Json(Value::Object(result)));
    };
    // Semua token lama sudah dicabut; pengganti terbit SESUDAH commit, `sid` sama.
    let sid = user
        .claims
        .get("sid")
        .filter(|s| js::truthy(Some(s)))
        .cloned();
    let access_token = session::sign_user_token(&state, user_id, sid)?;
    let mut data = match slot.take() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    data.insert("accessToken".into(), Value::String(access_token));
    *slot = Value::Object(data);
    Ok(Json(Value::Object(result)))
}

/// Node: src/auth/auth.controller.ts:243 (`confirmRegister`).
pub async fn confirm_register(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ConfirmRegisterDto>,
) -> Json<Value> {
    Json(password::confirm_register(&state, &audit, text(&dto.username), text(&dto.token)).await)
}

/// Node: src/auth/auth.controller.ts:250 (`resendConfirmation`).
pub async fn resend_confirmation(
    State(state): State<AppState>,
    ValidatedJson(dto): ValidatedJson<UsernameDto>,
) -> Json<Value> {
    Json(password::resend_confirmation(&state, text(&dto.username)).await)
}

/// Node: src/auth/auth.controller.ts:257 (`createOrganization`).
pub async fn create_organization(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<CreateOrganizationDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let result = organization::create_organization(&state, &audit, user_id, &dto).await;
    // Cache `GET /auth/me` masih memegang `hasCreateOrganization: false`.
    if result.get("success") == Some(&Value::Bool(true)) {
        session::invalidate_me_cache(&state, user_id).await?;
    }
    Ok(Json(result))
}

/// `@Query('username')` — `?username=a&username=b` / `username[]=a` bukan string.
pub fn single_query_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    let nested = format!("{key}[");
    if pairs.iter().any(|(k, _)| k.starts_with(&nested)) {
        return None;
    }
    let mut values = pairs.iter().filter(|(k, _)| k == key);
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(first.1.as_str())
}

/// Node: src/auth/auth.controller.ts:275 (`checkEmailVerification`).
pub async fn check_verification(State(state): State<AppState>, Query(pairs): Pairs) -> Json<Value> {
    match single_query_value(&pairs, "username").filter(|u| !u.is_empty()) {
        Some(username) => Json(password::check_email_verification(&state, username).await),
        None => Json(serde_json::json!({ "success": false, "message": "Username is required" })),
    }
}

/// Node: src/auth/auth.controller.ts:285 (`sendEmailForgotPassword`).
pub async fn forgot_password_generate(
    State(state): State<AppState>,
    ValidatedJson(dto): ValidatedJson<UsernameDto>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        user_ops::send_email_forgot_password(&state, text(&dto.username)).await?,
    ))
}

/// Node: src/auth/auth.controller.ts:296 (`forgotPassword`).
pub async fn forgot_password(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ForgotPasswordDto>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        user_ops::forgot_password(
            &state,
            &audit,
            text(&dto.forgot_password_uuid),
            text(&dto.password),
            text(&dto.username),
        )
        .await?,
    ))
}

/// Node: src/auth/auth.controller.ts:304 (`getInviteInfo`).
pub async fn invite_info(
    State(state): State<AppState>,
    PathParams(invite_code): PathParams<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(invite::get_invite_info(&state, &invite_code).await?))
}

/// Node: src/auth/auth.controller.ts:311 (`registerViaInvite`).
pub async fn register_invite(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<RegisterInviteDto>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        invite::register_via_invite(&state, &audit, &dto).await?,
    ))
}

fn location_header(url: &str) -> ApiResult<HeaderValue> {
    HeaderValue::from_str(url).map_err(ApiError::internal)
}

/// `this.redirect(location)` passport: 302 tanpa body.
fn passport_redirect(url: &str) -> ApiResult<Response> {
    Ok((
        StatusCode::FOUND,
        [
            (header::LOCATION, location_header(url)?),
            (header::CONTENT_LENGTH, HeaderValue::from_static("0")),
        ],
    )
        .into_response())
}

/// `res.redirect(url)` Express, lihat [`mz_core::redirect`].
fn express_redirect(headers: &axum::http::HeaderMap, url: &str) -> Response {
    mz_core::redirect::redirect_302(headers, url)
}

/// Node: src/auth/auth.controller.ts:322 (`googleLoginInit`, `GoogleLoginGuard`).
pub async fn google_login(
    State(state): State<AppState>,
    Query(pairs): Pairs,
) -> ApiResult<Response> {
    let raw = oauth::build_state(Action::Login, &oauth::query_object(&pairs));
    signed_redirect(&state, &raw)
}

/// Node: src/auth/auth.controller.ts:329 (`googleRegisterInit`, `GoogleRegisterGuard`).
pub async fn google_register(
    State(state): State<AppState>,
    Query(pairs): Pairs,
) -> ApiResult<Response> {
    let raw = oauth::build_state(Action::Register, &oauth::query_object(&pairs));
    signed_redirect(&state, &raw)
}

/// `issueSignedState` (Node `google-oauth.guard.ts`): tanda tangani state, pasang
/// cookie nonce di respons redirect yang sama.
fn signed_redirect(state: &AppState, raw: &str) -> ApiResult<Response> {
    let nonce = oauth_csrf::new_nonce();
    let signed = oauth_csrf::sign_state(
        raw,
        &nonce,
        &state.config.jwt_secret,
        chrono::Utc::now().timestamp(),
    )
    .ok_or_else(|| ApiError::internal(anyhow::anyhow!("gagal menandatangani state OAuth")))?;
    let mut res = passport_redirect(&oauth::authorize_location(state, Some(&signed)))?;
    if let Some(cookie) = oauth_csrf::set_cookie(state, &nonce) {
        res.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(res)
}

/// Node: src/auth/auth.controller.ts:336 (`googleCallback`, `GoogleCallbackGuard`).
pub async fn google_callback(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: axum::http::HeaderMap,
    Query(pairs): Pairs,
) -> ApiResult<Response> {
    let query = oauth::query_object(&pairs);
    // CSRF (`GoogleCallbackGuard.canActivate` di Node): hanya request ber-`code`
    // (tanpa `error`) yang diverifikasi — SEBELUM `code` ditukar ke Google.
    let has_code = !js::truthy(query.get("error")) && js::truthy(query.get("code"));
    let mut verified_payload: Option<String> = None;
    if has_code {
        verified_payload = oauth_csrf::verify_state(
            query.get("state").and_then(Value::as_str),
            oauth_csrf::read_cookie(&headers, oauth_csrf::OAUTH_STATE_COOKIE),
            &state.config.jwt_secret,
            chrono::Utc::now().timestamp(),
        );
        if verified_payload.is_none() {
            // `GoogleOAuthStateFilter`: kembali ke halaman callback dashboard.
            let location = format!(
                "{}/auth/google-callback?success=false&message={}",
                mz_account::util::env_or_undefined(state.config.var("HOST_FE")),
                mz_account::util::encode_uri_component(oauth_csrf::INVALID_STATE_MESSAGE)
            );
            let mut res = express_redirect(&headers, &location);
            res.headers_mut()
                .append(header::SET_COOKIE, oauth_csrf::clear_cookie(&state));
            return Ok(res);
        }
    }
    let user = match oauth::authenticate_callback(&state, &query).await {
        CallbackAuth::User(u) => u,
        CallbackAuth::Unauthorized => return Err(ApiError::Bare(StatusCode::UNAUTHORIZED)),
        CallbackAuth::Redirect(url) => return passport_redirect(&url),
        CallbackAuth::Error(err) => return Err(ApiError::Internal(err)),
    };
    // Payload yang SUDAH diverifikasi, bukan `state` mentah dari query.
    let location =
        oauth::callback_location(&state, &audit, &user, verified_payload.as_deref()).await?;
    let mut res = express_redirect(&headers, &location);
    res.headers_mut()
        .append(header::SET_COOKIE, oauth_csrf::clear_cookie(&state));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn username_query_must_be_single_string() {
        assert_eq!(
            single_query_value(&pairs(&[("username", "a@b.co")]), "username"),
            Some("a@b.co")
        );
        assert_eq!(
            single_query_value(&pairs(&[("username", "a"), ("username", "b")]), "username"),
            None
        );
        assert_eq!(
            single_query_value(&pairs(&[("username[]", "a")]), "username"),
            None
        );
        assert_eq!(single_query_value(&pairs(&[]), "username"), None);
    }
}
