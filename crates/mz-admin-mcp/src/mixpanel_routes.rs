//! Route analitik Mixpanel: `/admin/mixpanel/*` (JwtAdminAuthGuard, 8 route) dan
//! `/admin-mcp/mixpanel/*` (AdminPatAuthGuard, 13 route) memakai layanan yang sama
//! ([`crate::analytics`]).

use axum::Json;
use axum::extract::State;
use mz_core::extract::{Validate, ValidatedQuery};
use mz_core::{ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, Operator, audit_after_success};
use serde::Deserialize;
use serde_json::Value;

use crate::analytics::{self, Range};
use crate::body::Body;
use crate::pat::AdminPat;
use crate::sync;

/// `@Query('from') from?, @Query('to') to?, @Query('unit') unit?`.
#[derive(Debug, Deserialize, Default)]
pub struct RangeQuery {
    from: Option<String>,
    to: Option<String>,
    unit: Option<String>,
}

impl Validate for RangeQuery {}

impl From<RangeQuery> for Range {
    fn from(q: RangeQuery) -> Self {
        Self {
            from: q.from,
            to: q.to,
            unit: q.unit,
        }
    }
}

macro_rules! endpoints {
    ($auth:ty, $( $name:ident => $svc:ident, $node:literal ),+ $(,)?) => {
        $(
            #[doc = concat!("Node: ", $node)]
            pub async fn $name(
                State(state): State<AppState>,
                _auth: $auth,
                ValidatedQuery(q): ValidatedQuery<RangeQuery>,
            ) -> ApiResult<Json<Value>> {
                analytics::$svc(&state, &Range::from(q)).await.map(Json)
            }
        )+
    };
}

pub mod admin {
    use super::*;

    endpoints!(Operator,
        h_overview => overview, "src/admin-mixpanel/admin-mixpanel.controller.ts:36",
        h_traffic => traffic, "src/admin-mixpanel/admin-mixpanel.controller.ts:49",
        h_pages => pages, "src/admin-mixpanel/admin-mixpanel.controller.ts:66",
        h_cta => cta, "src/admin-mixpanel/admin-mixpanel.controller.ts:78",
        h_funnel => funnel, "src/admin-mixpanel/admin-mixpanel.controller.ts:90",
        h_campaigns => campaigns, "src/admin-mixpanel/admin-mixpanel.controller.ts:102",
        h_registrations => registrations, "src/admin-mixpanel/admin-mixpanel.controller.ts:114",
    );

    /// Node: src/admin-mixpanel/admin-mixpanel.controller.ts:131 (`POST sync`, 201,
    /// `@OperatorAuditAction('mixpanel.sync', { targetType: 'system' })`)
    pub async fn h_sync(
        State(state): State<AppState>,
        op: Operator,
        ctx: AuditCtx,
        ValidatedQuery(q): ValidatedQuery<RangeQuery>,
        Body(body): Body,
    ) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
        let body_str = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
        let from = q.from.clone().or_else(|| body_str("from"));
        let to = q.to.clone().or_else(|| body_str("to"));
        let message = match (
            from.as_deref().filter(|s| !s.is_empty()),
            to.as_deref().filter(|s| !s.is_empty()),
        ) {
            (Some(f), Some(t)) => {
                sync::sync_range(&state, f, t)
                    .await
                    .map_err(mz_core::ApiError::internal)?;
                format!("Sync completed for range {f} – {t}")
            }
            _ => {
                let date = sync::yesterday();
                sync::sync_date(&state, &date)
                    .await
                    .map_err(mz_core::ApiError::internal)?;
                format!("Sync completed for {date}")
            }
        };
        let raw = Value::Object(body);
        audit_after_success(
            &state,
            &op,
            &ctx,
            AuditMeta {
                action: "mixpanel.sync",
                target_type: "system",
                company_from: None,
            },
            None,
            Some(&raw),
        )
        .await;
        Ok((
            axum::http::StatusCode::CREATED,
            Json(serde_json::json!({ "success": true, "message": message })),
        ))
    }
}

pub mod mcp {
    use super::*;

    endpoints!(AdminPat,
        h_overview => overview, "src/admin-mcp/mixpanel/mixpanel.controller.ts:31",
        h_traffic => traffic, "src/admin-mcp/mixpanel/mixpanel.controller.ts:44",
        h_pages => pages, "src/admin-mcp/mixpanel/mixpanel.controller.ts:62",
        h_cta => cta, "src/admin-mcp/mixpanel/mixpanel.controller.ts:74",
        h_funnel => funnel, "src/admin-mcp/mixpanel/mixpanel.controller.ts:86",
        h_campaigns => campaigns, "src/admin-mcp/mixpanel/mixpanel.controller.ts:98",
        h_registrations => registrations, "src/admin-mcp/mixpanel/mixpanel.controller.ts:110",
        h_geo => geo, "src/admin-mcp/mixpanel/mixpanel.controller.ts:128",
        h_devices => devices, "src/admin-mcp/mixpanel/mixpanel.controller.ts:140",
        h_hourly => hourly, "src/admin-mcp/mixpanel/mixpanel.controller.ts:152",
        h_day_of_week => day_of_week, "src/admin-mcp/mixpanel/mixpanel.controller.ts:164",
        h_new_vs_returning => new_vs_returning, "src/admin-mcp/mixpanel/mixpanel.controller.ts:176",
        h_comparison => comparison, "src/admin-mcp/mixpanel/mixpanel.controller.ts:188",
    );
}
