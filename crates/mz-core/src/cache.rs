//! Cache in-process dengan tiga sifat yang dipakai berulang di Mudah-API:
//!
//! 1. **TTL + salinan basi** — entri punya `fresh_until` (jalur normal) dan
//!    `retain_until` (hanya dibaca saat DB gagal; lihat `token-revocation-cache.ts`).
//! 2. **Single-flight** — miss bersamaan untuk kunci yang sama menunggu SATU fetch
//!    (pengganti peta `inflight` di `RolesGuard`, `PermissionsService`, dst).
//! 3. **Penjaga epoch** — fetch yang DIMULAI sebelum invalidasi tidak boleh
//!    menulis hasilnya ke cache, dan request baru tidak boleh menumpang fetch
//!    lama seperti itu (TOCTOU yang ditutup `role-cache.ts` / `token-revocation-cache.ts`).
//!
//! Cache ini per proses, sama seperti di Node.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

pub type SharedError = Arc<anyhow::Error>;
type Flight<V> = Shared<BoxFuture<'static, Result<V, SharedError>>>;

struct Entry<V> {
    value: V,
    fresh_until: Instant,
    retain_until: Instant,
}

struct Inner<K, V> {
    entries: HashMap<K, Entry<V>>,
    inflight: HashMap<K, (u64, Flight<V>)>,
    epoch: u64,
}

/// Berapa lama hasil fetch disimpan. `None` = jangan disimpan.
#[derive(Debug, Clone, Copy)]
pub struct Keep {
    pub fresh: Duration,
    pub retain: Duration,
}

impl Keep {
    pub const fn ttl(fresh: Duration) -> Self {
        Self {
            fresh,
            retain: fresh,
        }
    }
}

pub struct EpochCache<K, V> {
    inner: Mutex<Inner<K, V>>,
    max_entries: usize,
}

impl<K, V> EpochCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                inflight: HashMap::new(),
                epoch: 0,
            }),
            max_entries,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<K, V>> {
        // Panic di dalam critical section tidak pernah meninggalkan state
        // setengah jadi di sini (semua operasi map atomik), jadi aman dipulihkan.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    pub fn get_fresh(&self, key: &K) -> Option<V> {
        let now = Instant::now();
        let g = self.lock();
        g.entries
            .get(key)
            .filter(|e| e.fresh_until > now)
            .map(|e| e.value.clone())
    }

    /// Mengabaikan TTL segar — khusus jalur DB gagal.
    pub fn get_stale(&self, key: &K) -> Option<V> {
        let now = Instant::now();
        let g = self.lock();
        g.entries
            .get(key)
            .filter(|e| e.retain_until > now)
            .map(|e| e.value.clone())
    }

    /// `started_epoch` = nilai [`Self::epoch`] saat fetch DIMULAI.
    pub fn set(&self, key: K, value: V, started_epoch: u64, keep: Keep) {
        let mut g = self.lock();
        if g.epoch != started_epoch {
            return;
        }
        let now = Instant::now();
        g.entries.insert(
            key,
            Entry {
                value,
                fresh_until: now + keep.fresh,
                retain_until: now + keep.retain.max(keep.fresh),
            },
        );
        if g.entries.len() > self.max_entries {
            // Pagar memori: buang entri yang paling dulu basi. Tergusur hanya
            // berarti satu load lagi, bukan jawaban salah.
            if let Some(oldest) = g
                .entries
                .iter()
                .min_by_key(|(_, e)| e.fresh_until)
                .map(|(k, _)| k.clone())
            {
                g.entries.remove(&oldest);
            }
        }
    }

    /// Hapus entri yang cocok dan naikkan epoch (fetch yang sedang berjalan
    /// tidak akan menulis hasil lamanya).
    pub fn invalidate_where(&self, pred: impl Fn(&K, &V) -> bool) {
        let mut g = self.lock();
        g.epoch += 1;
        g.entries.retain(|k, e| !pred(k, &e.value));
    }

    pub fn invalidate_all(&self) {
        let mut g = self.lock();
        g.epoch += 1;
        g.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn prune(g: &mut Inner<K, V>, now: Instant) {
        g.entries.retain(|_, e| e.retain_until > now);
    }

    /// Ambil dari cache, atau jalankan `fetch` (single-flight per kunci+epoch).
    /// `keep(&v)` menentukan berapa lama hasilnya disimpan.
    pub async fn get_or_load<F, Fut>(
        &self,
        key: K,
        keep: impl Fn(&V) -> Option<Keep>,
        fetch: F,
    ) -> Result<V, SharedError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, SharedError>> + Send + 'static,
    {
        let (flight, started_epoch) = {
            let mut g = self.lock();
            let now = Instant::now();
            if let Some(e) = g.entries.get(&key)
                && e.fresh_until > now
            {
                return Ok(e.value.clone());
            }
            let epoch = g.epoch;
            match g.inflight.get(&key) {
                Some((e, f)) if *e == epoch => (f.clone(), epoch),
                _ => {
                    Self::prune(&mut g, now);
                    let f: Flight<V> = fetch().boxed().shared();
                    g.inflight.insert(key.clone(), (epoch, f.clone()));
                    (f, epoch)
                }
            }
        };

        let result = flight.clone().await;

        {
            let mut g = self.lock();
            let mine = matches!(g.inflight.get(&key), Some((_, f)) if f.ptr_eq(&flight));
            if mine {
                g.inflight.remove(&key);
            }
        }

        if let Ok(value) = &result
            && let Some(k) = keep(value)
        {
            self.set(key, value.clone(), started_epoch, k);
        }
        result
    }
}

pub fn shared_err(err: impl Into<anyhow::Error>) -> SharedError {
    Arc::new(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MIN: Keep = Keep::ttl(Duration::from_secs(60));

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch() {
        let cache: Arc<EpochCache<i32, i32>> = Arc::new(EpochCache::new(100));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let cache = cache.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_load(
                        1,
                        |_| Some(MIN),
                        move || async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(7)
                        },
                    )
                    .await
            }));
        }
        for h in handles {
            assert_eq!(h.await.ok().and_then(|r| r.ok()), Some(7));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidation_during_fetch_is_not_overwritten() {
        let cache: Arc<EpochCache<i32, i32>> = Arc::new(EpochCache::new(100));
        let c2 = cache.clone();
        let load = tokio::spawn(async move {
            c2.get_or_load(
                1,
                |_| Some(MIN),
                || async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(1)
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.invalidate_where(|k, _| *k == 1);
        assert_eq!(load.await.ok().and_then(|r| r.ok()), Some(1));
        // Hasil fetch lama TIDAK boleh masuk cache.
        assert_eq!(cache.get_fresh(&1), None);
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let cache: EpochCache<i32, i32> = EpochCache::new(100);
        let r = cache
            .get_or_load(
                1,
                |_| Some(MIN),
                || async { Err(shared_err(anyhow::anyhow!("x"))) },
            )
            .await;
        assert!(r.is_err());
        let r = cache
            .get_or_load(1, |_| Some(MIN), || async { Ok(2) })
            .await;
        assert_eq!(r.ok(), Some(2));
    }

    #[test]
    fn max_entries_is_enforced() {
        let cache: EpochCache<i32, i32> = EpochCache::new(2);
        for i in 0..5 {
            cache.set(i, i, 0, MIN);
        }
        assert_eq!(cache.len(), 2);
    }
}
