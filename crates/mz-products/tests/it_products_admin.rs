//! Uji integrasi route admin produk (master, custom field, produk, gambar, import) terhadap
//! Postgres uji + emulator GCS + server HTTP tiruan untuk gambar.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-products --test it_products_admin`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

#[path = "admin/import.rs"]
mod import;
#[path = "admin/products.rs"]
mod products;
