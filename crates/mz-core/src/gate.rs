//! Satu middleware per route yang menjalankan rantai Nest dengan URUTAN SAMA:
//!
//! 1. rate limiter express (sebelum Nest)
//! 2. `JwtAuthGuard`
//! 3. `RolesGuard` (kalau `@Roles`)
//! 4. interceptor global sesuai urutan `imports` di `app.module.ts`:
//!    SubscriptionLock → FeatureModule → (PlanLimits) → Permission
//! 5. handler (validasi body/query terjadi di extractor = ValidationPipe)
//!
//! Pemakaian di crate domain:
//!
//! ```ignore
//! const LIST: RouteSpec = RouteSpec::jwt().feature("users");
//! Router::new().route("/users", guard(&state, LIST, get(list)))
//! ```

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use tokio::sync::OnceCell;

use crate::enums::{FeatureModule, PermissionAction, Role, action_for_method};
use crate::jwt::AuthUser;
use crate::permissions::{EffectiveAccess, Exempt};
use crate::plan::PlanFeature;
use crate::rate_limit::{self, Outcome, RateKey, RateRule};
use crate::{ApiError, ApiResult, AppState};

/// Metadata decorator Nest untuk satu route.
#[derive(Debug, Clone, Copy, Default)]
pub struct RouteSpec {
    /// `@UseGuards(JwtAuthGuard)`.
    pub auth: bool,
    /// `@Roles(...)` + `RolesGuard`.
    pub roles: Option<&'static [Role]>,
    /// `@RequireFeature(key)`.
    pub feature: Option<&'static str>,
    /// `@PermissionExempt(reason)`.
    pub exempt: Option<Exempt>,
    /// `@RequirePermission(action)`.
    pub action: Option<PermissionAction>,
    /// `@RequireModule(...)`.
    pub modules: &'static [FeatureModule],
    /// `@RequirePlanFeature(...)` (`PlanFeatureInterceptor`, sesudah modul, sebelum izin).
    pub plan_features: &'static [PlanFeature],
    /// `@AllowWhenSubscriptionLocked()`.
    pub allow_locked: bool,
    /// Limiter express yang dipasang di `main.ts` / `auth-rate-limits.ts`.
    pub rate_limits: &'static [RateRule],
    /// `@Header(name, value)`. Nest memasangnya SESUDAH guard lolos dan SEBELUM
    /// interceptor/handler, jadi ikut terkirim pada error interceptor/handler
    /// (mis. 404) tapi tidak pada 401/403 guard. Header yang dipasang handler sendiri
    /// (`res.set`) MENANG. Lihat [`apply_route_headers`].
    pub headers: &'static [(&'static str, &'static str)],
}

impl RouteSpec {
    pub const fn public() -> Self {
        Self {
            auth: false,
            roles: None,
            feature: None,
            exempt: None,
            action: None,
            modules: &[],
            plan_features: &[],
            allow_locked: false,
            rate_limits: &[],
            headers: &[],
        }
    }
    pub const fn jwt() -> Self {
        let mut s = Self::public();
        s.auth = true;
        s
    }
    pub const fn roles(mut self, roles: &'static [Role]) -> Self {
        self.roles = Some(roles);
        self
    }
    pub const fn feature(mut self, key: &'static str) -> Self {
        self.feature = Some(key);
        self
    }
    pub const fn exempt(mut self, reason: Exempt) -> Self {
        self.exempt = Some(reason);
        self
    }
    pub const fn action(mut self, action: PermissionAction) -> Self {
        self.action = Some(action);
        self
    }
    pub const fn modules(mut self, modules: &'static [FeatureModule]) -> Self {
        self.modules = modules;
        self
    }
    pub const fn plan_features(mut self, features: &'static [PlanFeature]) -> Self {
        self.plan_features = features;
        self
    }
    pub const fn allow_locked(mut self) -> Self {
        self.allow_locked = true;
        self
    }
    pub const fn headers(mut self, headers: &'static [(&'static str, &'static str)]) -> Self {
        self.headers = headers;
        self
    }
    pub const fn rate_limits(mut self, rules: &'static [RateRule]) -> Self {
        self.rate_limits = rules;
        self
    }
}

struct CtxInner {
    user: Option<AuthUser>,
    company: OnceCell<Option<i32>>,
    access: OnceLock<Option<Arc<EffectiveAccess>>>,
}

/// Konteks per request (`req.user`, memo company, `req.access`).
#[derive(Clone)]
pub struct RequestCtx(Arc<CtxInner>);

impl RequestCtx {
    fn new(user: Option<AuthUser>) -> Self {
        Self(Arc::new(CtxInner {
            user,
            company: OnceCell::new(),
            access: OnceLock::new(),
        }))
    }

    pub fn user(&self) -> Option<&AuthUser> {
        self.0.user.as_ref()
    }

    /// `req.user?.id`.
    pub fn user_id(&self) -> Option<i32> {
        self.0.user.as_ref().and_then(|u| u.id)
    }

    /// `req.user.id` di handler ber-JwtAuthGuard. Token operator tidak punya id;
    /// Prisma lalu melempar → 500 di Node, di sini 401 bare.
    pub fn require_user_id(&self) -> ApiResult<i32> {
        self.user_id()
            .ok_or(ApiError::Bare(StatusCode::UNAUTHORIZED))
    }

    pub fn require_user(&self) -> ApiResult<&AuthUser> {
        self.user().ok_or(ApiError::Bare(StatusCode::UNAUTHORIZED))
    }

    /// `resolveRequestCompanyId` — dihitung sekali per request; kegagalan
    /// query TIDAK di-memo.
    pub async fn company_id(&self, state: &AppState) -> ApiResult<Option<i32>> {
        let Some(user_id) = self.user_id() else {
            return Ok(None);
        };
        self.0
            .company
            .get_or_try_init(|| async { crate::company::find_company_id(&state.db, user_id).await })
            .await
            .copied()
            .map_err(ApiError::from)
    }

    /// `req.access` — `None` kalau gerbang permission tidak berjalan / tanpa
    /// penugasan aktif.
    pub fn access(&self) -> Option<&Arc<EffectiveAccess>> {
        self.0.access.get().and_then(Option::as_ref)
    }

    /// `req.access?.effectiveRoleCode`.
    pub fn effective_role_code(&self) -> Option<&str> {
        self.access().map(|a| a.effective_role_code.as_str())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestCtx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<RequestCtx>()
            .cloned()
            .unwrap_or_else(|| RequestCtx::new(None)))
    }
}

/// Pasang gerbang ke satu `MethodRouter`.
pub fn guard(
    state: &AppState,
    spec: RouteSpec,
    route: MethodRouter<AppState>,
) -> MethodRouter<AppState> {
    route.layer(from_fn_with_state((state.clone(), spec), gate))
}

/// `@Header` Nest 10 dipasang SEBELUM handler; di sini sesudahnya, jadi urutannya dibalik:
/// - header yang sudah ada di respons (dipasang handler, padanan `res.set`) dipertahankan;
/// - `Content-Type` `application/json*` (bawaan `Json`/`ApiError`) diganti nilai `@Header`
///   hanya bila status < 400 — `res.json()` Express tidak menimpa Content-Type yang sudah
///   dipasang, sedangkan untuk body error (`statusCode >= 400`) `ExpressAdapter.reply`
///   mengembalikannya ke `application/json`. Header lain (mis. `Content-Disposition`) tetap ikut.
fn apply_route_headers(res: &mut Response, headers: &[(&str, &str)]) {
    let is_error = res.status().as_u16() >= 400;
    for (name, value) in headers {
        let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) else {
            continue;
        };
        let replace = match res.headers().get(&n) {
            None => true,
            Some(cur) if n == http::header::CONTENT_TYPE => {
                !is_error && cur.as_bytes().starts_with(b"application/json")
            }
            Some(_) => false,
        };
        if replace {
            res.headers_mut().insert(n, v);
        }
    }
}

const BODY_LIMIT_FOR_RATE_KEY: usize = 100 * 1024;

async fn rate_limit_key(state: &AppState, rule: &RateRule, req: &mut Request) -> Option<String> {
    let _ = state;
    match rule.key {
        RateKey::Ip => {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0);
            rate_limit::client_ip(req.headers(), peer)
        }
        RateKey::Token => crate::jwt::extract_bearer(req.headers()).map(rate_limit::hash32),
        RateKey::Email => {
            // Body perlu dibaca untuk kuncinya, lalu dipasang kembali utuh.
            let body = std::mem::replace(req.body_mut(), Body::empty());
            let bytes = axum::body::to_bytes(body, BODY_LIMIT_FOR_RATE_KEY)
                .await
                .ok()?;
            let key = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("username")
                        .and_then(|u| u.as_str())
                        .map(str::to_owned)
                })
                .map(|u| u.trim().to_lowercase())
                .filter(|u| !u.is_empty())
                .map(|u| rate_limit::hash32(&u));
            *req.body_mut() = Body::from(bytes);
            key
        }
    }
}

/// `express.json({ limit: '100kb' })` dipasang SEBELUM semua limiter di `main.ts` Node: body
/// JSON yang ditolak body-parser (rusak / non-strict / terlalu besar) dijawab 400/413 dan TIDAK
/// dihitung limiter (paritas adm-core: 3 login ber-JSON rusak tidak memakan kuota
/// `rl:admin-auth-login`). `true` = body akan ditolak; body dipasang kembali utuh.
async fn rejected_by_express_json(req: &mut Request) -> bool {
    let is_json = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return false;
    }
    let too_large = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > BODY_LIMIT_FOR_RATE_KEY);
    if too_large {
        // Body tidak disentuh: extractor handler menjawab 413 seperti body-parser.
        return true;
    }
    let body = std::mem::replace(req.body_mut(), Body::empty());
    let Ok(bytes) = axum::body::to_bytes(body, BODY_LIMIT_FOR_RATE_KEY).await else {
        return true;
    };
    // body-parser `strict`: karakter non-spasi pertama harus `{` atau `[`; body kosong = `{}`.
    let rejected = !bytes.is_empty()
        && (!matches!(
            bytes
                .iter()
                .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r')),
            Some(b'{' | b'[')
        ) || serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err());
    *req.body_mut() = Body::from(bytes);
    rejected
}

async fn gate(
    State((state, spec)): State<(AppState, RouteSpec)>,
    mut req: Request,
    next: Next,
) -> Response {
    // 1. Rate limit (dilewati bila body-parser Node akan menolak body lebih dulu).
    let mut rl_headers = None;
    let skip_limits = !spec.rate_limits.is_empty() && rejected_by_express_json(&mut req).await;
    for rule in spec.rate_limits.iter().filter(|_| !skip_limits) {
        let key = rate_limit_key(&state, rule, &mut req).await;
        match rate_limit::check(&state, rule, key).await {
            Outcome::Pass(h) => {
                if h.is_some() {
                    rl_headers = h;
                }
            }
            Outcome::Limited(res) => return res,
        }
    }

    let mut after_guards = false;
    let mut res = match run_guards(&state, &spec, &mut req).await {
        Err(err) => err.into_response(),
        Ok(ctx) => {
            after_guards = true;
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            match run_interceptors(&state, &spec, &ctx, &method, &path).await {
                Ok(()) => next.run(req).await,
                Err(err) => err.into_response(),
            }
        }
    };
    if after_guards {
        apply_route_headers(&mut res, spec.headers);
    }
    if let Some(h) = rl_headers {
        h.apply(res.headers_mut());
    }
    res
}

/// `JwtAuthGuard` + `RolesGuard`.
async fn run_guards(
    state: &AppState,
    spec: &RouteSpec,
    req: &mut Request,
) -> ApiResult<RequestCtx> {
    // 2. JwtAuthGuard.
    let user = if spec.auth {
        let url = req
            .uri()
            .path_and_query()
            .map_or_else(|| req.uri().path().to_owned(), |p| p.to_string());
        Some(crate::jwt::authenticate(state, req.headers(), &url).await?)
    } else {
        None
    };
    let ctx = RequestCtx::new(user);
    req.extensions_mut().insert(ctx.clone());

    // 3. RolesGuard.
    if let Some(required) = spec.roles {
        let Some(user_id) = ctx.user_id() else {
            return Err(ApiError::unauthorized("User data not found"));
        };
        let company_id = ctx.company_id(state).await?;
        let snap = match company_id {
            Some(c) => Some(crate::roles::user_access(state, user_id, c).await?),
            None => None,
        };
        crate::roles::decide(required, company_id, snap.as_deref())?;
    }
    Ok(ctx)
}

/// Interceptor global, urutan `imports` di `app.module.ts`.
async fn run_interceptors(
    state: &AppState,
    spec: &RouteSpec,
    ctx: &RequestCtx,
    method: &http::Method,
    path: &str,
) -> ApiResult<()> {
    let Some(user_id) = ctx.user_id() else {
        // Tanpa user, semua interceptor global melewatkan.
        return Ok(());
    };

    // 4a. SubscriptionLockInterceptor.
    if !spec.allow_locked {
        let lock = crate::subscription_lock::get_state(state, user_id).await?;
        if lock.locked {
            return Err(crate::subscription_lock::locked_error(&lock));
        }
    }

    // 4b. FeatureModuleInterceptor.
    if !spec.modules.is_empty() {
        let enabled = crate::feature_modules::enabled_modules(state, user_id).await?;
        crate::feature_modules::decide(spec.modules, &enabled)?;
    }

    // 4c. PlanFeatureInterceptor.
    if !spec.plan_features.is_empty()
        && !crate::plan::has_any_feature(&state.db, user_id, spec.plan_features).await?
    {
        return Err(crate::plan::disabled_error(spec.plan_features));
    }

    // 4d. PermissionInterceptor.
    if spec.exempt == Some(Exempt::SeparateAuth) {
        return Ok(());
    }
    let company_id = ctx.company_id(state).await?;
    let access = match company_id {
        Some(c) => crate::permissions::effective_access(state, user_id, c).await?,
        None => None,
    };
    let _ = ctx.0.access.set(access.clone());
    if spec.exempt.is_some() {
        return Ok(());
    }
    let is_custom = match (&access, company_id) {
        (Some(a), _) => !a.is_system,
        (None, Some(_)) => false,
        (None, None) => crate::permissions::holds_custom_role(&state.db, user_id).await?,
    };
    let action = spec.action.or_else(|| action_for_method(method));
    if spec.feature.is_none() && is_custom {
        tracing::warn!(
            user_id,
            role = access.as_ref().map_or("unknown", |a| a.role_code.as_str()),
            method = %method,
            path = %path,
            "Route tanpa @RequireFeature dipanggil pemegang role custom"
        );
    }
    crate::permissions::decide(
        spec.feature,
        action,
        company_id,
        access.as_deref(),
        is_custom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    const HDR: &[(&str, &str)] = &[("cache-control", "private, max-age=300")];

    async fn not_found() -> ApiError {
        ApiError::not_found("x")
    }

    async fn status_and_header(spec: RouteSpec) -> (u16, Option<String>) {
        let state = AppState::for_test().unwrap_or_else(|e| unreachable!("{e}"));
        let app = Router::new()
            .route("/x", guard(&state, spec, get(not_found)))
            .with_state(state);
        let res = app
            .oneshot(
                axum::http::Request::get("/x")
                    .body(Body::empty())
                    .unwrap_or_default(),
            )
            .await
            .unwrap_or_else(|e| match e {});
        let h = res
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (res.status().as_u16(), h)
    }

    /// Nest: `@Header` dipasang sesudah guard → ikut pada error handler (404)…
    #[tokio::test]
    async fn header_applies_to_handler_errors_after_guards() {
        let (status, h) = status_and_header(RouteSpec::public().headers(HDR)).await;
        assert_eq!(status, 404);
        assert_eq!(h.as_deref(), Some("private, max-age=300"));
    }

    /// …tapi tidak pada 401 dari `JwtAuthGuard`.
    #[tokio::test]
    async fn header_absent_when_guard_rejects() {
        let (status, h) = status_and_header(RouteSpec::jwt().headers(HDR)).await;
        assert_eq!(status, 401);
        assert_eq!(h, None);
    }

    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    const EXCEL: &[(&str, &str)] = &[
        ("content-type", XLSX),
        ("content-disposition", "attachment; filename=report.xlsx"),
    ];

    fn res(status: u16, headers: &[(&'static str, &'static str)]) -> Response {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::from_u16(status).unwrap_or_default();
        for (n, v) in headers {
            r.headers_mut()
                .insert(*n, http::HeaderValue::from_static(v));
        }
        r
    }

    fn hdr<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
        r.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// `res.set` handler (export Excel: nama file dinamis) menimpa `@Header`.
    #[test]
    fn handler_headers_win_over_route_headers() {
        let mut r = res(
            200,
            &[
                ("content-type", XLSX),
                (
                    "content-disposition",
                    "attachment; filename=\"Report-a-to-b.xlsx\"",
                ),
            ],
        );
        apply_route_headers(&mut r, EXCEL);
        assert_eq!(
            hdr(&r, "content-disposition"),
            Some("attachment; filename=\"Report-a-to-b.xlsx\"")
        );
        assert_eq!(hdr(&r, "content-type"), Some(XLSX));
    }

    /// Error handler: `ExpressAdapter.reply` mengembalikan Content-Type ke JSON, header lain ikut.
    #[test]
    fn error_keeps_json_content_type_but_gets_other_headers() {
        let mut r = res(400, &[("content-type", "application/json")]);
        apply_route_headers(&mut r, EXCEL);
        assert_eq!(hdr(&r, "content-type"), Some("application/json"));
        assert_eq!(
            hdr(&r, "content-disposition"),
            Some("attachment; filename=report.xlsx")
        );
    }

    /// Sukses lewat `res.json()`: Content-Type `@Header` yang sudah terpasang tidak ditimpa.
    #[test]
    fn success_json_takes_route_content_type() {
        let mut r = res(200, &[("content-type", "application/json")]);
        apply_route_headers(&mut r, EXCEL);
        assert_eq!(hdr(&r, "content-type"), Some(XLSX));
    }

    fn req_with(ct: Option<&str>, body: &'static str) -> Request {
        let mut b = axum::http::Request::builder().method("POST").uri("/x");
        if let Some(ct) = ct {
            b = b.header("content-type", ct);
        }
        b.body(Body::from(body)).unwrap_or_default()
    }

    #[tokio::test]
    async fn express_json_rejection_matches_body_parser() {
        let json = Some("application/json; charset=utf-8");
        for (ct, body, want) in [
            (json, r#"{"username":"a"}"#, false),
            (json, "", false),
            (json, "  [1]", false),
            (json, "{bad", true),
            (json, r#""teks""#, true),
            (json, "   ", true),
            (Some("text/plain"), "{bad", false),
            (None, "{bad", false),
        ] {
            let mut r = req_with(ct, body);
            assert_eq!(
                rejected_by_express_json(&mut r).await,
                want,
                "{ct:?} {body:?}"
            );
            // body dipasang kembali utuh
            let back = axum::body::to_bytes(std::mem::take(r.body_mut()), 1024)
                .await
                .unwrap_or_default();
            assert_eq!(back, body.as_bytes());
        }
    }
}
