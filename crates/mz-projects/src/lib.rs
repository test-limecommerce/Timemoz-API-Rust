//! Port `src/projects` dari Mudah-API (8 route `ProjectsController`).
#![cfg_attr(test, allow(clippy::indexing_slicing))]

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::AppState;
use mz_core::RouteSpec;
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_jobs::{CronJob, Schedule};
use std::time::Duration;

pub mod approvers;
pub mod budget;
pub mod dto;
pub mod handlers;
pub use mz_core::jsfmt;
pub mod model;
pub mod reset;
pub mod rowjson;
pub mod service;

/// Cron modul budget. Node: `src/project-budget/project-budget.cron.ts:13`
/// (`@Cron('5 0 * * *', { timeZone: CRON_TIMEZONE })`).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "budget-daily-reset",
        schedule: Schedule::daily(&[0], &[5]),
        timeout: Duration::from_secs(600),
        run: |s| Box::pin(reset::run_daily_reset(s)),
    }]
}

/// `@RequireFeature('projects')` (class) + `@Roles(SYSTEM_ADMIN..GUEST)`.
const READ: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
        Role::STAFF,
        Role::GUEST,
    ])
    .feature("projects");

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`.
const WRITE: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .feature("projects");

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/projects", guard(state, READ, get(handlers::find_all)))
        .route("/projects", guard(state, WRITE, post(handlers::create)))
        .route(
            "/projects/{uuid}",
            guard(state, READ, get(handlers::find_one)),
        )
        .route(
            "/projects/{uuid}",
            guard(state, WRITE, patch(handlers::update)),
        )
        .route(
            "/projects/{uuid}",
            guard(state, WRITE, delete(handlers::delete)),
        )
        .route(
            "/projects/hour/{uuid}",
            guard(state, WRITE, post(handlers::create_hour)),
        )
        .route(
            "/projects/hour/{uuid}",
            guard(state, WRITE, patch(handlers::update_hour)),
        )
        .route(
            "/projects/hour/{uuid}",
            guard(state, WRITE, delete(handlers::delete_hour)),
        )
}
