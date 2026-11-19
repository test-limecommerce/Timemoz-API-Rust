//! Pembantu uji bersama: emulator GCS (satu implementasi di `mz_testkit::gcs`).
#![allow(dead_code, unused_imports)]

pub use mz_testkit::gcs::{Emulator, Store, start_emulator};

pub const BUCKET: &str = "it-invoices-bucket";
