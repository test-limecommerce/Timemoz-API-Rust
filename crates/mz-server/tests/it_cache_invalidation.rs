//! Invalidasi cache lintas proses (Redis Pub/Sub) terhadap Redis nyata.
//! Memakai channel UNIK per test (jangan pernah `mz:cache-invalidate`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use mz_core::AppState;
use mz_core::cache::Keep;
use mz_core::enums::BaseRole;
use mz_core::feature_modules::CompanyModules;
use mz_core::permissions::EffectiveAccess;
use mz_core::roles::RoleAccessSnapshot;
use mz_testkit::TestApp;
use serde_json::{Value, json};

fn keep() -> Keep {
    Keep::ttl(Duration::from_secs(60))
}

async fn app_with_channel() -> Option<(TestApp, String)> {
    let channel = format!("mz:test:cache-invalidate:{}", uuid::Uuid::new_v4().simple());
    let app =
        TestApp::with_vars(mz_server::app, &[("CACHE_INVALIDATION_CHANNEL", &channel)]).await?;
    Some((app, channel))
}

async fn raw_client() -> Client {
    let c = Builder::default_centralized().build().unwrap();
    c.init().await.unwrap();
    c
}

async fn raw_publish(c: &Client, channel: &str, v: Value) {
    let _: i64 = c.publish(channel, v.to_string()).await.unwrap();
}

async fn raw_publish_str(c: &Client, channel: &str, s: &str) {
    let _: i64 = c.publish(channel, s).await.unwrap();
}

fn snapshot(ids: &[i32]) -> Arc<RoleAccessSnapshot> {
    Arc::new(RoleAccessSnapshot {
        codes: vec![],
        custom_base_role: None,
        role_ids: ids.to_vec(),
    })
}

fn access(role_id: i32) -> Option<Arc<EffectiveAccess>> {
    Some(Arc::new(EffectiveAccess {
        role_id,
        role_code: "STAFF".into(),
        role_name: "Staff".into(),
        is_system: true,
        base_role: Option::<BaseRole>::None,
        effective_role_code: "STAFF".into(),
        company_id: 1,
        permissions: Default::default(),
    }))
}

/// Isi cache untuk user 1 dan 2 / company 1 dan 2 / role 50 dan 51.
fn prime(state: &AppState) {
    let c = &state.caches;
    let e = c.revocation.epoch();
    c.revocation.set(1, None, e, keep());
    c.revocation.set(2, None, e, keep());
    let e = c.roles.epoch();
    c.roles.set((1, 9), snapshot(&[50]), e, keep());
    c.roles.set((2, 9), snapshot(&[51]), e, keep());
    let e = c.permissions.epoch();
    c.permissions.set((1, 9), access(50), e, keep());
    c.permissions.set((2, 9), access(51), e, keep());
    let e = c.modules.epoch();
    for id in [1, 2] {
        c.modules.set(
            id,
            Some(Arc::new(CompanyModules {
                company_id: id,
                modules: vec![],
            })),
            e,
            keep(),
        );
    }
    let e = c.subscription_lock.epoch();
    c.subscription_lock
        .set(1, (Default::default(), Duration::from_secs(1)), e, keep());
    c.subscription_lock
        .set(2, (Default::default(), Duration::from_secs(1)), e, keep());
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timeout menunggu: {what}");
}

/// Tunggu subscriber aktif: kirim pesan `token-revocation` berulang sampai efeknya terlihat.
async fn wait_subscribed(app: &TestApp, raw: &Client, channel: &str) {
    for _ in 0..100 {
        let e = app.state.caches.revocation.epoch();
        app.state.caches.revocation.set(999, None, e, keep());
        raw_publish(
            raw,
            channel,
            json!({"v":1,"src":"node-probe","kind":"token-revocation","userId":999}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        if app.state.caches.revocation.get_fresh(&999).is_none() {
            return;
        }
    }
    panic!("subscriber tidak aktif");
}

#[tokio::test]
async fn remote_messages_clear_the_right_entries() {
    let Some((app, channel)) = app_with_channel().await else {
        return;
    };
    let _sub = mz_core::invalidation::spawn_subscriber(&app.state).unwrap();
    let raw = raw_client().await;
    wait_subscribed(&app, &raw, &channel).await;
    let c = app.state.caches.clone();
    let msg = |kind: &str, extra: Value| {
        let mut v = json!({"v":1,"src":"node-abc","kind":kind});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };

    // token-revocation
    prime(&app.state);
    raw_publish(&raw, &channel, msg("token-revocation", json!({"userId":1}))).await;
    wait_until("revocation user 1", || c.revocation.get_fresh(&1).is_none()).await;
    assert!(c.revocation.get_fresh(&2).is_some());
    assert!(c.roles.get_fresh(&(1, 9)).is_some());

    // user-access: roles + permissions user 1 di semua company
    prime(&app.state);
    raw_publish(&raw, &channel, msg("user-access", json!({"userId":1}))).await;
    wait_until("user-access", || c.roles.get_fresh(&(1, 9)).is_none()).await;
    assert!(c.permissions.get_fresh(&(1, 9)).is_none());
    assert!(c.roles.get_fresh(&(2, 9)).is_some());
    assert!(c.permissions.get_fresh(&(2, 9)).is_some());
    assert!(c.revocation.get_fresh(&1).is_some());

    // role
    prime(&app.state);
    raw_publish(&raw, &channel, msg("role", json!({"roleId":51}))).await;
    wait_until("role 51", || c.roles.get_fresh(&(2, 9)).is_none()).await;
    assert!(c.permissions.get_fresh(&(2, 9)).is_none());
    assert!(c.roles.get_fresh(&(1, 9)).is_some());
    assert!(c.permissions.get_fresh(&(1, 9)).is_some());

    // access-all
    prime(&app.state);
    raw_publish(&raw, &channel, msg("access-all", json!({}))).await;
    wait_until("access-all", || c.roles.is_empty()).await;
    assert!(c.permissions.is_empty());
    assert!(c.revocation.get_fresh(&1).is_some());
    assert!(c.modules.get_fresh(&1).is_some());

    // company-modules
    prime(&app.state);
    raw_publish(
        &raw,
        &channel,
        msg("company-modules", json!({"companyId":2})),
    )
    .await;
    wait_until("company 2", || c.modules.get_fresh(&2).is_none()).await;
    assert!(c.modules.get_fresh(&1).is_some());

    // subscription-lock
    prime(&app.state);
    raw_publish(
        &raw,
        &channel,
        msg("subscription-lock", json!({"userId":2})),
    )
    .await;
    wait_until("sublock 2", || c.subscription_lock.get_fresh(&2).is_none()).await;
    assert!(c.subscription_lock.get_fresh(&1).is_some());

    // Node-only, unknown, versi lain, rusak: tidak ada yang berubah. Sentinel
    // sesudahnya membuktikan semuanya sudah diproses (urutan satu channel).
    prime(&app.state);
    raw_publish(&raw, &channel, msg("division-gate", json!({"companyId":1}))).await;
    raw_publish(&raw, &channel, msg("table-field-policy", json!({}))).await;
    raw_publish(&raw, &channel, msg("from-the-future", json!({"userId":1}))).await;
    let mut v2 = msg("user-access", json!({"userId":1}));
    v2["v"] = json!(2);
    raw_publish(&raw, &channel, v2).await;
    raw_publish_str(&raw, &channel, "{not json").await;
    raw_publish(&raw, &channel, msg("user-access", json!({"userId":"1"}))).await;
    raw_publish(&raw, &channel, msg("token-revocation", json!({"userId":2}))).await;
    wait_until("sentinel", || c.revocation.get_fresh(&2).is_none()).await;
    assert!(c.revocation.get_fresh(&1).is_some());
    assert!(c.roles.get_fresh(&(1, 9)).is_some());
    assert!(c.permissions.get_fresh(&(1, 9)).is_some());
    assert!(c.modules.get_fresh(&1).is_some());
    assert!(c.modules.get_fresh(&2).is_some());
    assert!(c.subscription_lock.get_fresh(&1).is_some());
}

#[tokio::test]
async fn own_src_is_ignored() {
    let Some((app, channel)) = app_with_channel().await else {
        return;
    };
    let _sub = mz_core::invalidation::spawn_subscriber(&app.state).unwrap();
    let raw = raw_client().await;
    wait_subscribed(&app, &raw, &channel).await;
    let c = app.state.caches.clone();
    prime(&app.state);
    let own = mz_core::invalidation::instance_id();
    raw_publish(
        &raw,
        &channel,
        json!({"v":1,"src":own,"kind":"token-revocation","userId":1}),
    )
    .await;
    raw_publish(&raw, &channel, json!({"v":1,"src":own,"kind":"access-all"})).await;
    raw_publish(
        &raw,
        &channel,
        json!({"v":1,"src":"node-x","kind":"token-revocation","userId":2}),
    )
    .await;
    wait_until("sentinel", || c.revocation.get_fresh(&2).is_none()).await;
    assert!(c.revocation.get_fresh(&1).is_some());
    assert!(!c.roles.is_empty());
}

#[tokio::test]
async fn local_invalidation_publishes_messages() {
    let Some((app, channel)) = app_with_channel().await else {
        return;
    };
    let sub = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    sub.init().await.unwrap();
    let mut rx = sub.message_rx();
    sub.subscribe(channel.as_str()).await.unwrap();
    sub.ping::<String>(None).await.ok();

    prime(&app.state);
    app.state.invalidate_user_access(1);
    app.state.invalidate_token_revocation(1);
    mz_core::feature_modules::invalidate_company(&app.state, 2);
    mz_core::subscription_lock::invalidate_user(&app.state, 2);

    // Lokal langsung berlaku.
    assert!(app.state.caches.roles.get_fresh(&(1, 9)).is_none());
    assert!(app.state.caches.revocation.get_fresh(&1).is_none());
    assert!(app.state.caches.modules.get_fresh(&2).is_none());
    assert!(app.state.caches.subscription_lock.get_fresh(&2).is_none());

    let src = mz_core::invalidation::instance_id();
    let mut got: Vec<Value> = Vec::new();
    while got.len() < 4 {
        let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("pesan tidak datang")
            .unwrap();
        got.push(serde_json::from_str(m.value.as_str().unwrap().as_ref()).unwrap());
    }
    let expect = [
        json!({"v":1,"src":src,"kind":"user-access","userId":1}),
        json!({"v":1,"src":src,"kind":"token-revocation","userId":1}),
        json!({"v":1,"src":src,"kind":"company-modules","companyId":2}),
        json!({"v":1,"src":src,"kind":"subscription-lock","userId":2}),
    ];
    for e in &expect {
        assert!(got.contains(e), "pesan {e} tidak ada di {got:?}");
    }
}

#[tokio::test]
async fn disabled_does_not_publish_or_subscribe() {
    let channel = format!("mz:test:cache-invalidate:{}", uuid::Uuid::new_v4().simple());
    let Some(app) = TestApp::with_vars(
        mz_server::app,
        &[
            ("CACHE_INVALIDATION_CHANNEL", &channel),
            ("CACHE_INVALIDATION_ENABLED", "false"),
        ],
    )
    .await
    else {
        return;
    };
    assert!(mz_core::invalidation::spawn_subscriber(&app.state).is_none());
    let sub = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    sub.init().await.unwrap();
    let mut rx = sub.message_rx();
    sub.subscribe(channel.as_str()).await.unwrap();
    app.state.invalidate_user_access(1);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .is_err()
    );
}
