//! `{ success, message, data }` — bentuk balasan hampir semua method service integrasi Node.
//!
//! `serde_json::Value` di workspace ini TIDAK menjaga urutan kunci (tanpa fitur
//! `preserve_order`), jadi respons dibangun dari struct ber-`Serialize` supaya urutan kunci
//! sama dengan objek literal Node.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Envelope<T> {
    pub success: bool,
    pub message: String,
    /// `null` bila `None`.
    pub data: Option<T>,
}

impl<T> Envelope<T> {
    /// `{ success: true, message, data }`.
    pub fn ok(message: impl Into<String>, data: T) -> Self {
        Self {
            success: true,
            message: message.into(),
            data: Some(data),
        }
    }

    /// `{ success: false, message, data: null }`.
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            data: None,
        }
    }
}
