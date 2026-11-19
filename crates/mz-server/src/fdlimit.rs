//! Batas file terbuka proses (`RLIMIT_NOFILE`).
//!
//! Satu koneksi klien = satu file descriptor, ditambah pool Postgres dan
//! Redis. Batas lunak bawaan systemd dan sesi login Linux hanya 1024, sehingga
//! saat ±1000 desktop tersambung bersamaan `accept`, koneksi DB, dan Redis
//! gagal dengan "Too many open files" (terlihat di spike test 1000 user).
//! Saat start batas lunak dinaikkan ke batas keras, seperti yang dilakukan Go
//! dan nginx. Tidak butuh hak root; batas keras tetap diatur lewat
//! `LimitNOFILE` (systemd) atau `ulimit -Hn`.

/// Batas atas bila batas keras tak terbatas.
const CAP: u64 = 1 << 20;
/// Di bawah ini server bisa kehabisan file descriptor saat lonjakan.
const LOW: u64 = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Raised { from: u64, to: u64 },
    Unchanged(u64),
    Failed { current: u64, error: String },
    Unsupported,
}

/// Batas lunak baru, atau `None` bila sudah cukup. `None` pada argumen =
/// tak terbatas (`RLIM_INFINITY`).
fn target(soft: Option<u64>, hard: Option<u64>) -> Option<u64> {
    let soft = soft?;
    let want = hard.unwrap_or(CAP).min(CAP);
    (soft < want).then_some(want)
}

#[cfg(unix)]
pub fn raise() -> Outcome {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let lim = getrlimit(Resource::Nofile);
    let from = lim.current.unwrap_or(u64::MAX);
    let Some(to) = target(lim.current, lim.maximum) else {
        return Outcome::Unchanged(from);
    };
    match setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(to),
            maximum: lim.maximum,
        },
    ) {
        Ok(()) => Outcome::Raised { from, to },
        Err(e) => Outcome::Failed {
            current: from,
            error: e.to_string(),
        },
    }
}

#[cfg(not(unix))]
pub fn raise() -> Outcome {
    Outcome::Unsupported
}

/// [`raise`] lalu catat hasilnya; peringatan bila batas akhir masih rendah.
pub fn raise_and_log() {
    let limit = match raise() {
        Outcome::Raised { from, to } => {
            tracing::info!(from, to, "batas file terbuka dinaikkan");
            to
        }
        Outcome::Unchanged(n) => {
            tracing::info!(limit = n, "batas file terbuka");
            n
        }
        Outcome::Failed { current, error } => {
            tracing::warn!(limit = current, %error, "gagal menaikkan batas file terbuka");
            current
        }
        Outcome::Unsupported => return,
    };
    if limit < LOW {
        tracing::warn!(
            limit,
            "batas file terbuka rendah; server bisa gagal (Too many open files) saat banyak klien tersambung. Naikkan batas keras, mis. LimitNOFILE=65535 di unit systemd"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raises_soft_to_hard() {
        assert_eq!(target(Some(1024), Some(524_288)), Some(524_288));
        assert_eq!(target(Some(1024), Some(65_535)), Some(65_535));
    }

    #[test]
    fn caps_unlimited_hard() {
        assert_eq!(target(Some(1024), None), Some(CAP));
        assert_eq!(target(Some(1024), Some(u64::MAX >> 1)), Some(CAP));
    }

    #[test]
    fn leaves_enough_alone() {
        assert_eq!(target(Some(65_535), Some(65_535)), None);
        assert_eq!(target(Some(CAP), None), None);
        assert_eq!(target(None, None), None);
        // Batas keras 1024: tidak ada yang bisa dinaikkan.
        assert_eq!(target(Some(1024), Some(1024)), None);
    }
}
