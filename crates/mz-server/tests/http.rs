//! Uji kontrak HTTP yang tidak menyentuh DB/Redis: bentuk error Nest, CORS,
//! header keamanan, 404.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let state = mz_core::AppState::for_test().unwrap_or_else(|e| unreachable!("{e}"));
    let app = mz_server::app(&state);
    let res = app.oneshot(req).await.unwrap_or_else(|e| match e {});
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap_or_default()
}

#[tokio::test]
async fn missing_token_is_bare_401_like_passport() {
    for uri in [
        "/trackings",
        "/projects",
        "/projects/abc",
        "/users",
        "/auth/me",
        "/tracking-timelines/00000000-0000-4000-8000-000000000000",
        "/activity-logs",
        "/timesheets",
        "/project-types",
    ] {
        let (status, _, body) = call(get(uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            body,
            json!({ "message": "Unauthorized", "statusCode": 401 }),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn bad_signature_is_bare_401() {
    let req = Request::get("/trackings")
        .header(
            "authorization",
            "Bearer eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOjF9.x",
        )
        .body(Body::empty())
        .unwrap_or_default();
    let (status, _, body) = call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({ "message": "Unauthorized", "statusCode": 401 })
    );
}

#[tokio::test]
async fn token_with_oauth_type_is_session_invalid() {
    // State OAuth (`type`) ditandatangani secret yang sama tapi tidak boleh jadi login.
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &json!({ "userId": 1, "type": "google-oauth-state", "iat": 1 }),
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
    )
    .unwrap_or_default();
    let req = Request::get("/trackings")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap_or_default();
    let (status, _, body) = call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({ "status": false, "message": "Your session is no longer valid. Please login again." })
    );
}

#[tokio::test]
async fn unknown_route_is_nest_404() {
    let (status, _, body) = call(get("/does-not-exist")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "message": "Cannot GET /does-not-exist", "error": "Not Found", "statusCode": 404 })
    );
}

#[tokio::test]
async fn trailing_slash_matches_like_express_non_strict() {
    // `/health/` = `/health` (satu garis miring akhir).
    let (status, _, _) = call(get("/health/")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(get("/health/?x=1")).await;
    assert_eq!(status, StatusCode::OK);
    // Dua garis miring tidak cocok; pesan 404 memakai path asli.
    let (status, _, body) = call(get("/health//")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.get("message"), Some(&json!("Cannot GET /health//")));
    let (_, _, body) = call(get("/does-not-exist/")).await;
    assert_eq!(
        body.get("message"),
        Some(&json!("Cannot GET /does-not-exist/"))
    );
}

#[tokio::test]
async fn json_content_type_has_charset_like_express() {
    let (_, headers, _) = call(get("/does-not-exist")).await;
    assert_eq!(
        headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"application/json; charset=utf-8"[..])
    );
}

#[tokio::test]
async fn wrong_method_is_nest_404_not_405() {
    let req = Request::put("/trackings")
        .body(Body::empty())
        .unwrap_or_default();
    let (status, _, body) = call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "message": "Cannot PUT /trackings", "error": "Not Found", "statusCode": 404 })
    );
}

#[tokio::test]
async fn health_and_security_headers() {
    let (status, headers, body) = call(get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("status"), Some(&json!("ok")));
    assert_eq!(
        headers.get("x-content-type-options").map(|v| v.as_bytes()),
        Some(&b"nosniff"[..])
    );
    assert_eq!(
        headers.get("x-frame-options").map(|v| v.as_bytes()),
        Some(&b"SAMEORIGIN"[..])
    );
}

#[tokio::test]
async fn cors_preflight_allowed_and_blocked() {
    let pre = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/trackings")
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .body(Body::empty())
            .unwrap_or_default()
    };
    let (status, headers, _) = call(pre("https://app.timemoz.com")).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "Nest optionsSuccessStatus: 204"
    );
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(|v| v.as_bytes()),
        Some(&b"https://app.timemoz.com"[..])
    );
    assert_eq!(
        headers
            .get("access-control-allow-credentials")
            .map(|v| v.as_bytes()),
        Some(&b"true"[..])
    );
    let (_, headers, _) = call(pre("https://evil.example")).await;
    assert!(headers.get("access-control-allow-origin").is_none());
}

/// Node: src/app.controller.ts:9 — `@Redirect('api', 301)` (Express `res.redirect`).
#[tokio::test]
async fn root_redirects_to_api_like_express() {
    let state = mz_core::AppState::for_test().unwrap_or_else(|e| unreachable!("{e}"));
    for (accept, ctype, body) in [
        (
            None,
            Some("text/plain; charset=utf-8"),
            "Moved Permanently. Redirecting to api",
        ),
        (
            Some("text/html,*/*;q=0.8"),
            Some("text/html; charset=utf-8"),
            "<p>Moved Permanently. Redirecting to api</p>",
        ),
        (Some("application/json"), None, ""),
    ] {
        let mut req = Request::get("/");
        if let Some(a) = accept {
            req = req.header("accept", a);
        }
        let req = req.body(Body::empty()).unwrap_or_default();
        let res = mz_server::app(&state)
            .oneshot(req)
            .await
            .unwrap_or_else(|e| match e {});
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY, "{accept:?}");
        let h = res.headers().clone();
        assert_eq!(h.get("location").and_then(|v| v.to_str().ok()), Some("api"));
        assert_eq!(h.get("vary").and_then(|v| v.to_str().ok()), Some("Accept"));
        assert_eq!(
            h.get("content-type").and_then(|v| v.to_str().ok()),
            ctype,
            "{accept:?}"
        );
        let bytes = res
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        assert_eq!(String::from_utf8_lossy(&bytes), body, "{accept:?}");
    }
}
