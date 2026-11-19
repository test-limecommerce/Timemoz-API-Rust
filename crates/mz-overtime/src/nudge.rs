//! Nudge recalc lembur — pengganti `OvertimeJobsService.nudge` (`overtime-jobs.service.ts`)
//! + `src/tracking-timelines/overtime-nudge-dates.ts`.
//!
//! Node: `queue.add('overtime.recalc', …, { jobId: 'overtime-recalc:<c>:<u>:<date>:<menit>',
//! removeOnComplete: true, attempts: 1, timeout: 60 dtk })`, diproses satu per satu
//! (`concurrency: 1`). Rust: klaim Redis `mz:job:overtime-recalc:<…>` (`claim_once`,
//! setara jobId), `mz_core::bg::spawn`, satu jalur recalc per proses (semaphore 1), timeout
//! 60 dtk; kunci klaim dihapus setelah sukses (setara `removeOnComplete`), dibiarkan
//! kedaluwarsa bila gagal (job gagal Bull tetap menahan jobId-nya).
//!
//! Aktif hanya bila `MZ_JOBS` memuat `overtime-recalc` atau `all`. Selama Node masih
//! jalan, nudge dari jalur Rust tidak dikirim dan tick per jam Node menutup celahnya.

use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use fred::prelude::KeysInterface;
use mz_core::AppState;
use tokio::sync::Semaphore;

use crate::zone::{date_key, local_date, local_day_start_ms, matches_date_pattern, zone_or_utc};

/// Nama yang dicek di `MZ_JOBS`.
pub const RECALC_JOB_NAME: &str = "overtime-recalc";
/// `OVERTIME_RECALC_TIMEOUT_MS`.
pub const OVERTIME_RECALC_TIMEOUT: Duration = Duration::from_millis(60_000);
/// Umur kunci dedup: menit bucket + batas waktu job.
const CLAIM_TTL: Duration = Duration::from_secs(180);
/// Batas pengaman `localDateKeysCovered`.
const MAX_DAYS: usize = 400;

/// `concurrency: 1` processor recalc (per proses).
static RECALC_LANE: Semaphore = Semaphore::const_new(1);

/// Interval `[start, end)` epoch ms (`NudgeInterval`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NudgeInterval {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl NudgeInterval {
    pub const fn new(start_ms: i64, end_ms: i64) -> Self {
        Self { start_ms, end_ms }
    }

    /// `TrackingTimelineService.rowInterval(row)`: `timelineInstant` kedua ujung.
    pub fn of_row(
        start_at: NaiveDateTime,
        end_at: NaiveDateTime,
        start_at_utc: Option<NaiveDateTime>,
        end_at_utc: Option<NaiveDateTime>,
        timezone: Option<&str>,
    ) -> Self {
        Self {
            start_ms: crate::zone::timeline_instant(start_at, start_at_utc, timezone),
            end_ms: crate::zone::timeline_instant(end_at, end_at_utc, timezone),
        }
    }

    /// `rowInterval({ startAt: new Date(x), startAtUtc: new Date(x), …, timezone })` untuk
    /// nilai DTO (create/update): zona dikenal → instant apa adanya; selain itu jam dinding
    /// Jakarta.
    pub fn of_input(start_ms: i64, end_ms: i64, timezone: Option<&str>) -> Self {
        let at = |ms: i64| crate::zone::ndt(ms);
        Self::of_row(
            at(start_ms),
            at(end_ms),
            Some(at(start_ms)),
            Some(at(end_ms)),
            timezone,
        )
    }
}

/// Nudge aktif di proses ini? (`MZ_JOBS` memuat `overtime-recalc` atau `all`.)
pub fn recalc_enabled(state: &AppState) -> bool {
    mz_jobs::enabled_names(state)
        .iter()
        .any(|n| n == "all" || n == RECALC_JOB_NAME)
}

/// Normalisasi `nudge`: unik, cocok `/^\d{4}-\d{2}-\d{2}$/`, terurut.
pub fn normalize_date_keys(date_keys: Vec<String>) -> Vec<String> {
    let mut keys: Vec<String> = date_keys
        .into_iter()
        .filter(|d| matches_date_pattern(d))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Kunci dedup = jobId Node dengan prefix `mz:job:`.
pub fn recalc_claim_key(company_id: i32, user_id: i32, date: &str, minute_bucket: &str) -> String {
    format!("mz:job:overtime-recalc:{company_id}:{user_id}:{date}:{minute_bucket}")
}

/// `OvertimeJobsService.nudge`: jadwalkan recalc per tanggal. Tidak pernah gagal ke
/// pemanggil dan tidak perlu di-await.
pub fn nudge(state: &AppState, company_id: i32, user_id: i32, date_keys: Vec<String>) {
    if company_id <= 0 || user_id <= 0 || !recalc_enabled(state) {
        return;
    }
    let keys = normalize_date_keys(date_keys);
    if keys.is_empty() {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::warn!("Gagal menjadwalkan recalc lembur: tidak ada runtime tokio");
        return;
    }
    let bucket = Utc::now().format("%Y%m%d%H%M").to_string();
    for date in keys {
        let state = state.clone();
        let claim = recalc_claim_key(company_id, user_id, &date, &bucket);
        mz_core::bg::spawn(async move {
            match mz_jobs::claim_once(&state, &claim, CLAIM_TTL).await {
                Ok(true) => {}
                Ok(false) => return, // jobId sama masih antre/berjalan/gagal → dedup.
                Err(e) => {
                    tracing::warn!("Gagal enqueue overtime.recalc ({claim}): {e}");
                    return;
                }
            }
            run_recalc(&state, company_id, user_id, date, &claim).await;
        });
    }
}

async fn run_recalc(state: &AppState, company_id: i32, user_id: i32, date: String, claim: &str) {
    let Ok(_permit) = RECALC_LANE.acquire().await else {
        return;
    };
    let now = mz_core::time::now_naive();
    let job =
        crate::tick::handle_recalc_at(state, company_id, user_id, std::slice::from_ref(&date), now);
    match tokio::time::timeout(OVERTIME_RECALC_TIMEOUT, job).await {
        Ok(Ok(())) => {
            if let Err(e) = state.redis.next().del::<(), _>(claim).await {
                tracing::warn!("overtime.recalc ({claim}): gagal hapus kunci dedup: {e}");
            }
        }
        Ok(Err(e)) => tracing::error!("overtime.recalc ({claim}) gagal: {e:#}"),
        Err(_) => {
            tracing::error!("overtime.recalc ({claim}) timeout setelah {OVERTIME_RECALC_TIMEOUT:?}")
        }
    }
}

/// `localDateKeysCovered(intervals, timeZone)`: tanggal lokal yang disentuh tiap
/// `[start, end)`. Lintas tengah malam lokal → 2 tanggal; interval kosong/terbalik tetap
/// menyumbang tanggal `start`. Hasil unik dan terurut.
pub fn local_date_keys_covered(
    intervals: &[NudgeInterval],
    time_zone: Option<&str>,
) -> Vec<String> {
    let tz = zone_or_utc(time_zone);
    let mut keys: Vec<String> = Vec::new();
    for iv in intervals {
        let mut day = local_date(iv.start_ms, tz);
        keys.push(date_key(day));
        for _ in 0..MAX_DAYS {
            let Some(next) = day.succ_opt() else { break };
            day = next;
            if local_day_start_ms(day, tz) >= iv.end_ms {
                break;
            }
            keys.push(date_key(day));
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NudgeTarget {
    pub company_id: i32,
    pub time_zone: String,
}

/// `loadNudgeCompany`: company default pemilik (aturan `findCompanyId`) + zonanya.
async fn load_nudge_company(state: &AppState, user_id: i32) -> Option<NudgeTarget> {
    let sql = format!(
        r#"SELECT uc."companyId", c."timeZone" FROM "userCompany" uc
           LEFT JOIN company c ON c.id = uc."companyId"
           WHERE {} LIMIT 1"#,
        mz_core::company::USER_COMPANY_SCOPE_SQL
    );
    let row: Option<(i32, Option<String>)> = sqlx::query_as(&sql)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    row.map(|(company_id, tz)| NudgeTarget {
        company_id,
        time_zone: tz.unwrap_or_else(|| "UTC".to_owned()),
    })
}

/// `loadNudgeCompanies`: company project timeline tempat pemilik anggota aktif; tanpa
/// project → company default. Gagal baca → kosong.
pub async fn load_nudge_companies(
    state: &AppState,
    user_id: i32,
    project_ids: &[Option<i32>],
) -> Vec<NudgeTarget> {
    let mut ids: Vec<i32> = Vec::new();
    for id in project_ids.iter().flatten() {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    if ids.is_empty() {
        return load_nudge_company(state, user_id)
            .await
            .into_iter()
            .collect();
    }
    let links: Vec<(i32, Option<String>)> = match sqlx::query_as(
        r#"SELECT pc."companyId", c."timeZone" FROM "projectCompany" pc
           JOIN company c ON c.id = pc."companyId"
           WHERE pc."projectId" = ANY($1)
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."companyId" = c.id AND uc."userId" = $2 AND uc.status = 1)
           ORDER BY pc.id"#,
    )
    .bind(&ids)
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut targets: Vec<NudgeTarget> = Vec::new();
    for (company_id, tz) in links {
        let time_zone = tz.unwrap_or_else(|| "UTC".to_owned());
        match targets.iter_mut().find(|t| t.company_id == company_id) {
            Some(t) => t.time_zone = time_zone,
            None => targets.push(NudgeTarget {
                company_id,
                time_zone,
            }),
        }
    }
    targets
}

/// `nudgeOvertimeForTimelines`: dipanggil SETELAH commit, fire-and-forget, tidak pernah
/// gagal ke pemanggil. Tanggal = tanggal lokal tiap company yang disentuh interval.
pub fn nudge_for_timelines(
    state: &AppState,
    owner_id: Option<i32>,
    project_ids: Vec<Option<i32>>,
    intervals: Vec<NudgeInterval>,
) {
    let Some(owner) = owner_id.filter(|o| *o != 0) else {
        return;
    };
    if intervals.is_empty() || !recalc_enabled(state) {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let state = state.clone();
    mz_core::bg::spawn(async move {
        for target in load_nudge_companies(&state, owner, &project_ids).await {
            nudge(
                &state,
                target.company_id,
                owner,
                local_date_keys_covered(&intervals, Some(&target.time_zone)),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        mz_core::time::parse_js_date(s).map_or(0, crate::zone::ms_of)
    }

    fn iv(a: &str, b: &str) -> NudgeInterval {
        NudgeInterval::new(ms(a), ms(b))
    }

    #[test]
    fn covered_dates() {
        assert_eq!(
            local_date_keys_covered(
                &[iv("2026-10-20T01:00:00Z", "2026-10-20T05:00:00Z")],
                Some("UTC")
            ),
            ["2026-10-20"]
        );
        assert_eq!(
            local_date_keys_covered(
                &[iv("2026-10-20T16:00:00Z", "2026-10-20T18:00:00Z")],
                Some("Asia/Jakarta")
            ),
            ["2026-10-20", "2026-10-21"]
        );
        assert_eq!(
            local_date_keys_covered(
                &[iv("2026-10-20T20:00:00Z", "2026-10-21T00:00:00Z")],
                Some("UTC")
            ),
            ["2026-10-20"]
        );
        assert_eq!(
            local_date_keys_covered(
                &[
                    iv("2026-10-20T01:00:00Z", "2026-10-20T02:00:00Z"),
                    iv("2026-10-20T03:00:00Z", "2026-10-21T03:00:00Z"),
                ],
                Some("Bukan/Zona"),
            ),
            ["2026-10-20", "2026-10-21"]
        );
        // Interval terbalik tetap menyumbang tanggal start.
        assert_eq!(
            local_date_keys_covered(&[iv("2026-10-20T05:00:00Z", "2026-10-20T01:00:00Z")], None),
            ["2026-10-20"]
        );
    }

    #[test]
    fn normalize_like_node() {
        let keys = normalize_date_keys(vec![
            "2026-10-23".into(),
            "2026-10-22".into(),
            "2026-10-23".into(),
            "bukan-tanggal".into(),
            "2026/10/22".into(),
        ]);
        assert_eq!(keys, ["2026-10-22", "2026-10-23"]);
        assert_eq!(
            recalc_claim_key(3, 42, "2026-10-22", "202610220745"),
            "mz:job:overtime-recalc:3:42:2026-10-22:202610220745"
        );
    }

    #[test]
    fn input_interval_legacy_zone() {
        // Zona kosong → jam dinding Jakarta (FR-028).
        let i =
            NudgeInterval::of_input(ms("2026-10-20T09:00:00Z"), ms("2026-10-20T10:00:00Z"), None);
        assert_eq!(i, iv("2026-10-20T02:00:00Z", "2026-10-20T03:00:00Z"));
        let u = NudgeInterval::of_input(
            ms("2026-10-20T09:00:00Z"),
            ms("2026-10-20T10:00:00Z"),
            Some("Asia/Jakarta"),
        );
        assert_eq!(u, iv("2026-10-20T09:00:00Z", "2026-10-20T10:00:00Z"));
    }
}
