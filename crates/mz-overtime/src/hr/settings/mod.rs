//! Route HR lembur: settings (4) + status (1).
//!
//! - `overtime-settings.controller.ts`: `@RequireFeature('overtime-settings')`,
//!   `@RequireModule(TIME_TRACKING)`, `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)` (MANAGER
//!   SENGAJA tidak ada), `@SkipThrottle()` (tak ada limiter), tanpa override per method.
//! - `overtime-status.controller.ts`: `@RequireFeature('overtime')`,
//!   `@RequireModule(TIME_TRACKING)`, `@Roles(…, MANAGER, STAFF)`.
//!
//! Logika ada di `hr::common::settings_svc`.

use axum::extract::{RawQuery, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::enums::{FeatureModule, Role};
use mz_core::extract::{Validate, ValidatedJson};
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::common::dto::{
    QVal, as_js_integer, is_uuid_v4, js_trim, query_get, query_map, validate_optional_bool,
    validate_optional_int,
};
use super::common::settings_svc::{
    self, ApproverView, DisableImpactView, OVERTIME_SETTINGS_ROLES, OvertimeSettingsView,
    OvertimeStatusView, UpdateOvertimeSettingsInput,
};

const TIME_TRACKING: &[FeatureModule] = &[FeatureModule::TIME_TRACKING];

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)` (status; tanpa GUEST).
const STATUS_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

const SETTINGS: RouteSpec = RouteSpec::jwt()
    .roles(OVERTIME_SETTINGS_ROLES)
    .feature("overtime-settings")
    .modules(TIME_TRACKING);

const STATUS: RouteSpec = RouteSpec::jwt()
    .roles(STATUS_ROLES)
    .feature("overtime")
    .modules(TIME_TRACKING);

/// Route settings + status.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/v1/overtime/settings",
            guard(state, SETTINGS, get(h_get_settings)),
        )
        .route(
            "/v1/overtime/settings",
            guard(state, SETTINGS, put(h_update_settings)),
        )
        .route(
            "/v1/overtime/settings/approver-candidates",
            guard(state, SETTINGS, get(h_approver_candidates)),
        )
        .route(
            "/v1/overtime/settings/disable-impact",
            guard(state, SETTINGS, get(h_disable_impact)),
        )
        .route("/v1/overtime/status", guard(state, STATUS, get(h_status)))
}

// ── DTO ──────────────────────────────────────────────────────────────────────

/// Query `ApproverCandidatesQueryDto`: `@Transform(trim; hanya spasi → undefined)
/// @IsOptional() @IsString() @MaxLength(100) search`. Mengembalikan `search` ter-trim.
pub fn parse_candidates_query(raw: Option<&str>) -> Result<Option<String>, Vec<String>> {
    let map = query_map(raw);
    match query_get(&map, "search") {
        None => Ok(None),
        Some(QVal::One(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                return Ok(None);
            }
            if super::common::dto::validator_js_length(t) > 100 {
                return Err(vec![
                    "search must be shorter than or equal to 100 characters".to_owned(),
                ]);
            }
            Ok(Some(t.to_owned()))
        }
        Some(QVal::Many(_) | QVal::Obj) => Err(vec![
            "search must be shorter than or equal to 100 characters".to_owned(),
            "search must be a string".to_owned(),
        ]),
    }
}

/// Body `UpdateOvertimeSettingsDto` (semua field `@ValidateIf(v !== undefined)`: `null`
/// ditolak). Field tak dikenal diabaikan (tanpa `whitelist`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct UpdateSettingsBody(pub Map<String, Value>);

impl Validate for UpdateSettingsBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut e = Vec::new();
        // enabled: @IsBoolean
        validate_optional_bool(&mut e, "enabled", m.get("enabled"));
        // expectedCancelCount: @IsInt @Min(0) @Max(1_000_000)
        validate_optional_int(
            &mut e,
            "expectedCancelCount",
            m.get("expectedCancelCount"),
            0,
            1_000_000,
        );
        // retroWindowDays: @IsInt @Min(0) @Max(31)
        validate_optional_int(&mut e, "retroWindowDays", m.get("retroWindowDays"), 0, 31);
        // approverUserIds: @IsArray @ArrayMaxSize(50) @ArrayUnique @IsUUID('4', {each})
        if let Some(v) = m.get("approverUserIds") {
            let arr = v.as_array();
            let uuid_ok = match arr {
                Some(items) => items.iter().all(|i| i.as_str().is_some_and(is_uuid_v4)),
                None => v.as_str().is_some_and(is_uuid_v4),
            };
            if !uuid_ok {
                e.push("each value in approverUserIds must be a UUID".to_owned());
            }
            let unique = arr.is_some_and(|items| {
                items
                    .iter()
                    .enumerate()
                    .all(|(i, a)| items.iter().skip(i + 1).all(|b| a != b))
            });
            if !unique {
                e.push("All approverUserIds's elements must be unique".to_owned());
            }
            if !arr.is_some_and(|a| a.len() <= 50) {
                e.push("approverUserIds must contain no more than 50 elements".to_owned());
            }
            if arr.is_none() {
                e.push("approverUserIds must be an array".to_owned());
            }
        }
        e
    }
}

impl UpdateSettingsBody {
    /// Masukan service (dipanggil sesudah validasi lolos).
    pub fn to_input(&self) -> UpdateOvertimeSettingsInput {
        let m = &self.0;
        UpdateOvertimeSettingsInput {
            enabled: m.get("enabled").and_then(Value::as_bool),
            expected_cancel_count: m.get("expectedCancelCount").and_then(as_js_integer),
            retro_window_days: m.get("retroWindowDays").and_then(as_js_integer),
            approver_user_ids: m.get("approverUserIds").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        }
    }
}

// ── handler ──────────────────────────────────────────────────────────────────

/// `{ data, cancelled? }` — struct (bukan `json!`) supaya urutan kunci di kabel = Node.
#[derive(Debug, Serialize)]
pub struct DataResponse<T: Serialize> {
    pub data: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<u64>,
}

fn data<T: Serialize>(data: T) -> Json<DataResponse<T>> {
    Json(DataResponse {
        data,
        cancelled: None,
    })
}

/// Node: src/overtime/overtime-settings.controller.ts:57 (`getSettings`)
pub async fn h_get_settings(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<DataResponse<OvertimeSettingsView>>> {
    Ok(data(
        settings_svc::get_settings(&state, ctx.require_user_id()?).await?,
    ))
}

/// Node: src/overtime/overtime-settings.controller.ts:65 (`approverCandidates`)
pub async fn h_approver_candidates(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<DataResponse<Vec<ApproverView>>>> {
    let search = parse_candidates_query(raw.as_deref()).map_err(ApiError::Validation)?;
    let rows = settings_svc::approver_candidates(&state, ctx.require_user_id()?, search.as_deref())
        .await?;
    Ok(data(rows))
}

/// Node: src/overtime/overtime-settings.controller.ts:82 (`disableImpact`)
pub async fn h_disable_impact(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<DataResponse<DisableImpactView>>> {
    Ok(data(
        settings_svc::disable_impact(&state, ctx.require_user_id()?).await?,
    ))
}

/// Node: src/overtime/overtime-settings.controller.ts:105 (`updateSettings`). 200 (PUT);
/// `cancelled` hanya ada saat panggilan ini menonaktifkan fitur.
pub async fn h_update_settings(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<UpdateSettingsBody>,
) -> ApiResult<Json<DataResponse<OvertimeSettingsView>>> {
    let result =
        settings_svc::update_settings(&state, &audit, ctx.require_user_id()?, &body.to_input())
            .await?;
    Ok(Json(DataResponse {
        data: result.view,
        cancelled: result.cancelled,
    }))
}

/// Node: src/overtime/overtime-status.controller.ts:40 (`getStatus`)
pub async fn h_status(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<DataResponse<OvertimeStatusView>>> {
    Ok(data(
        settings_svc::get_status(&state, ctx.require_user_id()?).await?,
    ))
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-dto.http.spec.ts` untuk settings (tanpa HTTP/DB).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    const U1: &str = "5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02";
    const U2: &str = "9a1f3c52-6b7d-4e8a-8c4f-1d2e3f4a5b03";
    const UUID_V1: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    fn body(v: Value) -> UpdateSettingsBody {
        UpdateSettingsBody(v.as_object().cloned().unwrap_or_default())
    }

    fn ids(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| Value::String(format!("00000000-0000-4000-8000-{i:012}")))
                .collect(),
        )
    }

    #[test]
    fn candidates_query() {
        assert_eq!(parse_candidates_query(None), Ok(None));
        assert_eq!(
            parse_candidates_query(Some("search=%20%20sari%20%20")),
            Ok(Some("sari".into()))
        );
        assert_eq!(parse_candidates_query(Some("search=%20%20")), Ok(None));
        assert!(parse_candidates_query(Some(&format!("search={}", "a".repeat(100)))).is_ok());
        assert!(parse_candidates_query(Some(&format!("search={}", "a".repeat(101)))).is_err());
        assert_eq!(
            parse_candidates_query(Some("search=a&search=b"))
                .err()
                .map(|e| e.len()),
            Some(2)
        );
    }

    #[test]
    fn update_body_valid() {
        assert!(body(json!({})).validate().is_empty());
        assert_eq!(
            body(json!({})).to_input(),
            UpdateOvertimeSettingsInput::default()
        );
        let full = body(json!({
            "enabled": false, "expectedCancelCount": 5, "retroWindowDays": 7,
            "approverUserIds": [U1, U2]
        }));
        assert!(full.validate().is_empty());
        let input = full.to_input();
        assert_eq!(input.enabled, Some(false));
        assert_eq!(input.expected_cancel_count, Some(5));
        assert_eq!(input.retro_window_days, Some(7));
        assert_eq!(
            input.approver_user_ids,
            Some(vec![U1.to_owned(), U2.to_owned()])
        );
        assert!(body(json!({"approverUserIds": []})).validate().is_empty());
        for ok in [
            json!({"retroWindowDays": 0}),
            json!({"retroWindowDays": 31, "expectedCancelCount": 0}),
            json!({"approverUserIds": ids(50)}),
            json!({"expectedCancelCount": 1_000_000}),
            json!({"legalNote": "x"}),
        ] {
            assert!(body(ok.clone()).validate().is_empty(), "{ok}");
        }
    }

    #[test]
    fn update_body_invalid() {
        for bad in [
            json!({"enabled": "false"}),
            json!({"enabled": 0}),
            json!({"expectedCancelCount": -1}),
            json!({"expectedCancelCount": 1.5}),
            json!({"expectedCancelCount": "5"}),
            json!({"retroWindowDays": 32}),
            json!({"retroWindowDays": -1}),
            json!({"retroWindowDays": 3.5}),
            json!({"retroWindowDays": "7"}),
            json!({"approverUserIds": U1}),
            json!({"approverUserIds": ["abc"]}),
            json!({"approverUserIds": [UUID_V1]}),
            json!({"approverUserIds": ids(51)}),
            json!({"approverUserIds": [U1, U1]}),
            json!({"expectedCancelCount": 1_000_001}),
            json!({"enabled": null}),
            json!({"retroWindowDays": null}),
            json!({"expectedCancelCount": null}),
            json!({"approverUserIds": null}),
        ] {
            assert!(!body(bad.clone()).validate().is_empty(), "{bad}");
        }
        assert_eq!(
            body(json!({"retroWindowDays": "7"})).validate(),
            vec![
                "retroWindowDays must not be greater than 31",
                "retroWindowDays must not be less than 0",
                "retroWindowDays must be an integer number"
            ]
        );
        assert_eq!(
            body(json!({"approverUserIds": U1})).validate(),
            vec![
                "All approverUserIds's elements must be unique",
                "approverUserIds must contain no more than 50 elements",
                "approverUserIds must be an array"
            ]
        );
        assert_eq!(
            body(json!({"enabled": null, "approverUserIds": [U1, U1]})).validate(),
            vec![
                "enabled must be a boolean value",
                "All approverUserIds's elements must be unique"
            ]
        );
    }
}
