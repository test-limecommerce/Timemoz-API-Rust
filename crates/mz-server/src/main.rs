//! Binary `mudah-rs`.

use std::net::SocketAddr;

use mz_core::AppState;
use mz_core::config::Config;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _guard = mz_server::init_tracing();
    mz_server::fdlimit::raise_and_log();

    let config = Config::from_env()?;
    let port = config.port;
    let state = AppState::connect(config).await?;

    // `mudah-rs job <nama>`: jalankan satu cron sekali (tanpa dedup slot), lalu keluar.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [cmd, name] = args.as_slice()
        && cmd == "job"
    {
        let jobs = mz_server::jobs::registry();
        let Some(job) = jobs.iter().find(|j| j.name == name) else {
            anyhow::bail!(
                "job tidak dikenal: {name}. Pilihan: {}",
                mz_server::jobs::names().join(", ")
            );
        };
        let result = mz_jobs::run_now(&state, job).await;
        // Email dkk. yang di-spawn job ini harus selesai sebelum proses keluar.
        mz_core::bg::drain(mz_core::bg::drain_timeout(&state.config)).await;
        return result;
    }

    // Node `BuiltInMatrixCheck` (onApplicationBootstrap): hasilnya muncul di `/health`.
    mz_server::health::check_builtin_matrix(&state).await;
    let _invalidation = mz_core::invalidation::spawn_subscriber(&state);
    let _jobs = mz_jobs::spawn(&state, &mz_server::jobs::registry(), mz_server::jobs::GATES);
    // Antrean Mixpanel tahan restart (Redis `mz:q:mixpanel:*`). Selalu jalan: hanya diisi
    // Rust, terpisah dari Bull `analytics` Node. Job yang terputus saat shutdown diambil
    // ulang setelah lease habis.
    let _mixpanel = mz_core::mixpanel::spawn_worker(&state);
    // Antrean `admin-jobs` milik Rust (concurrency 1) + reaper; job yang dibuat Node tetap
    // diproses processor Bull Node.
    mz_admin_jobs::jobs::worker::register_apply_holidays(|s, p, log| {
        Box::pin(async move { mz_leave_holidays::run_apply_holidays_job(&s, &p, &*log).await })
    });
    let _admin_jobs = mz_admin_jobs::spawn_worker(&state);
    // Antrean `screenshot` (compress/purge) tahan restart di Redis `mz:q:screenshot:*`, dengan
    // limiter 1 job / 3 dtk dan jeda/lanjut lewat `admin/screenshot/queue/*`. Cron + route
    // admin hanya meng-enqueue; job yang terputus diambil ulang setelah lease habis.
    let _screenshots = mz_screenshots::jobs::spawn_worker(&state);
    // Antrean `leave-schedule` (pengganti Bull Node: holiday sync, pengingat, kalender cuti).
    let _leave = mz_leave::spawn_worker(&state);
    // Antrean `product-import` (pengganti Bull Node; concurrency 1, timeout 15 menit).
    let _product_import = mz_products::spawn_worker(&state);
    // Laporan monitoring Discord (Node `MonitoringReportService`). Digerbang
    // `MONITORING_CRON_ENABLED=true` + `DISCORD_MONITORING_WEBHOOK` seperti Node, BUKAN `MZ_JOBS`.
    // Hanya di sini (bukan di `mz_server::app`), jadi uji yang membangun app tidak menyalakannya.
    let monitoring = mz_misc::monitoring::spawn(&state);
    let app = mz_server::app(&state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "mudah-rs listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    // SIGTERM (deploy Docker) atau Ctrl+C: berhenti menerima request, tunggu request
    // yang sedang jalan, lalu tunggu tugas latar (email, flush audit, GA, …) sampai
    // `SHUTDOWN_DRAIN_SECS` — harus di bawah `stop_grace_period` Docker (bawaan 10 dtk).
    .with_graceful_shutdown(mz_core::bg::shutdown_signal())
    .await?;
    tracing::info!("mudah-rs berhenti menerima request");
    // Node `onModuleDestroy`: hentikan timer monitoring sebelum menunggu tugas latar.
    if let Some(m) = &monitoring {
        m.stop();
    }
    mz_core::bg::drain(mz_core::bg::drain_timeout(&state.config)).await;
    Ok(())
}
