//! Mixpanel end-to-end: `track` -> antrean Redis -> worker -> server HTTP palsu (axum) sebagai
//! `MIXPANEL_API_HOST`. Dilewati bila Redis tidak tersedia.
//! Satu fungsi test (antrean `mixpanel` bersifat global di DB Redis test).

#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

mod common;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use common::redis_state;
use fred::prelude::*;
use mz_core::AppState;
use mz_core::mixpanel::{self, MixpanelEvent, TrackInput};
use mz_core::queue;
use serde_json::{Value, json};

const UID: &str = "123e4567-e89b-42d3-a456-426614174000";

struct Srv {
    status: AtomicU16,
    hits: Mutex<Vec<Value>>,
}

async fn track_handler(
    State(s): State<Arc<Srv>>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, &'static str) {
    s.hits.lock().unwrap().push(body);
    (
        axum::http::StatusCode::from_u16(s.status.load(Ordering::SeqCst)).unwrap(),
        "1",
    )
}

async fn start_server(status: u16) -> (Arc<Srv>, String) {
    let srv = Arc::new(Srv {
        status: AtomicU16::new(status),
        hits: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/track", post(track_handler))
        .with_state(Arc::clone(&srv));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (srv, format!("http://{addr}"))
}

async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn with_vars(state: &AppState, host: &str) -> AppState {
    let mut config = (*state.config).clone();
    config
        .var_overrides
        .insert("MIXPANEL_PROJECT_TOKEN".into(), "tok-rahasia".into());
    config
        .var_overrides
        .insert("MIXPANEL_API_HOST".into(), host.into());
    config
        .var_overrides
        .insert("MIXPANEL_ENV".into(), "Test".into());
    let mut s = state.clone();
    s.config = Arc::new(config);
    s
}

fn input(dedup: &str) -> TrackInput {
    TrackInput {
        event: MixpanelEvent::SignUp,
        distinct_id: UID.into(),
        dedup_key: dedup.into(),
        properties: json!({"signup_type": "invite"})
            .as_object()
            .cloned()
            .unwrap(),
        occurred_at_ms: Some(1_700_000_000_000),
    }
}

async fn queue_len(state: &AppState) -> usize {
    queue::len(state, mixpanel::QUEUE_NAME).await.unwrap()
}

#[tokio::test]
async fn mixpanel_lewat_antrean_redis() {
    let Some(base) = redis_state().await else {
        return;
    };
    let _: i64 = base
        .redis
        .next()
        .del(vec![
            queue::due_key(mixpanel::QUEUE_NAME),
            queue::jobs_key(mixpanel::QUEUE_NAME),
        ])
        .await
        .unwrap();

    // 1) Sukses: event terkirim sekali, payload benar, antrean kosong.
    let (srv, host) = start_server(200).await;
    let state = with_vars(&base, &host);
    let worker = mixpanel::spawn_worker(&state);
    mixpanel::track(&state, input("sukses"));
    assert!(wait_until(|| srv.hits.lock().unwrap().len() == 1).await);
    {
        let hits = srv.hits.lock().unwrap();
        let ev = &hits[0][0];
        assert_eq!(ev["event"], "Sign Up");
        let p = &ev["properties"];
        assert_eq!(p["token"], "tok-rahasia");
        assert_eq!(p["distinct_id"], UID);
        assert_eq!(p["time"], 1_700_000_000_000_i64);
        assert_eq!(p["$insert_id"], mixpanel::insert_id("Sign Up", "sukses"));
        assert_eq!(p["env"], "test");
        assert_eq!(p["signup_type"], "invite");
    }
    assert!(wait_until_async(|| async { queue_len(&state).await == 0 }).await);

    // 2) Id (dedup_key) sama saat masih antre tidak menggandakan job.
    srv.status.store(500, Ordering::SeqCst);
    mixpanel::track(&state, input("dobel"));
    mixpanel::track(&state, input("dobel"));
    let id = mixpanel::insert_id("Sign Up", "dobel");
    // Gagal 5xx -> tetap di antrean dengan attempts naik (backoff 30 dtk).
    assert!(
        wait_until_async(|| async {
            queue::get(&state, mixpanel::QUEUE_NAME, &id)
                .await
                .unwrap()
                .is_some_and(|j| j.attempts >= 1)
        })
        .await
    );
    assert_eq!(queue_len(&state).await, 1);

    // 3) Tahan restart: worker mati, job tetap ada; worker baru mengirimnya setelah jatuh tempo.
    worker.abort();
    let hits_before = srv.hits.lock().unwrap().len();
    srv.status.store(200, Ordering::SeqCst);
    let _: i64 = base
        .redis
        .next()
        .zadd(
            queue::due_key(mixpanel::QUEUE_NAME),
            None,
            None,
            false,
            false,
            (0.0_f64, id.as_str()),
        )
        .await
        .unwrap();
    let worker2 = mixpanel::spawn_worker(&state);
    assert!(wait_until_async(|| async { queue_len(&state).await == 0 }).await);
    assert!(srv.hits.lock().unwrap().len() > hits_before);

    // 4) 400 = final: dibuang setelah SATU request.
    srv.status.store(400, Ordering::SeqCst);
    let before = srv.hits.lock().unwrap().len();
    mixpanel::track(&state, input("ditolak"));
    assert!(
        wait_until_async(|| async {
            queue_len(&state).await == 0 && srv.hits.lock().unwrap().len() == before + 1
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(srv.hits.lock().unwrap().len(), before + 1);

    // 5) Token dicabut saat job antre: dibuang diam-diam tanpa request.
    srv.status.store(200, Ordering::SeqCst);
    let no_token = {
        let mut config = (*state.config).clone();
        config
            .var_overrides
            .insert("MIXPANEL_PROJECT_TOKEN".into(), "   ".into());
        let mut s = state.clone();
        s.config = Arc::new(config);
        s
    };
    worker2.abort();
    queue::enqueue(
        &state,
        mixpanel::QUEUE_NAME,
        Some("tanpa-token".into()),
        json!({"event": "Sign Up", "distinctId": UID, "insertId": "tanpa-token", "time": 1, "properties": {}}),
        queue::JobOpts::new(5, Duration::from_secs(30)),
    )
    .await
    .unwrap();
    let before = srv.hits.lock().unwrap().len();
    let worker3 = mixpanel::spawn_worker(&no_token);
    assert!(wait_until_async(|| async { queue_len(&state).await == 0 }).await);
    assert_eq!(srv.hits.lock().unwrap().len(), before);
    // Job tanpa event/distinctId/insertId dibuang.
    queue::enqueue(
        &state,
        mixpanel::QUEUE_NAME,
        Some("rusak".into()),
        json!({"foo": 1}),
        queue::JobOpts::new(5, Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert!(wait_until_async(|| async { queue_len(&state).await == 0 }).await);
    assert_eq!(srv.hits.lock().unwrap().len(), before);
    worker3.abort();

    // 6) Fallback: Redis tidak bisa dipakai (pool tidak pernah tersambung) -> kirim langsung.
    let broken = AppState::for_test().unwrap();
    let broken = with_vars(&broken, &host);
    let before = srv.hits.lock().unwrap().len();
    mixpanel::track(&broken, input("fallback"));
    assert!(wait_until(|| srv.hits.lock().unwrap().len() == before + 1).await);
    assert_eq!(queue_len(&state).await, 0);
}

async fn wait_until_async<F, Fut>(mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}
