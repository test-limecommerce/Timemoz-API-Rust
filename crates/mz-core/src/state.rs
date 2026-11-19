use std::sync::Arc;
use std::time::Duration;

use fred::prelude::*;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::config::Config;
use crate::feature_modules::ModuleCache;
use crate::jwt::RevocationCache;
use crate::permissions::PermissionCache;
use crate::roles::RoleCache;
use crate::subscription_lock::SubscriptionLockCache;

/// Semua cache in-process. Satu instans per proses (setara objek modul Node).
pub struct Caches {
    pub revocation: RevocationCache,
    pub roles: RoleCache,
    pub permissions: PermissionCache,
    pub modules: ModuleCache,
    pub subscription_lock: SubscriptionLockCache,
}

impl Caches {
    pub fn new() -> Self {
        Self {
            revocation: crate::jwt::new_revocation_cache(),
            roles: crate::roles::new_role_cache(),
            permissions: crate::permissions::new_permission_cache(),
            modules: crate::feature_modules::new_module_cache(),
            subscription_lock: crate::subscription_lock::new_cache(),
        }
    }

    /// `PermissionsService.invalidate(userId)` — juga membersihkan cache role
    /// (satu pintu invalidasi, `role-cache.ts`). Hanya lokal; pemanggil biasa
    /// memakai [`AppState::invalidate_user_access`] yang juga mem-publish.
    pub fn invalidate_user_access_local(&self, user_id: i32) {
        self.permissions.invalidate_where(|(u, _), _| *u == user_id);
        self.roles.invalidate_where(|(u, _), _| *u == user_id);
    }

    /// `invalidateTokenRevocation(userId)` — panggil SESUDAH commit.
    /// Hanya lokal; lihat [`AppState::invalidate_token_revocation`].
    pub fn invalidate_token_revocation_local(&self, user_id: i32) {
        self.revocation.invalidate_where(|u, _| *u == user_id);
    }

    /// Bersihkan SEMUA cache yang diatur protokol invalidasi (aturan 5).
    pub fn clear_all(&self) {
        self.revocation.invalidate_all();
        self.roles.invalidate_all();
        self.permissions.invalidate_all();
        self.modules.invalidate_all();
        self.subscription_lock.invalidate_all();
        crate::invalidation::clear_external();
    }
}

impl Default for Caches {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: Pool,
    pub config: Arc<Config>,
    pub caches: Arc<Caches>,
    pub http: reqwest::Client,
}

impl AppState {
    /// Invalidasi lokal + publish `user-access` ke proses lain.
    pub fn invalidate_user_access(&self, user_id: i32) {
        self.caches.invalidate_user_access_local(user_id);
        crate::invalidation::publish(self, crate::invalidation::Message::UserAccess { user_id });
    }

    /// `PermissionsService.invalidateRole(roleId)`: lokal (cache role + permission semua pemegang
    /// role itu) + publish `role`. Panggil SESUDAH commit.
    pub fn invalidate_role(&self, role_id: i32) {
        let msg = crate::invalidation::Message::Role { role_id };
        crate::invalidation::apply(&self.caches, &msg);
        crate::invalidation::publish(self, msg);
    }

    /// Invalidasi lokal + publish `token-revocation` — panggil SESUDAH commit.
    pub fn invalidate_token_revocation(&self, user_id: i32) {
        self.caches.invalidate_token_revocation_local(user_id);
        crate::invalidation::publish(
            self,
            crate::invalidation::Message::TokenRevocation { user_id },
        );
    }

    /// State tanpa koneksi nyata (pool lazy, Redis belum di-init) untuk test
    /// jalur yang tidak menyentuh DB/Redis.
    pub fn for_test() -> anyhow::Result<Self> {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://test:test@127.0.0.1:1/test")?;
        let redis = Builder::default_centralized().build_pool(1)?;
        Ok(Self {
            db,
            redis,
            config: Arc::new(Config::for_test()),
            caches: Arc::new(Caches::new()),
            http: reqwest::Client::new(),
        })
    }

    pub async fn connect(config: Config) -> anyhow::Result<Self> {
        let db = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(crate::db::connect_options(&config.database_url)?)
            .await?;

        let redis = redis_builder(&config).build_pool(4)?;
        redis.init().await?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            db,
            redis,
            config: Arc::new(config),
            caches: Arc::new(Caches::new()),
            http,
        })
    }
}

/// Builder Redis bersama (pool perintah dan koneksi subscriber).
pub(crate) fn redis_builder(config: &Config) -> Builder {
    let redis_config = fred::types::config::Config {
        server: fred::types::config::ServerConfig::new_centralized(
            config.redis_host.clone(),
            config.redis_port,
        ),
        password: config.redis_password.clone(),
        database: config.redis_db,
        ..Default::default()
    };
    let mut builder = Builder::from_config(redis_config);
    builder
        .with_connection_config(|c| {
            c.connection_timeout = Duration::from_secs(5);
        })
        .set_policy(ReconnectPolicy::new_exponential(0, 100, 3_000, 2));
    builder
}
