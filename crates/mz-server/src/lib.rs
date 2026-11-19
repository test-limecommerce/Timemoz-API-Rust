//! `mudah-rs`: merakit router semua crate domain + layer global yang di
//! Node dipasang di `main.ts` (CORS, helmet, kompresi, batas body).

pub mod fdlimit;
pub mod health;
pub mod jobs;

use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method};
use axum::routing::get;
use mz_core::{ApiError, AppState};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Daftar origin `main.ts` (CORS). `file://` dikirim browser sebagai `null`
/// tidak termasuk — Node mencocokkan string `file://` apa adanya.
const ALLOWED_ORIGINS: &[&str] = &[
    "http://localhost:3001",
    "http://localhost:3100",
    "http://172.18.13.202:3001",
    "http://localhost:3003",
    "https://landingmudah.netlify.app",
    "https://mudah-landing.netlify.app",
    "https://staging.mudah.co.id",
    "https://staging-lp.mudah.co.id",
    "https://app.mudah.co.id",
    "https://app.timemoz.com",
    "https://timemoz.com",
    "https://af0eb407-1720-4ae1-aadf-700c7b3f6106.lovableproject.com",
    "file://",
];

pub fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    let file = tracing_appender::rolling::daily(
        std::env::var("RS_LOG_DIR").unwrap_or_else(|_| "logs".into()),
        "mudah-rs.log",
    );
    let (file_writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .init();
    guard
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            origin.to_str().is_ok_and(|o| ALLOWED_ORIGINS.contains(&o))
        }))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::PATCH,
            Method::POST,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
        .expose_headers([HeaderName::from_static("content-disposition")])
        .allow_credentials(true)
        .max_age(Duration::from_secs(0))
}

/// Header `helmet()` dengan konfigurasi `main.ts` (CORP & COEP mati, CSP tanpa
/// `upgrade-insecure-requests`).
fn security_headers(router: Router) -> Router {
    const CSP: &str = "default-src 'self';base-uri 'self';font-src 'self' https: data:;\
form-action 'self';frame-ancestors 'self';img-src 'self' data:;object-src 'none';\
script-src 'self';script-src-attr 'none';style-src 'self' https: 'unsafe-inline'";
    let headers: [(&str, &str); 10] = [
        ("content-security-policy", CSP),
        ("cross-origin-opener-policy", "same-origin"),
        ("origin-agent-cluster", "?1"),
        ("referrer-policy", "no-referrer"),
        (
            "strict-transport-security",
            "max-age=31536000; includeSubDomains",
        ),
        ("x-content-type-options", "nosniff"),
        ("x-dns-prefetch-control", "off"),
        ("x-download-options", "noopen"),
        ("x-frame-options", "SAMEORIGIN"),
        ("x-permitted-cross-domain-policies", "none"),
    ];
    let mut router = router;
    for (name, value) in headers {
        router = router.layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        ));
    }
    router.layer(SetResponseHeaderLayer::overriding(
        HeaderName::from_static("x-xss-protection"),
        HeaderValue::from_static("0"),
    ))
}

/// Node: src/app.controller.ts:9 — `@Get() @Redirect('api', 301)` = `res.redirect(301, 'api')`.
async fn root_redirect(headers: axum::http::HeaderMap) -> axum::response::Response {
    mz_core::redirect::express_redirect(&headers, axum::http::StatusCode::MOVED_PERMANENTLY, "api")
}

/// Express selalu mengirim `application/json; charset=utf-8`; axum hanya
/// `application/json`. Disamakan di satu tempat untuk semua route.
async fn json_charset(mut res: axum::response::Response) -> axum::response::Response {
    let ct = axum::http::header::CONTENT_TYPE;
    if res
        .headers()
        .get(&ct)
        .is_some_and(|v| v.as_bytes() == b"application/json")
    {
        res.headers_mut().insert(
            ct,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    res
}

/// Nest `enableCors({ optionsSuccessStatus: 204 })`: preflight dijawab 204, sedangkan
/// `tower_http::cors` menjawab 200.
async fn preflight_204(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let preflight = req.method() == Method::OPTIONS
        && req.headers().contains_key(axum::http::header::ORIGIN)
        && req
            .headers()
            .contains_key(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut res = next.run(req).await;
    if preflight && res.status() == axum::http::StatusCode::OK {
        *res.status_mut() = axum::http::StatusCode::NO_CONTENT;
    }
    res
}

async fn not_found(
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    method: Method,
) -> ApiError {
    // Bentuk 404 Nest: `Cannot GET /x` (path asli, termasuk garis miring akhir).
    ApiError::not_found(format!("Cannot {method} {}", uri.path()))
}

/// Routing Express non-strict (`strict: false`): `/x/` cocok dengan route `/x` (path-to-regexp
/// `^/x/?$`, hanya SATU garis miring akhir). Path ditulis ulang sebelum router; URI asli disimpan
/// sebagai `OriginalUri` (axum tidak menimpanya) untuk pesan 404.
async fn trim_trailing_slash(mut req: axum::extract::Request) -> axum::extract::Request {
    let path = req.uri().path();
    if path.len() > 1 && path.ends_with('/') {
        let original = req.uri().clone();
        let trimmed = path.strip_suffix('/').unwrap_or(path);
        let pq = match original.query() {
            Some(q) => format!("{trimmed}?{q}"),
            None => trimmed.to_owned(),
        };
        let mut parts = original.clone().into_parts();
        if let Ok(pq) = pq.parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                req.extensions_mut()
                    .insert(axum::extract::OriginalUri(original));
                *req.uri_mut() = uri;
            }
        }
    }
    req
}

pub fn app(state: &AppState) -> Router {
    let api = Router::new()
        .route("/", get(root_redirect))
        .route("/health", get(health::health))
        .merge(mz_auth::router(state))
        .merge(mz_users::router(state))
        .merge(mz_projects::router(state))
        .merge(mz_trackings::router(state))
        .merge(mz_timelines::router(state))
        .merge(mz_screenshots::router(state))
        .merge(mz_timesheets::router(state))
        .merge(mz_timeconfig::router(state))
        .merge(mz_audit::router(state))
        .merge(mz_overtime::router(state))
        .merge(mz_int_clickup::router(state))
        .merge(mz_int_jira::router(state))
        .merge(mz_int_discord::router(state))
        .merge(mz_int_gcal::router(state))
        .merge(mz_integrations::router(state))
        .merge(mz_misc::router(state))
        .merge(mz_settings::router(state))
        .merge(mz_table_fields::router(state))
        .merge(mz_companies::router(state))
        .merge(mz_roles::router(state))
        .merge(mz_divisions::router(state))
        .merge(mz_admin::router(state))
        .merge(mz_admin_company::router(state))
        .merge(mz_admin_jobs::router(state))
        .merge(mz_admin_invoice::router(state))
        .merge(mz_admin_mcp::router(state))
        .merge(mz_admin_misc::router(state))
        .merge(mz_tasks_sync::router(state))
        .merge(mz_billing::router(state))
        .merge(mz_paypal_api::router(state))
        .merge(mz_billing_tasks::router(state))
        .merge(mz_invoices::router(state))
        .merge(mz_mcp_api::router(state))
        .merge(mz_attendance::router(state))
        .merge(mz_sales_geo::router(state))
        .merge(mz_leave_config::router(state))
        .merge(mz_leaves::router(state))
        .merge(mz_leave_holidays::router(state))
        .merge(mz_visit_forms::router(state))
        .merge(mz_customers::router(state))
        .merge(mz_products::router(state))
        .merge(mz_store_visits::router(state))
        .merge(mz_orders::router(state))
        .fallback(not_found)
        // Nest/Express tidak punya 405: method yang tidak terdaftar = 404 `Cannot PUT /x`.
        .method_not_allowed_fallback(not_found)
        .with_state(state.clone())
        // Buffer audit per request; di-flush SESUDAH respons (setara
        // `RequestContextMiddleware` + `AuditLogsService.flush` di Node).
        .layer(mz_audit::layer())
        // Batas body default express.json (100 kB). Route berbatas lain
        // (`/users/local-db` 50 MB, `/users/log` 10 MB) memasang
        // `DefaultBodyLimit` sendiri di crate domainnya.
        .layer(DefaultBodyLimit::max(100 * 1024));
    let api = Router::new()
        .fallback_service(api)
        .layer(axum::middleware::map_request(trim_trailing_slash));

    security_headers(api)
        .layer(axum::middleware::map_response(json_charset))
        .layer(CompressionLayer::new())
        .layer(cors())
        .layer(axum::middleware::from_fn(preflight_204))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
}
