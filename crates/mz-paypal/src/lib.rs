//! Klien PayPal lengkap = port `src/paypal/paypal.service.ts` (Node). Tanpa route.
//!
//! * URL dasar: `PAYPAL_MODE=live` -> `https://api-m.paypal.com`, selain itu sandbox.
//!   `MZ_PAYPAL_API_BASE` (BUKAN fitur Node) mengalihkan ke server tiruan saat uji.
//! * Token OAuth di-cache per proses (statik, kunci = base+client id+secret), kedaluwarsa
//!   `expires_in - 60 dtk` seperti Node.
//! * Galat = [`PaypalError`]; `Display` = `err.message` axios, `data` = `err.response.data`.
//! * Semua body JSON dibangun dengan urutan kunci sama seperti Node.

mod client;
mod error;
mod payloads;

pub use client::{PaypalClient, PaypalConfig, TOKEN_PATH, to_fixed2};
pub use error::PaypalError;
pub use payloads::{CreatePlanPayload, CreateSubscriptionPayload, get_approval_url};
