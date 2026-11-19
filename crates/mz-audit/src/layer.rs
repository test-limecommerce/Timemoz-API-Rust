//! `RequestContextMiddleware` (`forRoutes('*')`): satu scope audit per request, buffer
//! ditulis SATU `INSERT` multi-baris SESUDAH respons dibuat (Node: `res.on('finish')`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::ConnectInfo;
use futures::future::BoxFuture;
use http::Request;
use tower::{Layer, Service};

use crate::handle::{Scope, ScopeExt, is_enabled};
use crate::sink;

/// Pasang di router teratas (lihat README).
pub fn layer() -> AuditLayer {
    AuditLayer
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AuditLayer;

impl<S> Layer<S> for AuditLayer {
    type Service = AuditService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AuditService { inner }
    }
}

#[derive(Debug, Clone)]
pub struct AuditService<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for AuditService<S>
where
    S: Service<Request<B>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<S::Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        if !is_enabled() {
            return Box::pin(inner.call(req));
        }
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        let scope = Arc::new(Scope::for_request(
            req.method(),
            req.uri(),
            req.headers(),
            peer,
        ));
        req.extensions_mut().insert(ScopeExt(scope.clone()));
        Box::pin(async move {
            let res = inner.call(req).await;
            // Tanpa syarat status respons (§0.24): isi buffer = tulisan yang berhasil.
            flush(&scope);
            res
        })
    }
}

/// `flushStore` — di latar belakang, supaya respons tidak menunggu.
fn flush(scope: &Scope) {
    let entries = scope.take_for_flush();
    if entries.is_empty() {
        return;
    }
    // Pool dipasang extractor `AuditHandle`; tanpa handle tidak ada entri.
    let Some(pool) = scope.pool.get().cloned() else {
        return;
    };
    let ctx = scope.write_context();
    mz_core::bg::spawn(async move {
        if let Err(err) = sink::write(&pool, &entries, &ctx).await {
            tracing::warn!("Gagal menulis {} baris audit: {err}", entries.len());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    #[tokio::test]
    async fn layer_installs_scope_and_passes_response_through() {
        async fn handler(req: axum::extract::Request) -> String {
            req.extensions()
                .get::<ScopeExt>()
                .map_or("none".into(), |s| {
                    s.0.write_context().route.unwrap_or_default()
                })
        }
        let app = Router::new().route("/x/{id}", get(handler)).layer(layer());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/x/1?q=2")
                    .body(Body::empty())
                    .unwrap_or_default(),
            )
            .await;
        let Ok(res) = res;
        let body = axum::body::to_bytes(res.into_body(), 1024)
            .await
            .unwrap_or_default();
        let expected: &[u8] = if is_enabled() { b"GET /x/1" } else { b"none" };
        assert_eq!(&body[..], expected);
    }
}
