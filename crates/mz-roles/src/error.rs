//! Galat crate ini = [`ApiError`] mz-core. Body objek Node (`{ code, message }`,
//! `{ code, feature, actions, message }`, ...) dibangun dengan `ApiError::coded` atau
//! `ApiError::serialized` dari struct — urutan kunci di kabel = urutan objek literal Node
//! (workspace memakai `serde_json` dengan fitur `preserve_order`).

use mz_core::{ApiError, ApiResult};

pub type RolesError = ApiError;
pub type RolesResult<T> = ApiResult<T>;
