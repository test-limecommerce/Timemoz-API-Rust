//! Pesan protokol invalidasi (`plan/cache-invalidation-protocol.md`) yang dikirim route
//! `mz-roles` ke Redis nyata: urutan & isinya harus sama dengan `PermissionsService.invalidateRole`
//! Node (satu `{"kind":"role","roleId":..}` per panggilan, tanpa `user-access`/`access-all`).
//! Channel UNIK per test (jangan pernah `mz:cache-invalidate`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};

fn build(state: &AppState) -> Router {
    mz_roles::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

fn ok(res: &TestResponse) -> &TestResponse {
    res.assert_no_sql_error();
    res.assert_ok();
    res
}

async fn role_id(app: &TestApp, uuid: &str) -> i32 {
    sqlx::query_scalar(r#"SELECT id FROM "accessLevel" WHERE uuid = $1::uuid"#)
        .bind(uuid)
        .fetch_one(app.db())
        .await
        .unwrap()
}

/// Ambil semua pesan sampai `quiet` tanpa pesan baru.
async fn drain(
    rx: &mut tokio::sync::broadcast::Receiver<fred::types::Message>,
    quiet: Duration,
) -> Vec<Value> {
    let mut got = Vec::new();
    while let Ok(Ok(m)) = tokio::time::timeout(quiet, rx.recv()).await {
        let raw = m.value.as_str().map(|s| s.to_string()).unwrap_or_default();
        got.push(serde_json::from_str(&raw).unwrap_or(Value::Null));
    }
    got
}

fn role_msg(role_id: i32) -> Value {
    json!({"v": 1, "src": mz_core::invalidation::instance_id(), "kind": "role", "roleId": role_id})
}

#[tokio::test]
async fn role_changes_publish_role_messages() {
    let channel = format!("mz:test:roles:{}", uuid::Uuid::new_v4().simple());
    let Some(app) = TestApp::with_vars(build, &[("CACHE_INVALIDATION_CHANNEL", &channel)]).await
    else {
        return;
    };
    let c: Company = app.company().await;
    let sub = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    sub.init().await.unwrap();
    let mut rx = sub.message_rx();
    sub.subscribe(channel.as_str()).await.unwrap();
    sub.ping::<String>(None).await.ok();
    let quiet = Duration::from_millis(500);
    let admin = &c.admin.token;

    // create: Node tidak meng-invalidasi apa pun (role baru belum punya pemegang).
    let res = app
        .post("/v1/admin/roles")
        .token(admin)
        .json(
            &json!({"name": "Pubsub", "baseRole": "STAFF", "permissions": [
            {"featureKey": "clock", "actions": ["READ"]}]}),
        )
        .send()
        .await;
    res.assert_status(201);
    let uuid = res.data()["uuid"].as_str().unwrap().to_owned();
    let id = role_id(&app, &uuid).await;
    assert_eq!(drain(&mut rx, quiet).await, Vec::<Value>::new());

    // PUT permissions / PATCH → satu pesan `role`.
    ok(&app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(admin)
        .json(&json!({"permissions": [{"featureKey": "clock", "actions": ["WRITE"]}]}))
        .send()
        .await);
    assert_eq!(drain(&mut rx, quiet).await, vec![role_msg(id)]);
    ok(&app
        .patch(&format!("/v1/admin/roles/{uuid}"))
        .token(admin)
        .json(&json!({"description": "x"}))
        .send()
        .await);
    assert_eq!(drain(&mut rx, quiet).await, vec![role_msg(id)]);

    // Penolakan (422) tidak mengirim apa pun.
    let res = app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(admin)
        .json(&json!({"permissions": [{"featureKey": "store-visits", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(drain(&mut rx, quiet).await, Vec::<Value>::new());

    // customize bawaan → `role` sumber lalu `role` override; revert sama.
    let (staff_id, staff_uuid): (i32, String) = sqlx::query_as(
        r#"SELECT id, uuid::text FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem""#,
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    let res = app
        .put(&format!("/v1/admin/roles/{staff_uuid}/customize"))
        .token(admin)
        .json(&json!({"permissions": [{"featureKey": "clock", "actions": ["READ"]}]}))
        .send()
        .await;
    ok(&res);
    let override_uuid = res.data()["uuid"].as_str().unwrap().to_owned();
    let override_id = role_id(&app, &override_uuid).await;
    assert_eq!(
        drain(&mut rx, quiet).await,
        vec![role_msg(staff_id), role_msg(override_id)]
    );
    ok(&app
        .post(&format!("/v1/admin/roles/{override_uuid}/revert"))
        .token(admin)
        .send()
        .await);
    assert_eq!(
        drain(&mut rx, quiet).await,
        vec![role_msg(staff_id), role_msg(override_id)]
    );

    // delete → satu pesan `role` untuk role yang dihapus.
    ok(&app
        .delete(&format!("/v1/admin/roles/{uuid}"))
        .token(admin)
        .send()
        .await);
    assert_eq!(drain(&mut rx, quiet).await, vec![role_msg(id)]);

    // Rute baca tidak mengirim apa pun.
    ok(&app.get("/v1/admin/roles").token(admin).send().await);
    ok(&app.get("/v1/me/permissions").token(admin).send().await);
    assert_eq!(drain(&mut rx, quiet).await, Vec::<Value>::new());
}
