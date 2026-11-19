//! Fondasi bersama untuk port Mudah-API ke Rust.
//!
//! Semua yang di Node hidup sebagai guard / interceptor global / helper lintas
//! modul ada di sini, supaya crate domain (`mz-auth`, `mz-users`, ...) hanya
//! berisi handler dan query miliknya sendiri.

pub mod bg;
pub mod cache;
pub mod company;
pub mod config;
pub mod csv_parse;
pub mod db;
pub mod enums;
pub mod error;
pub mod extract;
pub mod feature_modules;
pub mod feature_registry;
pub mod gate;
pub mod invalidation;
pub mod invoice;
pub mod iso8601;
pub mod jsfmt;
pub mod jwt;
pub mod mail;
pub mod mixpanel;
pub mod permissions;
pub mod plan;
pub mod prisma;
pub mod qs;
pub mod queue;
pub mod rate_limit;
pub mod redirect;
pub mod roles;
pub mod state;
pub mod subscription_lock;
pub mod time;
pub mod tz;
pub mod upload;
pub mod user_status;
pub mod uuid_guard;
pub mod uuid_rules;

pub use error::{ApiError, ApiResult};
pub use gate::{RequestCtx, RouteSpec};
pub use state::AppState;
