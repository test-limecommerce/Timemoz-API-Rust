//! Helper bersama uji integrasi mz-core.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use fred::prelude::*;
use mz_core::AppState;
use mz_core::config::Config;

/// State dengan Redis nyata; `None` (SKIP) bila tidak tersambung.
pub async fn redis_state() -> Option<AppState> {
    let mut state = AppState::for_test().ok()?;
    let mut config = Config::for_test();
    config.redis_host = std::env::var("REDIS_HOST").unwrap_or_else(|_| "localhost".into());
    config.redis_port = std::env::var("REDIS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6379);
    config.redis_password = std::env::var("REDIS_PASSWORD")
        .ok()
        .filter(|v| !v.is_empty());
    config.redis_db = Some(
        std::env::var("MZ_TEST_REDIS_DB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
    );
    let rc = fred::types::config::Config {
        server: fred::types::config::ServerConfig::new_centralized(
            config.redis_host.clone(),
            config.redis_port,
        ),
        password: config.redis_password.clone(),
        database: config.redis_db,
        ..Default::default()
    };
    let pool = Builder::from_config(rc)
        .with_connection_config(|c| c.connection_timeout = Duration::from_secs(2))
        .build_pool(2)
        .ok()?;
    let ok = tokio::time::timeout(Duration::from_secs(3), pool.init()).await;
    if !matches!(ok, Ok(Ok(_))) {
        assert!(
            std::env::var("MZ_TEST_REQUIRE_DB").is_err(),
            "MZ_TEST_REQUIRE_DB=1 tetapi Redis tidak tersedia"
        );
        eprintln!("SKIP uji integrasi: Redis tidak tersedia");
        return None;
    }
    state.redis = pool;
    state.config = Arc::new(config);
    Some(state)
}
