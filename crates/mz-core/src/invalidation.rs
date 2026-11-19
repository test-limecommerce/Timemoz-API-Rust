//! Invalidasi cache lintas proses lewat Redis Pub/Sub — sisi Rust dari
//! `plan/cache-invalidation-protocol.md` (v1).
//!
//! - [`publish`]: fire-and-forget, dipanggil SESUDAH invalidasi lokal.
//! - [`apply`]: menerapkan pesan masuk, HANYA lokal (tidak mem-publish ulang).
//! - [`spawn_subscriber`]: task latar dengan koneksi subscriber terpisah yang
//!   menyambung ulang selamanya; setelah re-subscribe pasca putus, semua cache
//!   protokol dibersihkan (aturan 5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::state::{Caches, redis_builder};

pub const PROTOCOL_VERSION: i64 = 1;

/// ID instans proses ini: `rust-<random>`, dibuat sekali.
pub fn instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("rust-{}", uuid::Uuid::new_v4().simple()))
}

/// Pesan yang dipublish/diterapkan Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    TokenRevocation {
        user_id: i32,
    },
    UserAccess {
        user_id: i32,
    },
    Role {
        role_id: i32,
    },
    AccessAll,
    CompanyModules {
        company_id: i32,
    },
    SubscriptionLock {
        user_id: i32,
    },
    /// `TableFieldPolicyCache` (`table-field-policy`); `None` = semua company. Cache-nya milik
    /// `mz-table-fields`, diterapkan lewat [`register_external`].
    TableFieldPolicy {
        company_id: Option<i32>,
    },
}

/// Hasil parse pesan yang sah secara struktur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Pesan untuk Rust.
    Msg { src: String, msg: Message },
    /// `kind` tak dikenal atau khusus Node (`division-gate`: Rust membaca gerbang divisi dari DB
    /// tanpa cache).
    Ignored,
}

impl Message {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TokenRevocation { .. } => "token-revocation",
            Self::UserAccess { .. } => "user-access",
            Self::Role { .. } => "role",
            Self::AccessAll => "access-all",
            Self::CompanyModules { .. } => "company-modules",
            Self::SubscriptionLock { .. } => "subscription-lock",
            Self::TableFieldPolicy { .. } => "table-field-policy",
        }
    }

    /// Objek JSON protokol dengan `src` yang diberikan.
    pub fn to_value(&self, src: &str) -> Value {
        let mut v = json!({ "v": PROTOCOL_VERSION, "src": src, "kind": self.kind() });
        let extra = match self {
            Self::TokenRevocation { user_id }
            | Self::UserAccess { user_id }
            | Self::SubscriptionLock { user_id } => Some(("userId", *user_id)),
            Self::Role { role_id } => Some(("roleId", *role_id)),
            Self::CompanyModules { company_id } => Some(("companyId", *company_id)),
            Self::TableFieldPolicy { company_id } => company_id.map(|id| ("companyId", id)),
            Self::AccessAll => None,
        };
        if let (Some((k, n)), Some(obj)) = (extra, v.as_object_mut()) {
            obj.insert(k.to_owned(), Value::from(n));
        }
        v
    }

    pub fn to_json(&self, src: &str) -> String {
        self.to_value(src).to_string()
    }

    /// `Err` = rusak (pemanggil mencatat `warn`); `Ok(Ignored)` = abaikan diam-diam.
    pub fn parse(raw: &str) -> Result<Parsed, String> {
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("bukan JSON: {e}"))?;
        let obj = v.as_object().ok_or("bukan objek JSON")?;
        match obj.get("v").and_then(Value::as_i64) {
            Some(PROTOCOL_VERSION) => {}
            _ => return Ok(Parsed::Ignored),
        }
        let src = obj
            .get("src")
            .and_then(Value::as_str)
            .ok_or("field `src` hilang")?
            .to_owned();
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("field `kind` hilang")?;
        let int = |field: &str| -> Result<i32, String> {
            obj.get(field)
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| format!("field `{field}` hilang/bukan integer (kind={kind})"))
        };
        let msg = match kind {
            "token-revocation" => Self::TokenRevocation {
                user_id: int("userId")?,
            },
            "user-access" => Self::UserAccess {
                user_id: int("userId")?,
            },
            "role" => Self::Role {
                role_id: int("roleId")?,
            },
            "access-all" => Self::AccessAll,
            "company-modules" => Self::CompanyModules {
                company_id: int("companyId")?,
            },
            "subscription-lock" => Self::SubscriptionLock {
                user_id: int("userId")?,
            },
            // `companyId` tidak ada = semua company; ada tapi bukan integer = rusak.
            "table-field-policy" => Self::TableFieldPolicy {
                company_id: match obj.get("companyId") {
                    None | Some(Value::Null) => None,
                    Some(_) => Some(int("companyId")?),
                },
            },
            _ => return Ok(Parsed::Ignored),
        };
        Ok(Parsed::Msg { src, msg })
    }
}

/// Hook invalidasi untuk cache yang hidup di crate lain (mis. kebijakan kolom `mz-table-fields`).
/// `Some(msg)` = pesan yang baru diterapkan; `None` = bersihkan semuanya (subscriber tersambung
/// ulang, aturan 5).
pub type ExternalHook = fn(Option<&Message>);

static EXTERNAL_HOOKS: RwLock<Vec<ExternalHook>> = RwLock::new(Vec::new());

/// Daftarkan hook sekali per proses (biasanya dari inisialisasi cache crate itu).
pub fn register_external(hook: ExternalHook) {
    if let Ok(mut hooks) = EXTERNAL_HOOKS.write()
        && !hooks.iter().any(|h| std::ptr::fn_addr_eq(*h, hook))
    {
        hooks.push(hook);
    }
}

fn run_external(msg: Option<&Message>) {
    if let Ok(hooks) = EXTERNAL_HOOKS.read() {
        for hook in hooks.iter() {
            hook(msg);
        }
    }
}

/// Bersihkan cache eksternal (bagian dari [`Caches::clear_all`]).
pub fn clear_external() {
    run_external(None);
}

/// Terapkan pesan dari proses lain: hanya invalidasi lokal.
pub fn apply(caches: &Caches, msg: &Message) {
    run_external(Some(msg));
    match *msg {
        Message::TokenRevocation { user_id } => caches.invalidate_token_revocation_local(user_id),
        Message::UserAccess { user_id } => caches.invalidate_user_access_local(user_id),
        Message::Role { role_id } => {
            caches
                .roles
                .invalidate_where(|_, v| v.role_ids.contains(&role_id));
            caches
                .permissions
                .invalidate_where(|_, v| v.as_ref().is_some_and(|a| a.role_id == role_id));
        }
        Message::AccessAll => {
            caches.roles.invalidate_all();
            caches.permissions.invalidate_all();
        }
        Message::CompanyModules { company_id } => {
            crate::feature_modules::invalidate_company_local(caches, company_id);
        }
        Message::SubscriptionLock { user_id } => {
            crate::subscription_lock::invalidate_user_local(caches, user_id);
        }
        Message::TableFieldPolicy { .. } => {}
    }
}

/// `warn` dibatasi: paling banyak satu per 30 detik per proses.
fn warn_limited(what: &str, err: &dyn std::fmt::Display) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let last = LAST.load(Ordering::Relaxed);
    if (last == 0 || now.saturating_sub(last) >= 30)
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(error = %err, "{what}");
    }
}

/// Fire-and-forget: tidak pernah ditunggu, galat hanya `warn` (dibatasi).
/// Tanpa runtime Tokio (mis. test sinkron) atau saat dimatikan = no-op.
pub fn publish(state: &AppState, msg: Message) {
    if !state.config.cache_invalidation_enabled() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let channel = state.config.cache_invalidation_channel();
    let payload = msg.to_json(instance_id());
    let redis = state.redis.clone();
    handle.spawn(async move {
        let res: Result<i64, _> = redis.next().publish(channel, payload).await;
        if let Err(e) = res {
            warn_limited("publish invalidasi cache gagal", &e);
        }
    });
}

fn handle_raw(caches: &Caches, raw: &str) {
    match Message::parse(raw) {
        Ok(Parsed::Msg { src, msg }) => {
            if src != instance_id() {
                apply(caches, &msg);
            }
        }
        Ok(Parsed::Ignored) => {}
        Err(e) => tracing::warn!(error = %e, "pesan invalidasi cache rusak, diabaikan"),
    }
}

/// Jalankan subscriber di latar. `None` bila dimatikan lewat config.
pub fn spawn_subscriber(state: &AppState) -> Option<JoinHandle<()>> {
    if !state.config.cache_invalidation_enabled() {
        tracing::info!(
            "invalidasi cache lintas proses dimatikan (CACHE_INVALIDATION_ENABLED=false)"
        );
        return None;
    }
    let state = state.clone();
    Some(tokio::spawn(async move {
        let mut backoff = Duration::from_millis(200);
        loop {
            match run_once(&state).await {
                Ok(()) => backoff = Duration::from_millis(200),
                Err(e) => warn_limited("subscriber invalidasi cache gagal", &e),
            }
            // Koneksi ditutup/gagal: setelah menyambung lagi pesan mungkin hilang.
            state.caches.clear_all();
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }))
}

/// Satu siklus koneksi subscriber; kembali (Ok/Err) hanya bila klien mati total.
async fn run_once(state: &AppState) -> Result<(), Error> {
    let channel = state.config.cache_invalidation_channel();
    let client = redis_builder(&state.config).build_subscriber_client()?;
    client.init().await?;
    let mut messages = client.message_rx();
    let mut reconnects = client.reconnect_rx();
    client.subscribe(channel.as_str()).await?;
    // Event koneksi pertama (saat init) bukan "reconnect": buang.
    while reconnects.try_recv().is_ok() {}
    tracing::info!(%channel, src = instance_id(), "subscriber invalidasi cache aktif");

    loop {
        tokio::select! {
            m = messages.recv() => match m {
                Ok(m) => {
                    if let Some(raw) = m.value.as_str() {
                        handle_raw(&state.caches, &raw);
                    } else {
                        tracing::warn!("pesan invalidasi cache bukan teks, diabaikan");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    state.caches.clear_all();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            r = reconnects.recv() => match r {
                Ok(_) => {
                    client.resubscribe_all().await?;
                    state.caches.clear_all();
                    tracing::info!("subscriber invalidasi cache tersambung ulang; cache dibersihkan");
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    client.resubscribe_all().await?;
                    state.caches.clear_all();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// `cacheManager.del('user-me-<id>')` — kunci Redis sama dengan Node (tanpa prefix). Varian yang
/// mengembalikan galat Redis (pemanggil memutuskan sendiri, mis. menambah konteks log).
pub async fn try_forget_user_me(state: &AppState, user_id: i32) -> Result<(), fred::error::Error> {
    state
        .redis
        .next()
        .del::<(), _>(format!("user-me-{user_id}"))
        .await
}

/// Seperti [`try_forget_user_me`], tetapi gagal Redis hanya dicatat: tulisan DB sudah commit, jadi
/// melaporkan galat ke klien akan menyesatkan.
pub async fn forget_user_me(state: &AppState, user_id: i32) {
    if let Err(err) = try_forget_user_me(state, user_id).await {
        tracing::warn!(user_id, error = %err, "gagal menghapus cache user-me");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Keep;
    use crate::roles::RoleAccessSnapshot;
    use std::sync::Arc;

    fn keep() -> Keep {
        Keep::ttl(Duration::from_secs(60))
    }

    #[test]
    fn serialize_matches_spec() {
        let v = Message::UserAccess { user_id: 42 }.to_value("rust-x");
        assert_eq!(
            v,
            json!({"v":1,"src":"rust-x","kind":"user-access","userId":42})
        );
        let v = Message::AccessAll.to_value("rust-x");
        assert_eq!(v, json!({"v":1,"src":"rust-x","kind":"access-all"}));
        let v = Message::CompanyModules { company_id: 7 }.to_value("s");
        assert_eq!(v.get("companyId"), Some(&json!(7)));
        let v = Message::Role { role_id: 3 }.to_value("s");
        assert_eq!(v.get("roleId"), Some(&json!(3)));
    }

    #[test]
    fn roundtrip_all_kinds() {
        for m in [
            Message::TokenRevocation { user_id: 1 },
            Message::UserAccess { user_id: 2 },
            Message::Role { role_id: 3 },
            Message::TableFieldPolicy {
                company_id: Some(4),
            },
            Message::TableFieldPolicy { company_id: None },
            Message::AccessAll,
            Message::CompanyModules { company_id: 4 },
            Message::SubscriptionLock { user_id: 5 },
        ] {
            assert_eq!(
                Message::parse(&m.to_json("node-1")),
                Ok(Parsed::Msg {
                    src: "node-1".into(),
                    msg: m
                })
            );
        }
    }

    #[test]
    fn parse_ignores_and_rejects() {
        let p = Message::parse;
        assert_eq!(
            p(r#"{"v":2,"src":"a","kind":"user-access","userId":1}"#),
            Ok(Parsed::Ignored)
        );
        assert_eq!(
            p(r#"{"v":1,"src":"a","kind":"future-thing"}"#),
            Ok(Parsed::Ignored)
        );
        assert_eq!(
            p(r#"{"v":1,"src":"a","kind":"division-gate","companyId":1}"#),
            Ok(Parsed::Ignored)
        );
        // `table-field-policy` kini diterapkan; `companyId` bukan integer = rusak.
        assert!(p(r#"{"v":1,"src":"a","kind":"table-field-policy","companyId":"x"}"#).is_err());
        assert!(p("not json").is_err());
        assert!(p("[1]").is_err());
        assert!(p(r#"{"v":1,"src":"a","kind":"user-access"}"#).is_err());
        assert!(p(r#"{"v":1,"src":"a","kind":"user-access","userId":"1"}"#).is_err());
        assert!(p(r#"{"v":1,"src":"a","kind":"user-access","userId":1.5}"#).is_err());
        assert!(p(r#"{"v":1,"kind":"access-all"}"#).is_err());
    }

    #[test]
    fn instance_id_format() {
        assert!(instance_id().starts_with("rust-"));
        assert_eq!(instance_id(), instance_id());
    }

    fn snapshot(ids: &[i32]) -> Arc<RoleAccessSnapshot> {
        Arc::new(RoleAccessSnapshot {
            codes: vec![],
            custom_base_role: None,
            role_ids: ids.to_vec(),
        })
    }

    #[test]
    fn apply_semantics() {
        let c = Caches::new();
        let e = c.roles.epoch();
        c.roles.set((1, 10), snapshot(&[5, 6]), e, keep());
        c.roles.set((1, 11), snapshot(&[7]), e, keep());
        c.roles.set((2, 10), snapshot(&[5]), e, keep());
        let e = c.revocation.epoch();
        c.revocation.set(1, None, e, keep());
        c.revocation.set(2, None, e, keep());
        let e = c.subscription_lock.epoch();
        c.subscription_lock
            .set(1, (Default::default(), Duration::from_secs(1)), e, keep());
        c.subscription_lock
            .set(2, (Default::default(), Duration::from_secs(1)), e, keep());

        apply(&c, &Message::Role { role_id: 5 });
        assert!(c.roles.get_fresh(&(1, 10)).is_none());
        assert!(c.roles.get_fresh(&(2, 10)).is_none());
        assert!(c.roles.get_fresh(&(1, 11)).is_some());

        apply(&c, &Message::UserAccess { user_id: 1 });
        assert!(c.roles.get_fresh(&(1, 11)).is_none());

        apply(&c, &Message::TokenRevocation { user_id: 1 });
        assert!(c.revocation.get_fresh(&1).is_none());
        assert!(c.revocation.get_fresh(&2).is_some());

        apply(&c, &Message::SubscriptionLock { user_id: 2 });
        assert!(c.subscription_lock.get_fresh(&2).is_none());
        assert!(c.subscription_lock.get_fresh(&1).is_some());

        apply(&c, &Message::AccessAll);
        assert!(c.roles.is_empty());
    }

    #[test]
    fn handle_raw_ignores_own_src_and_garbage() {
        let c = Caches::new();
        let e = c.revocation.epoch();
        c.revocation.set(1, None, e, keep());
        let own = Message::TokenRevocation { user_id: 1 }.to_json(instance_id());
        handle_raw(&c, &own);
        handle_raw(&c, "garbage");
        assert!(c.revocation.get_fresh(&1).is_some());
        handle_raw(
            &c,
            &Message::TokenRevocation { user_id: 1 }.to_json("node-9"),
        );
        assert!(c.revocation.get_fresh(&1).is_none());
    }

    #[test]
    fn config_defaults_and_overrides() {
        let mut cfg = crate::config::Config::for_test();
        assert!(cfg.cache_invalidation_enabled());
        assert_eq!(cfg.cache_invalidation_channel(), "mz:cache-invalidate");
        cfg.var_overrides
            .insert("CACHE_INVALIDATION_ENABLED".into(), "false".into());
        cfg.var_overrides
            .insert("CACHE_INVALIDATION_CHANNEL".into(), "x:y".into());
        assert!(!cfg.cache_invalidation_enabled());
        assert_eq!(cfg.cache_invalidation_channel(), "x:y");
    }

    /// Interop: string PERSIS keluaran `encodeMessage` Node (Mudah-API
    /// `src/cache-invalidation/cache-invalidation.protocol.ts`, 2026-11-16).
    /// Ubah bila protokol berubah; pasangannya ada di spec Node (golden Rust).
    #[test]
    fn parses_node_golden_messages() {
        let cases: &[(&str, Option<Message>)] = &[
            (
                r#"{"v":1,"src":"node-golden","kind":"token-revocation","userId":42}"#,
                Some(Message::TokenRevocation { user_id: 42 }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"user-access","userId":42}"#,
                Some(Message::UserAccess { user_id: 42 }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"role","roleId":7}"#,
                Some(Message::Role { role_id: 7 }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"access-all"}"#,
                Some(Message::AccessAll),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"company-modules","companyId":9}"#,
                Some(Message::CompanyModules { company_id: 9 }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"subscription-lock","userId":42}"#,
                Some(Message::SubscriptionLock { user_id: 42 }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"division-gate","companyId":9}"#,
                None,
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"table-field-policy","companyId":9}"#,
                Some(Message::TableFieldPolicy {
                    company_id: Some(9),
                }),
            ),
            (
                r#"{"v":1,"src":"node-golden","kind":"table-field-policy"}"#,
                Some(Message::TableFieldPolicy { company_id: None }),
            ),
        ];
        for (raw, want) in cases {
            let want = Ok(match want {
                Some(m) => Parsed::Msg {
                    src: "node-golden".into(),
                    msg: *m,
                },
                None => Parsed::Ignored,
            });
            assert_eq!(Message::parse(raw), want, "{raw}");
        }
    }

    /// Cetak keluaran Rust untuk golden di sisi Node (`cargo test -p mz-core
    /// print_rust_golden -- --nocapture`).
    #[test]
    fn print_rust_golden() {
        for m in [
            Message::TokenRevocation { user_id: 42 },
            Message::UserAccess { user_id: 42 },
            Message::Role { role_id: 7 },
            Message::AccessAll,
            Message::CompanyModules { company_id: 9 },
            Message::SubscriptionLock { user_id: 42 },
        ] {
            println!("GOLDEN {}", m.to_json("rust-golden"));
        }
    }
}
