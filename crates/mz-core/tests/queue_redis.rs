//! Uji integrasi `mz_core::queue` terhadap Redis NYATA (localhost:6379, DB 15 seperti `mz-testkit`).
//! Dilewati (`SKIP`) bila Redis tidak tersedia, kecuali `MZ_TEST_REQUIRE_DB=1`.
//! Setiap test memakai nama antrean unik (UUID), jadi aman dijalankan paralel.

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
        .del(vec![queue::due_key(name), queue::jobs_key(name)])
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

async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Majukan jam: job langsung jatuh tempo.
async fn make_due(state: &AppState, name: &str, id: &str) {
    let _: i64 = state
        .redis
        .next()
        .zadd(
            queue::due_key(name),
            None,
            None,
            false,
            false,
            (0.0_f64, id),
        )
        .await
        .unwrap();
}

async fn zscore(state: &AppState, name: &str, id: &str) -> Option<f64> {
    state
        .redis
        .next()
        .zscore(queue::due_key(name), id)
        .await
        .unwrap()
}

#[tokio::test]
async fn enqueue_diproses_worker_lalu_hilang_dari_redis() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let seen = Arc::new(AtomicU32::new(0));
    let s2 = Arc::clone(&seen);
    let worker = queue::spawn_worker_with(&state, &name, fast(3), move |job| {
        let s2 = Arc::clone(&s2);
        async move {
            assert_eq!(job.attempt, 1);
            assert_eq!(job.data["n"], 7);
            s2.fetch_add(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    let id = queue::enqueue(
        &state,
        &name,
        None,
        json!({"n": 7}),
        JobOpts::new(3, Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert!(!id.is_empty());
    assert!(
        wait_until(Duration::from_secs(5), || async {
            seen.load(Ordering::SeqCst) == 1 && queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    assert_eq!(zscore(&state, &name, &id).await, None);
    assert_eq!(queue::get(&state, &name, &id).await.unwrap(), None);
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn id_kembar_tidak_dobel() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let opts = JobOpts::new(3, Duration::from_millis(10));
    for i in 0..3 {
        let id = queue::enqueue(&state, &name, Some("sama".into()), json!({"i": i}), opts)
            .await
            .unwrap();
        assert_eq!(id, "sama");
    }
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1);
    let card: usize = state
        .redis
        .next()
        .zcard(queue::due_key(&name))
        .await
        .unwrap();
    assert_eq!(card, 1);
    // Payload pertama dipertahankan.
    let stored = queue::get(&state, &name, "sama").await.unwrap().unwrap();
    assert_eq!(stored.data, json!({"i": 0}));
    assert_eq!(stored.max_attempts, 3);
    assert_eq!(stored.backoff_ms, 10);
    assert_eq!(stored.attempts, 0);

    let jobs = queue::claim(&state, &name, 10, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    // Enqueue ulang saat sedang diproses pun tidak membuat job kedua.
    queue::enqueue(&state, &name, Some("sama".into()), json!({"i": 9}), opts)
        .await
        .unwrap();
    assert_eq!(queue::len(&state, &name).await.unwrap(), 1);
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn retry_naik_attempts_backoff_eksponensial_dan_berhenti_di_max() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let id = queue::enqueue(
        &state,
        &name,
        Some("j".into()),
        json!("x"),
        JobOpts::new(3, Duration::from_secs(10)),
    )
    .await
    .unwrap();

    // Percobaan 1 -> retry dengan jeda 10 dtk.
    let lease = Duration::from_secs(30);
    let mut jobs = queue::claim(&state, &name, 5, lease).await.unwrap();
    assert_eq!(jobs.len(), 1);
    let job = jobs.remove(0);
    assert_eq!((job.attempt, job.max_attempts), (1, 3));
    let t0 = chrono::Utc::now().timestamp_millis() as f64;
    queue::retry(
        &state,
        &name,
        &job,
        queue::backoff_delay_ms(job.backoff_ms, job.attempt),
    )
    .await
    .unwrap();
    let score = zscore(&state, &name, &id).await.unwrap();
    assert!(
        (score - t0 - 10_000.0).abs() < 2_000.0,
        "score {score} t0 {t0}"
    );
    assert_eq!(
        queue::get(&state, &name, &id)
            .await
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    // Belum jatuh tempo -> tidak bisa diklaim.
    assert!(
        queue::claim(&state, &name, 5, lease)
            .await
            .unwrap()
            .is_empty()
    );

    // Percobaan 2 -> jeda 20 dtk.
    make_due(&state, &name, &id).await;
    let mut jobs = queue::claim(&state, &name, 5, lease).await.unwrap();
    assert_eq!(jobs.len(), 1);
    let job = jobs.remove(0);
    assert_eq!(job.attempt, 2);
    let t1 = chrono::Utc::now().timestamp_millis() as f64;
    queue::retry(
        &state,
        &name,
        &job,
        queue::backoff_delay_ms(job.backoff_ms, job.attempt),
    )
    .await
    .unwrap();
    let score = zscore(&state, &name, &id).await.unwrap();
    assert!(
        (score - t1 - 20_000.0).abs() < 2_000.0,
        "score {score} t1 {t1}"
    );
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn worker_retry_sampai_max_attempts_lalu_dihapus() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&calls);
    let worker = queue::spawn_worker_with(&state, &name, fast(2), move |job| {
        let c2 = Arc::clone(&c2);
        async move {
            let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(job.attempt, n, "attempt naik 1,2,3");
            Outcome::Retry("gagal terus".into())
        }
    });
    let id = queue::enqueue(
        &state,
        &name,
        None,
        json!(1),
        JobOpts::new(3, Duration::from_millis(30)),
    )
    .await
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || async {
            queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    // Beri waktu bila ada eksekusi liar ke-4.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(zscore(&state, &name, &id).await, None);
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn drop_menghapus_tanpa_retry() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&calls);
    let worker = queue::spawn_worker_with(&state, &name, fast(2), move |_job| {
        let c2 = Arc::clone(&c2);
        async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Outcome::Drop("ditolak".into())
        }
    });
    queue::enqueue(
        &state,
        &name,
        None,
        json!(1),
        JobOpts::new(5, Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || async {
            queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn panic_handler_dianggap_retry() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&calls);
    let worker = queue::spawn_worker_with(&state, &name, fast(2), move |_job| {
        let c2 = Arc::clone(&c2);
        async move {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("boom");
            }
            Outcome::Done
        }
    });
    queue::enqueue(
        &state,
        &name,
        None,
        json!(1),
        JobOpts::new(3, Duration::from_millis(20)),
    )
    .await
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || async {
            calls.load(Ordering::SeqCst) == 2 && queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn timeout_handler_dianggap_retry() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&calls);
    let mut cfg = fast(2);
    cfg.handler_timeout = Duration::from_millis(100);
    let worker = queue::spawn_worker_with(&state, &name, cfg, move |_job| {
        let c2 = Arc::clone(&c2);
        async move {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Outcome::Done
        }
    });
    queue::enqueue(
        &state,
        &name,
        None,
        json!(1),
        JobOpts::new(3, Duration::from_millis(20)),
    )
    .await
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || async {
            calls.load(Ordering::SeqCst) == 2 && queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn job_terklaim_tanpa_ack_diambil_lagi_setelah_lease_habis() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    queue::enqueue(
        &state,
        &name,
        Some("mati".into()),
        json!(1),
        JobOpts::new(3, Duration::from_secs(1)),
    )
    .await
    .unwrap();
    // "Worker" klaim dengan lease pendek lalu mati (tidak ack).
    let lease = Duration::from_millis(400);
    let first = queue::claim(&state, &name, 5, lease).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempt, 1);
    // Selama lease berjalan job tidak bisa diklaim lagi.
    assert!(
        queue::claim(&state, &name, 5, lease)
            .await
            .unwrap()
            .is_empty()
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = queue::claim(&state, &name, 5, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, "mati");
    // Lease yang kedaluwarsa tidak menghabiskan jatah percobaan.
    assert_eq!(second[0].attempt, 1);
    queue::ack(&state, &name, "mati").await.unwrap();
    assert_eq!(queue::len(&state, &name).await.unwrap(), 0);
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn worker_baru_mengambil_job_yang_ditinggal_worker_mati() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    queue::enqueue(
        &state,
        &name,
        Some("x".into()),
        json!(1),
        JobOpts::new(3, Duration::from_secs(1)),
    )
    .await
    .unwrap();
    let _ = queue::claim(&state, &name, 1, Duration::from_millis(300))
        .await
        .unwrap();
    let done = Arc::new(AtomicU32::new(0));
    let d2 = Arc::clone(&done);
    let worker = queue::spawn_worker_with(&state, &name, fast(1), move |_job| {
        let d2 = Arc::clone(&d2);
        async move {
            d2.fetch_add(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    assert!(
        wait_until(Duration::from_secs(5), || async {
            done.load(Ordering::SeqCst) == 1 && queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    worker.abort();
    cleanup(&state, &name).await;
}

#[tokio::test]
async fn concurrency_dibatasi_dan_batch_penuh_diproses_tanpa_menunggu_poll() {
    let Some(state) = redis_state().await else {
        return;
    };
    let name = qname();
    for i in 0..12 {
        queue::enqueue(
            &state,
            &name,
            Some(format!("j{i}")),
            json!(i),
            JobOpts::new(1, Duration::ZERO),
        )
        .await
        .unwrap();
    }
    let running = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let (r2, p2) = (Arc::clone(&running), Arc::clone(&peak));
    let mut cfg = fast(3);
    // Poll sangat lama: batch penuh harus langsung diulang tanpa menunggu poll.
    // Sisa job (< batch) baru menunggu poll, jadi 12 = 4 batch penuh habis semuanya.
    cfg.poll = Duration::from_secs(30);
    let worker = queue::spawn_worker_with(&state, &name, cfg, move |_job| {
        let (r2, p2) = (Arc::clone(&r2), Arc::clone(&p2));
        async move {
            let now = r2.fetch_add(1, Ordering::SeqCst) + 1;
            p2.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            r2.fetch_sub(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    assert!(
        wait_until(Duration::from_secs(5), || async {
            queue::len(&state, &name).await.unwrap() == 0
        })
        .await
    );
    let peak = peak.load(Ordering::SeqCst);
    assert!((2..=3).contains(&peak), "peak {peak}");
    worker.abort();
    cleanup(&state, &name).await;
}
