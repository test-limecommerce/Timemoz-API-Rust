//! Port `products/safe-image-fetch.ts`: unduh gambar produk dari URL kolom Excel dengan SSRF
//! guard. IP diperiksa di resolver DNS reqwest (IP yang diperiksa = IP yang dipakai koneksi,
//! tanpa TOCTOU DNS rebinding), IP literal diperiksa sebelum request, redirect diikuti manual
//! dan diperiksa ulang tiap hop. Proksi sistem dimatikan.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{StatusCode, Url};

use super::columns::{IMAGE_DOWNLOAD_MAX_REDIRECTS, IMAGE_DOWNLOAD_TIMEOUT};
use crate::util::{MAX_PRODUCT_IMAGE_BYTES, is_blocked_ip};

/// `ImageDownloadError` (`code` masuk `warnings` job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDownloadError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ImageDownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ImageDownloadError {}

fn err(code: &'static str, message: impl Into<String>) -> ImageDownloadError {
    ImageDownloadError {
        code,
        message: message.into(),
    }
}

/// `DownloadedImage`.
#[derive(Debug, Clone)]
pub struct DownloadedImage {
    pub buffer: Vec<u8>,
    pub content_type: String,
}

/// Penolak IP; produksi SELALU [`is_blocked_ip`] (titik injeksi hanya untuk uji).
pub type BlockFn = fn(&str) -> bool;

const ALLOWED_CONTENT_TYPES: &[&str] = &["image/jpeg", "image/png", "image/webp"];

/// Resolver `dns.lookup` + penolakan alamat terblokir (alamat pertama, seperti Node).
struct GuardResolver {
    is_blocked: BlockFn,
}

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let is_blocked = self.is_blocked;
        Box::pin(async move {
            let first: Option<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.next();
            let Some(addr) = first else {
                return Err(format!("getaddrinfo ENOTFOUND {host}").into());
            };
            let ip = addr.ip().to_string();
            if is_blocked(&ip) {
                return Err(Box::new(err(
                    "SSRF_BLOCKED_ADDRESS",
                    format!("Refusing to connect to {ip}"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Addrs = Box::new(std::iter::once(addr));
            Ok(addrs)
        })
    }
}

fn assert_http_url(raw: &str) -> Result<Url, ImageDownloadError> {
    let url =
        Url::parse(raw).map_err(|_| err("IMAGE_URL_INVALID", format!("Not a valid URL: {raw}")))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(err(
            "IMAGE_URL_SCHEME_REJECTED",
            "Only http/https URLs are allowed",
        ));
    }
    Ok(url)
}

/// `assertLiteralIpAllowed`: IP literal tidak pernah lewat resolver.
fn assert_literal_ip_allowed(url: &Url, is_blocked: BlockFn) -> Result<(), ImageDownloadError> {
    let ip = match url.host() {
        Some(url::Host::Ipv4(v4)) => v4.to_string(),
        Some(url::Host::Ipv6(v6)) => v6.to_string(),
        _ => return Ok(()),
    };
    if is_blocked(&ip) {
        return Err(err(
            "SSRF_BLOCKED_ADDRESS",
            format!("Refusing to connect to {ip}"),
        ));
    }
    Ok(())
}

/// Galat reqwest → kode Node (resolver terblokir, timeout, lainnya).
fn map_reqwest(e: &reqwest::Error) -> ImageDownloadError {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = src {
        if let Some(d) = s.downcast_ref::<ImageDownloadError>() {
            return d.clone();
        }
        src = s.source();
    }
    if e.is_timeout() {
        return err("IMAGE_DOWNLOAD_TIMEOUT", "Download timed out");
    }
    err("IMAGE_DOWNLOAD_FAILED", e.to_string())
}

fn client(is_blocked: BlockFn) -> Result<reqwest::Client, ImageDownloadError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(IMAGE_DOWNLOAD_TIMEOUT)
        .read_timeout(IMAGE_DOWNLOAD_TIMEOUT)
        .user_agent("mudah-product-import/1.0")
        .dns_resolver(Arc::new(GuardResolver { is_blocked }))
        .build()
        .map_err(|e| err("IMAGE_DOWNLOAD_FAILED", e.to_string()))
}

enum Hop {
    Redirect(String),
    Body(Vec<u8>, String),
}

async fn fetch_one_hop(
    http: &reqwest::Client,
    url: &Url,
    is_blocked: BlockFn,
) -> Result<Hop, ImageDownloadError> {
    assert_literal_ip_allowed(url, is_blocked)?;
    let mut resp = http
        .get(url.clone())
        .send()
        .await
        .map_err(|e| map_reqwest(&e))?;
    let status = resp.status();
    if status.is_redirection()
        && let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
    {
        return Ok(Hop::Redirect(loc.to_owned()));
    }
    if status != StatusCode::OK {
        return Err(err(
            "IMAGE_DOWNLOAD_FAILED",
            format!("Unexpected HTTP status {}", status.as_u16()),
        ));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| map_reqwest(&e))? {
        if (buf.len() + chunk.len()) as u64 > MAX_PRODUCT_IMAGE_BYTES {
            return Err(err(
                "IMAGE_TOO_LARGE",
                format!("Image exceeds the {MAX_PRODUCT_IMAGE_BYTES} byte limit"),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Hop::Body(buf, content_type))
}

/// `downloadImageSafely(rawUrl)` dengan guard produksi.
pub async fn download_image_safely(raw_url: &str) -> Result<DownloadedImage, ImageDownloadError> {
    download_image_safely_with(raw_url, is_blocked_ip).await
}

/// Varian dengan penolak IP yang bisa disuntik (HANYA uji).
pub async fn download_image_safely_with(
    raw_url: &str,
    is_blocked: BlockFn,
) -> Result<DownloadedImage, ImageDownloadError> {
    let http = client(is_blocked)?;
    let mut current = assert_http_url(raw_url)?;
    for _ in 0..=IMAGE_DOWNLOAD_MAX_REDIRECTS {
        match fetch_one_hop(&http, &current, is_blocked).await? {
            Hop::Redirect(to) => {
                let next = current
                    .join(&to)
                    .map_err(|_| err("IMAGE_URL_INVALID", format!("Not a valid URL: {to}")))?;
                current = assert_http_url(next.as_str())?;
            }
            Hop::Body(buffer, content_type) => {
                if content_type.is_empty() {
                    return Err(err("IMAGE_DOWNLOAD_FAILED", "Download produced no data"));
                }
                if !ALLOWED_CONTENT_TYPES.contains(&content_type.as_str()) {
                    return Err(err(
                        "IMAGE_TYPE_REJECTED",
                        format!(
                            "Content-Type {content_type} is not one of image/jpeg, image/png, image/webp"
                        ),
                    ));
                }
                return Ok(DownloadedImage {
                    buffer,
                    content_type,
                });
            }
        }
    }
    Err(err("IMAGE_TOO_MANY_REDIRECTS", "Too many redirects"))
}

/// `allowLoopbackOnly` uji: hanya `127.0.0.1` yang dilepas, sisanya guard asli.
pub fn allow_loopback_only(ip: &str) -> bool {
    ip != "127.0.0.1" && is_blocked_ip(ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode as S, Uri};
    use axum::response::{IntoResponse, Response};

    async fn listen<F, Fut>(handler: F) -> u16
    where
        F: Fn(Uri, u16) -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(move |uri: Uri, _h: HeaderMap| {
            let h = handler.clone();
            async move { h(uri, port).await }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    #[tokio::test]
    async fn skema_bukan_http() {
        let e = download_image_safely("file:///etc/passwd")
            .await
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_URL_SCHEME_REJECTED");
        assert_eq!(
            download_image_safely("::nope").await.unwrap_err().code,
            "IMAGE_URL_INVALID"
        );
    }

    #[tokio::test]
    async fn loopback_ditolak_tanpa_menyentuh_server() {
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = hit.clone();
        let port = listen(move |_, _| {
            let h = h.clone();
            async move {
                h.store(true, std::sync::atomic::Ordering::SeqCst);
                (S::OK, [("content-type", "image/png")], "x").into_response()
            }
        })
        .await;
        let e = download_image_safely(&format!("http://127.0.0.1:{port}/img.png"))
            .await
            .unwrap_err();
        assert_eq!(e.code, "SSRF_BLOCKED_ADDRESS");
        // Nama host yang di-resolve ke loopback juga ditolak (jalur resolver).
        let e = download_image_safely(&format!("http://localhost:{port}/img.png"))
            .await
            .unwrap_err();
        assert_eq!(e.code, "SSRF_BLOCKED_ADDRESS");
        assert!(!hit.load(std::sync::atomic::Ordering::SeqCst));
        let e = download_image_safely("http://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert_eq!(e.code, "SSRF_BLOCKED_ADDRESS");
    }

    #[tokio::test]
    async fn redirect_ke_alamat_terblokir() {
        let port = listen(|_, _| async {
            (S::FOUND, [("location", "http://169.254.169.254/evil.png")]).into_response()
        })
        .await;
        let e =
            download_image_safely_with(&format!("http://127.0.0.1:{port}/r"), allow_loopback_only)
                .await
                .unwrap_err();
        assert_eq!(e.code, "SSRF_BLOCKED_ADDRESS");
    }

    #[tokio::test]
    async fn redirect_terlalu_banyak() {
        let port = listen(|uri: Uri, port| async move {
            (
                S::FOUND,
                [("location", format!("http://127.0.0.1:{port}{}", uri.path()))],
            )
                .into_response()
        })
        .await;
        let e = download_image_safely_with(
            &format!("http://127.0.0.1:{port}/loop"),
            allow_loopback_only,
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_TOO_MANY_REDIRECTS");
    }

    #[tokio::test]
    async fn unduh_dan_redirect_sah() {
        let port = listen(|uri: Uri, _| async move {
            match uri.path() {
                "/start" => (S::FOUND, [("location", "/final.webp")]).into_response(),
                "/final.webp" => {
                    (S::OK, [("content-type", "image/webp")], "webp-bytes").into_response()
                }
                "/img.png" => (
                    S::OK,
                    [("content-type", "image/png; charset=binary")],
                    "fake-png-bytes",
                )
                    .into_response(),
                "/text" => (S::OK, [("content-type", "text/html")], "<html>").into_response(),
                "/big" => (
                    S::OK,
                    [("content-type", "image/png")],
                    vec![0u8; MAX_PRODUCT_IMAGE_BYTES as usize + 1],
                )
                    .into_response(),
                _ => S::NOT_FOUND.into_response(),
            }
        })
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let ok = download_image_safely_with(&format!("{base}/img.png"), allow_loopback_only)
            .await
            .unwrap();
        assert_eq!(ok.content_type, "image/png");
        assert_eq!(ok.buffer, b"fake-png-bytes");
        let ok = download_image_safely_with(&format!("{base}/start"), allow_loopback_only)
            .await
            .unwrap();
        assert_eq!(ok.content_type, "image/webp");
        assert_eq!(ok.buffer, b"webp-bytes");
        let e = download_image_safely_with(&format!("{base}/text"), allow_loopback_only)
            .await
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_TYPE_REJECTED");
        let e = download_image_safely_with(&format!("{base}/big"), allow_loopback_only)
            .await
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_TOO_LARGE");
        let e = download_image_safely_with(&format!("{base}/missing"), allow_loopback_only)
            .await
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_DOWNLOAD_FAILED");
        assert_eq!(e.message, "Unexpected HTTP status 404");
    }
}
