//! Regresi: ekspor CSV streaming lewat router lengkap (termasuk `CompressionLayer`).
//!
//! `CompressionLayer` bisa mem-poll body lagi sesudah stream selesai; `futures::stream::unfold`
//! panic bila itu terjadi ("Unfold must not be polled after it returned `Poll::Ready(None)`"),
//! sehingga browser (selalu mengirim `Accept-Encoding: gzip, br`) menerima koneksi terputus.
//! Uji ini memastikan semua ekspor CSV streaming selesai utuh saat dikompresi.
//!
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_export_compression`

use mz_testkit::TestApp;

#[tokio::test]
async fn csv_exports_survive_compression() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // stores & store-visits berbagi kuota ekspor company: satu request masing-masing.
    for (uri, encoding, head) in [
        ("/v1/admin/stores/export", "gzip", "id,code,name"),
        ("/v1/admin/store-visits/export", "br", "id,visitNumber"),
        (
            "/v1/admin/orders/export?format=csv",
            "gzip",
            "id,orderNumber",
        ),
        (
            "/v1/admin/orders/export?format=csv&level=item",
            "br",
            "orderNumber,status",
        ),
    ] {
        let r = app
            .get(uri)
            .token(&fx.super_admin.token)
            .header("accept-encoding", encoding)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(
            r.headers
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some(encoding),
            "{uri}: respons tidak dikompresi"
        );
        let plain = decompress(encoding, &r.bytes);
        let text = String::from_utf8_lossy(&plain);
        assert!(
            text.trim_start_matches('\u{feff}').starts_with(head),
            "{uri} ({encoding}): {text}"
        );
    }
}

fn decompress(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    let read = if encoding == "gzip" {
        flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)
    } else {
        brotli_decompressor::Decompressor::new(bytes, 4096).read_to_end(&mut out)
    };
    assert!(read.is_ok(), "{encoding} rusak: {read:?}");
    out
}
