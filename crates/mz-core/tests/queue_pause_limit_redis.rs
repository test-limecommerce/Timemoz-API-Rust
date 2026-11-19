//! Uji integrasi tambahan `mz_core::queue` (jeda, limiter jarak mulai, enqueue massal, clear)
//! terhadap Redis NYATA. Dilewati bila Redis tidak tersedia (lihat `queue_redis.rs`).

#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

mod common;

use common::redis_state;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use fred::prelude::*;
use mz_core::AppState;
use mz_core::queue::{self, JobOpts, Outcome, WorkerConfig};
use serde_json::json;

fn qname() -> String {
    format!("test-{}", uuid::Uuid::new_v4())
}

async fn cleanup(state: &AppState, name: &str) {
    let _: Result<i64, _> = state
        .redis
        .next()
        .del(vec![
            queue::due_key(name),
            queue::jobs_key(name),
            queue::paused_key(name),
            queue::limiter_key(name),
        ])
        .await;
}

fn fast(concurrency: usize) -> WorkerConfig {
    WorkerConfig {
        concurrency,
        lease: Duration::from_secs(30),
        handler_timeout: Duration::from_secs(10),
        poll: Duration::from_millis(20),
    }
}

const OPTS: JobOpts = JobOpts::new(1, Duration::ZERO);

#[tokio::test]
async fn jeda_menahan_klaim_dan_resume_melanjutkan() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    assert!(!queue::is_paused(&state, &name).await.unwrap());
    queue::pause(&state, &name).await.unwrap();
    queue::pause(&state, &name).await.unwrap();
    assert!(queue::is_paused(&state, &name).await.unwrap());
    // Enqueue tetap diterima saat dijeda, tetapi tidak ada yang diserahkan.
    queue::enqueue(&state, &name, Some("a".into()), json!(1), OPTS)
        .await
        .unwrap();
    let lease = Duration::from_secs(30);
    assert!(
        queue::claim(&state, &name, 5, lease)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(queue::waiting_len(&state, &name).await.unwrap(), 1);
    queue::resume(&state, &name).await.unwrap();
    queue::resume(&state, &name).await.unwrap();
    assert!(!queue::is_paused(&state, &name).await.unwrap());
    let jobs = queue::claim(&state, &name, 5, lease).await.unwrap();
    assert_eq!(jobs.len(), 1);
    // Yang sedang di-lease tidak dihitung menunggu.
    assert_eq!(queue::waiting_len(&state, &name).await.unwrap(), 0);
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn clear_waiting_tidak_menyentuh_job_berjalan() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let jobs: Vec<_> = (0..5).map(|i| (format!("j{i}"), json!(i))).collect();
    assert_eq!(
        queue::enqueue_many(&state, &name, jobs, OPTS)
            .await
            .unwrap(),
        5
    );
    // Satu job diklaim (berjalan), empat menunggu.
    let running = queue::claim(&state, &name, 1, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(queue::clear_waiting(&state, &name).await.unwrap(), 4);
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1);
    assert!(
        queue::get(&state, &name, &running[0].id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(queue::clear_waiting(&state, &name).await.unwrap(), 0);
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn enqueue_many_dedup_dan_chunk() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let jobs: Vec<_> = (0..1200).map(|i| (format!("j{i}"), json!(i))).collect();
    assert_eq!(
        queue::enqueue_many(&state, &name, jobs, OPTS)
            .await
            .unwrap(),
        1200
    );
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1200);
    // Ulang: 1100 id lama + 100 baru -> hanya 100 dibuat; payload lama dipertahankan.
    let again: Vec<_> = (100..1300).map(|i| (format!("j{i}"), json!(-1))).collect();
    assert_eq!(
        queue::enqueue_many(&state, &name, again, OPTS)
            .await
            .unwrap(),
        100
    );
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1300);
    let old = queue::get(&state, &name, "j500").await.unwrap().unwrap();
    assert_eq!(old.data, json!(500));
    assert_eq!(
        queue::enqueue_many(&state, &name, vec![], OPTS)
            .await
            .unwrap(),
        0
    );
    assert!(
        queue::enqueue_many(&state, &name, vec![(String::new(), json!(1))], OPTS)
            .await
            .is_err()
    );
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn limiter_satu_job_per_jarak_dan_worker_menghormatinya() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let jobs: Vec<_> = (0..3).map(|i| (format!("j{i}"), json!(i))).collect();
    queue::enqueue_many(&state, &name, jobs, OPTS)
        .await
        .unwrap();

    // Klaim langsung: limiter membatasi 1 job walau limit 10, klaim kedua ditahan.
    let lease = Duration::from_secs(30);
    let spacing = Some(Duration::from_millis(400));
    let first = queue::claim_limited(&state, &name, 10, lease, spacing)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert!(
        queue::claim_limited(&state, &name, 10, lease, spacing)
            .await
            .unwrap()
            .is_empty()
    );
    // Tanpa limiter (claim biasa) tidak terpengaruh kunci limiter.
    let free = queue::claim(&state, &name, 10, lease).await.unwrap();
    assert_eq!(free.len(), 2);
    cleanup(&state, &name).await;

    // Worker: 3 job, jarak 300 ms -> mulai ke-3 paling cepat ~600 ms sesudah ke-1.
    let jobs: Vec<_> = (0..3).map(|i| (format!("j{i}"), json!(i))).collect();
    queue::enqueue_many(&state, &name, jobs, OPTS)
        .await
        .unwrap();
    let starts = Arc::new(std::sync::Mutex::new(Vec::<Instant>::new()));
    let done = Arc::new(AtomicU32::new(0));
    let (s2, d2) = (Arc::clone(&starts), Arc::clone(&done));
    let worker = queue::spawn_worker_limited(
        &state,
        &name,
        fast(3),
        Some(Duration::from_millis(300)),
        move |_job| {
            let (s2, d2) = (Arc::clone(&s2), Arc::clone(&d2));
            async move {
                s2.lock().unwrap().push(Instant::now());
                d2.fetch_add(1, Ordering::SeqCst);
                Outcome::Done
            }
        },
    );
    let end = Instant::now() + Duration::from_secs(10);
    while done.load(Ordering::SeqCst) < 3 && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    worker.abort();
    let starts = starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 3);
    for w in starts.windows(2) {
        assert!(
            w[1].duration_since(w[0]) >= Duration::from_millis(280),
            "jarak mulai terlalu pendek: {:?}",
            w[1].duration_since(w[0])
        );
    }
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn worker_berhenti_saat_dijeda_dan_lanjut_saat_resume() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    queue::pause(&state, &name).await.unwrap();
    let done = Arc::new(AtomicU32::new(0));
    let d2 = Arc::clone(&done);
    let worker = queue::spawn_worker_with(&state, &name, fast(2), move |_job| {
        let d2 = Arc::clone(&d2);
        async move {
            d2.fetch_add(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    queue::enqueue(&state, &name, None, json!(1), OPTS)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(done.load(Ordering::SeqCst), 0);
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1);
    queue::resume(&state, &name).await.unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while done.load(Ordering::SeqCst) < 1 && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(done.load(Ordering::SeqCst), 1);
    worker.abort();
    cleanup(&state, &name).await;
}
