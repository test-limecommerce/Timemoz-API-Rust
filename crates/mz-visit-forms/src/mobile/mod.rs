//! Port `mobile-store-visit-forms.controller.ts` (7 route `/v1/mobile/store-visits/:visitUuid/forms/**`)
//! + `mobile-store-visit-forms.service.ts` + DTO. (Pemilik file: fork G / mobile.)
//!
//! Setiap endpoint dibatasi pemilik kunjungan. `visitUuid`/`templateUuid` TANPA
//! `ParseUUIDPipe` di Node: string non-UUID sampai ke Prisma (P2023 → 500), ditiru
//! [`visit::prisma_uuid`]. Pembacaan `storeVisit`/`store`: [`visit`] (titik temu tahap S3).

pub mod service;
pub mod visit;

use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use mz_core::enums::{FeatureModule, Role};
use mz_core::extract::{Validate, ValidatedJson, check_int_range, validator_js_length};
use mz_core::gate::guard;
use mz_core::plan::PlanFeature;
use mz_core::rate_limit::RateRule;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::consts::MAX_FILE_BYTES;
use crate::files::UploadRequest;

const MOBILE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// Limiter `main.ts` (per token).
pub mod limits {
    use super::*;
    pub const READ: RateRule = RateRule::token("rl:store-visit-forms-read", 60, MINUTE);
    pub const DRAFT: RateRule = RateRule::token("rl:store-visit-forms-draft", 30, MINUTE);
    pub const SUBMIT: RateRule = RateRule::token("rl:store-visit-forms-submit", 10, MINUTE);
    pub const FILE_URL: RateRule = RateRule::token("rl:store-visit-forms-file-url", 120, HOUR);
    pub const FILE_CONFIRM: RateRule =
        RateRule::token("rl:store-visit-forms-file-confirm", 120, HOUR);
}

/// Gerbang kelas `MobileStoreVisitFormsController`: `@RequireFeature('store-visit-forms')`,
/// `@RequireModule(SALES_TRACKER)`, `@RequirePlanFeature(FORM)`, `@Roles(SA, SUPER, ADMIN,
/// MANAGER, STAFF)`. `@SkipThrottle()`/`@Throttle()` tanpa efek (ThrottlerGuard tidak terdaftar).
const BASE: RouteSpec = RouteSpec::jwt()
    .feature("store-visit-forms")
    .modules(&[FeatureModule::SALES_TRACKER])
    .plan_features(&[PlanFeature::Form])
    .roles(MOBILE_ROLES);

const LIST: RouteSpec = BASE.rate_limits(&[limits::READ]);
/// `@RequirePlanFeature(ORDER)` route-level MENIMPA FORM kelas (`getAllAndOverride`). Pola
/// express `/forms/:templateUuid` ikut mencocokkan `order-choices` → limiter baca berlaku.
const ORDER_CHOICES: RouteSpec = BASE
    .plan_features(&[PlanFeature::Order])
    .rate_limits(&[limits::READ]);
const GET_FORM: RouteSpec = BASE.rate_limits(&[limits::READ]);
const DRAFT: RouteSpec = BASE.rate_limits(&[limits::DRAFT]);
const SUBMIT: RouteSpec = BASE.rate_limits(&[limits::SUBMIT]);
const FILE_URL: RouteSpec = BASE.rate_limits(&[limits::FILE_URL]);
const FILE_CONFIRM: RouteSpec = BASE.rate_limits(&[limits::FILE_CONFIRM]);

const FORMS: &str = "/v1/mobile/store-visits/{visit_uuid}/forms";

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(FORMS, guard(state, LIST, get(h_list)))
        .route(
            &format!("{FORMS}/order-choices"),
            guard(state, ORDER_CHOICES, get(h_order_choices)),
        )
        .route(
            &format!("{FORMS}/{{template_uuid}}"),
            guard(state, GET_FORM, get(h_get_form)),
        )
        .route(
            &format!("{FORMS}/{{template_uuid}}/draft"),
            guard(state, DRAFT, put(h_draft)),
        )
        .route(
            &format!("{FORMS}/{{template_uuid}}/submit"),
            guard(state, SUBMIT, post(h_submit)),
        )
        .route(
            &format!("{FORMS}/{{template_uuid}}/files"),
            guard(state, FILE_URL, post(h_file_url)),
        )
        .route(
            &format!("{FORMS}/{{template_uuid}}/files/{{file_uuid}}/confirm"),
            guard(state, FILE_CONFIRM, post(h_file_confirm)),
        )
}

// ── DTO ──────────────────────────────────────────────────────────────────────

fn is_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_)))
}

/// `SaveFormDraftDto` — `@IsObject() answers`.
#[derive(Debug, Deserialize)]
pub struct SaveFormDraftDto {
    #[serde(default)]
    pub answers: Option<Value>,
}

impl Validate for SaveFormDraftDto {
    fn validate(&self) -> Vec<String> {
        if is_object(self.answers.as_ref()) {
            Vec::new()
        } else {
            vec!["answers must be an object".to_owned()]
        }
    }
}

/// `SubmitFormDto` — `@IsObject() answers`, `@IsUUID() clientRequestId`.
#[derive(Debug, Deserialize)]
pub struct SubmitFormDto {
    #[serde(default)]
    pub answers: Option<Value>,
    #[serde(default, rename = "clientRequestId")]
    pub client_request_id: Option<Value>,
}

impl Validate for SubmitFormDto {
    fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !is_object(self.answers.as_ref()) {
            errors.push("answers must be an object".to_owned());
        }
        let ok = self
            .client_request_id
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(mz_core::uuid_rules::is_uuid);
        if !ok {
            errors.push("clientRequestId must be a UUID".to_owned());
        }
        errors
    }
}

/// `MEDIA_TYPE_PATTERN` `/^[a-z0-9!#$&^_.+-]{1,64}\/[a-z0-9!#$&^_.+-]{1,64}$/i`.
pub fn is_media_type(s: &str) -> bool {
    let part = |p: &str| {
        (1..=64).contains(&p.chars().count())
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c))
    };
    s.split_once('/').is_some_and(|(a, b)| part(a) && part(b))
}

/// `CreateFormFileUploadUrlDto`. Urutan pesan per properti = dekorator dari bawah.
#[derive(Debug, Deserialize)]
pub struct CreateFormFileUploadUrlDto {
    #[serde(default, rename = "fieldPath")]
    pub field_path: Option<Value>,
    #[serde(default, rename = "contentType")]
    pub content_type: Option<Value>,
    #[serde(default, rename = "sizeBytes")]
    pub size_bytes: Option<Value>,
}

impl CreateFormFileUploadUrlDto {
    /// Body yang sudah lolos validasi.
    pub fn to_request(&self) -> UploadRequest {
        let text = |v: &Option<Value>| {
            v.as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let size = self
            .size_bytes
            .as_ref()
            .and_then(Value::as_i64)
            .unwrap_or_default();
        UploadRequest {
            field_path: text(&self.field_path),
            content_type: text(&self.content_type),
            size_bytes: size,
        }
    }
}

impl Validate for CreateFormFileUploadUrlDto {
    fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        // fieldPath: @IsString() @MaxLength(200)
        let field_path = self.field_path.as_ref().and_then(Value::as_str);
        if field_path.is_none_or(|s| validator_js_length(s) > 200) {
            errors.push("fieldPath must be shorter than or equal to 200 characters".to_owned());
        }
        if field_path.is_none() {
            errors.push("fieldPath must be a string".to_owned());
        }
        // contentType: @IsString() @MaxLength(128) @Matches(MEDIA_TYPE_PATTERN)
        let content_type = self.content_type.as_ref().and_then(Value::as_str);
        if !content_type.is_some_and(is_media_type) {
            errors.push("contentType must be a media type".to_owned());
        }
        if content_type.is_none_or(|s| validator_js_length(s) > 128) {
            errors.push("contentType must be shorter than or equal to 128 characters".to_owned());
        }
        if content_type.is_none() {
            errors.push("contentType must be a string".to_owned());
        }
        // sizeBytes: @IsInt() @Min(1) @Max(MAX_FILE_BYTES)
        let size = self.size_bytes.as_ref().and_then(Value::as_f64);
        check_int_range(&mut errors, "sizeBytes", size, 1, MAX_FILE_BYTES);
        errors
    }
}

/// `ListOrderChoicesQueryDto` sesudah transform → `(search, limit)`.
pub fn parse_order_choices_query(raw: &str) -> ApiResult<(Option<String>, i64)> {
    let q = mz_core::qs::parse(raw);
    let mut errors = Vec::new();
    // search: @IsOptional() @IsString() @MaxLength(120)
    let search = mz_core::qs::get(&q, "search");
    if let Some(v) = search {
        let s = v.as_str();
        if s.is_none_or(|s| validator_js_length(s) > 120) {
            errors.push("search must be shorter than or equal to 120 characters".to_owned());
        }
        if s.is_none() {
            errors.push("search must be a string".to_owned());
        }
    }
    // limit: @Transform(v !== undefined ? parseInt(v, 10) : 20) @IsInt() @Min(1) @Max(100) @IsOptional()
    let limit = match mz_core::qs::get(&q, "limit") {
        None => Some(20),
        Some(v) => {
            #[allow(clippy::cast_precision_loss)]
            let num = mz_core::jsfmt::parse_int(&v.js_string()).map(|n| n as f64);
            check_int_range(&mut errors, "limit", num, 1, 100)
        }
    };
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    Ok((
        search.and_then(|v| v.as_str()).map(str::to_owned),
        limit.unwrap_or(20),
    ))
}

// ── Handler ──────────────────────────────────────────────────────────────────

fn data(v: Value) -> Json<Value> {
    Json(json!({ "data": v }))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:128 `listForms` (tanpa bungkus tambahan).
async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(visit_uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        service::list_forms(&state, user_id, &visit_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:179 `orderChoices`.
async fn h_order_choices(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(visit_uuid): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let (search, limit) = parse_order_choices_query(uri.query().unwrap_or_default())?;
    Ok(Json(
        service::list_order_choices(&state, user_id, &visit_uuid, search.as_deref(), limit).await?,
    ))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:261 `getForm`.
async fn h_get_form(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, template_uuid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(data(
        service::get_form(&state, user_id, &visit_uuid, &template_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:361 `saveDraft`.
async fn h_draft(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, template_uuid)): Path<(String, String)>,
    ValidatedJson(dto): ValidatedJson<SaveFormDraftDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let answers = dto.answers.unwrap_or_default();
    Ok(data(
        service::save_draft_response(&state, user_id, &visit_uuid, &template_uuid, &answers)
            .await?,
    ))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:455 `submit` (`@HttpCode(200)`).
async fn h_submit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, template_uuid)): Path<(String, String)>,
    ValidatedJson(dto): ValidatedJson<SubmitFormDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let answers = dto.answers.unwrap_or_default();
    let request_id = dto
        .client_request_id
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(data(
        service::submit_response(
            &state,
            user_id,
            &visit_uuid,
            &template_uuid,
            &answers,
            &request_id,
        )
        .await?,
    ))
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:540 `createFileUploadUrl` (201).
async fn h_file_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, template_uuid)): Path<(String, String)>,
    ValidatedJson(dto): ValidatedJson<CreateFormFileUploadUrlDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let v = service::create_file_upload_url(
        &state,
        user_id,
        &visit_uuid,
        &template_uuid,
        &dto.to_request(),
    )
    .await?;
    Ok((StatusCode::CREATED, data(v)).into_response())
}

/// Node: src/store-visit-forms/mobile-store-visit-forms.controller.ts:612 `confirmFile` (`@HttpCode(200)`).
async fn h_file_confirm(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, template_uuid, file_uuid)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(data(
        service::confirm_file(&state, user_id, &visit_uuid, &template_uuid, &file_uuid).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validation(e: ApiError) -> Vec<String> {
        match e {
            ApiError::Validation(v) => v,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn draft_dto() {
        let ok: SaveFormDraftDto = serde_json::from_value(json!({"answers": {}})).unwrap();
        assert!(ok.validate().is_empty());
        for bad in [
            json!({}),
            json!({"answers": null}),
            json!({"answers": []}),
            json!({"answers": "x"}),
        ] {
            let dto: SaveFormDraftDto = serde_json::from_value(bad).unwrap();
            assert_eq!(dto.validate(), vec!["answers must be an object"]);
        }
    }

    #[test]
    fn submit_dto() {
        let dto: SubmitFormDto = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            dto.validate(),
            vec![
                "answers must be an object",
                "clientRequestId must be a UUID"
            ]
        );
        let dto: SubmitFormDto = serde_json::from_value(
            json!({"answers": {}, "clientRequestId": "c0ffee00-1111-4222-8333-444455556666"}),
        )
        .unwrap();
        assert!(dto.validate().is_empty());
    }

    #[test]
    fn file_dto() {
        let dto: CreateFormFileUploadUrlDto = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            dto.validate(),
            vec![
                "fieldPath must be shorter than or equal to 200 characters",
                "fieldPath must be a string",
                "contentType must be a media type",
                "contentType must be shorter than or equal to 128 characters",
                "contentType must be a string",
                "sizeBytes must not be greater than 2097152",
                "sizeBytes must not be less than 1",
                "sizeBytes must be an integer number",
            ]
        );
        let dto: CreateFormFileUploadUrlDto = serde_json::from_value(
            json!({"fieldPath": "ttd", "contentType": "image/png", "sizeBytes": 2_097_153}),
        )
        .unwrap();
        assert_eq!(
            dto.validate(),
            vec!["sizeBytes must not be greater than 2097152"]
        );
        let dto: CreateFormFileUploadUrlDto = serde_json::from_value(
            json!({"fieldPath": "ttd", "contentType": "image/PNG", "sizeBytes": 10}),
        )
        .unwrap();
        assert!(dto.validate().is_empty());
        assert_eq!(dto.to_request().size_bytes, 10);
        assert!(!is_media_type("image"));
        assert!(!is_media_type("image/png/x"));
        assert!(!is_media_type("a b/c"));
    }

    // spec: listOrderChoices — limit diteruskan sebagai take, default 20
    #[test]
    fn order_query() {
        assert_eq!(parse_order_choices_query("").unwrap(), (None, 20));
        assert_eq!(
            parse_order_choices_query("search=APR&limit=5").unwrap(),
            (Some("APR".into()), 5)
        );
        assert_eq!(parse_order_choices_query("limit=7abc").unwrap().1, 7);
        assert_eq!(
            validation(parse_order_choices_query("limit=0").unwrap_err()),
            vec!["limit must not be less than 1"]
        );
        assert_eq!(
            validation(parse_order_choices_query("limit=abc").unwrap_err()),
            vec![
                "limit must not be greater than 100",
                "limit must not be less than 1",
                "limit must be an integer number"
            ]
        );
        assert_eq!(
            validation(parse_order_choices_query("search=a&search=b").unwrap_err()),
            vec![
                "search must be shorter than or equal to 120 characters",
                "search must be a string"
            ]
        );
    }
}
