//! Cek apakah sqlx — dengan opsi koneksi yang sama seperti server
//! ([`mz_core::db::connect_options`], cache prepared statement aktif) — aman
//! lewat pgbouncer transaction mode.
//!
//! ```sh
//! cargo run --release -p mz-core --example pgbouncer_check
//! # URL ditanya; atau: DATABASE_URL='postgresql://…:25061/pool?sslmode=require' cargo run …
//! ```
//!
//! Aman untuk produksi: hanya `SELECT` konstanta, tanpa tabel.
//!
//! Tahap 1 — koneksi baru berurutan. Setiap koneksi sqlx menamai statement dari
//! `sqlx_s_1` lagi. pgbouncer tanpa dukungan prepared statement memberi koneksi
//! server yang sama ke klien berikutnya, sehingga muncul `already exists`.
//! Tahap 2 — pool (`RS_DB_MAX_CONNECTIONS`, bawaan 20), ratusan query paralel
//! di dalam dan di luar transaksi, seperti beban server sungguhan.

use std::io::Write as _;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

const QUERIES: [&str; 3] = [
    "SELECT $1::int4 + 0 AS v",
    "SELECT $1::int4 + 1 AS v",
    "SELECT ($1::int4)::text AS v",
];

#[derive(Default)]
struct Tally {
    ok: u32,
    prepared_err: u32,
    other_err: u32,
    first_err: Option<String>,
}

impl Tally {
    fn record(&mut self, r: Result<(), sqlx::Error>) {
        match r {
            Ok(()) => self.ok += 1,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("prepared statement") || msg.contains("portal") {
                    self.prepared_err += 1;
                } else {
                    self.other_err += 1;
                }
                self.first_err.get_or_insert(msg);
            }
        }
    }

    fn merge(&mut self, o: Tally) {
        self.ok += o.ok;
        self.prepared_err += o.prepared_err;
        self.other_err += o.other_err;
        if self.first_err.is_none() {
            self.first_err = o.first_err;
        }
    }

    fn line(&self) -> String {
        format!(
            "{} ok · {} error prepared statement · {} error lain",
            self.ok, self.prepared_err, self.other_err
        )
    }
}

async fn run_queries(conn: &mut PgConnection, n: i32) -> Result<(), sqlx::Error> {
    for q in QUERIES {
        sqlx::query(q).bind(n).fetch_one(&mut *conn).await?;
    }
    Ok(())
}

async fn stage_fresh_connections(opts: &PgConnectOptions, runs: i32) -> anyhow::Result<Tally> {
    let mut t = Tally::default();
    for i in 1..=runs {
        let mut conn = PgConnection::connect_with(opts).await?;
        t.record(run_queries(&mut conn, i).await);
        let _ = conn.close().await;
    }
    Ok(t)
}

async fn one_pool_task(pool: PgPool, i: i32) -> Tally {
    let mut t = Tally::default();
    // Di luar transaksi: tiap query bisa mendarat di koneksi server berbeda.
    let r = async {
        let mut conn = pool.acquire().await?;
        run_queries(&mut conn, i).await
    }
    .await;
    t.record(r);
    // Di dalam transaksi: koneksi server terkunci sampai COMMIT.
    let r = async {
        let mut tx = pool.begin().await?;
        run_queries(&mut tx, i).await?;
        tx.commit().await
    }
    .await;
    t.record(r);
    t
}

async fn stage_pool(opts: &PgConnectOptions, max_conn: u32, tasks: i32) -> anyhow::Result<Tally> {
    let pool = PgPoolOptions::new()
        .max_connections(max_conn)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts.clone())
        .await?;
    let handles: Vec<_> = (1..=tasks)
        .map(|i| tokio::spawn(one_pool_task(pool.clone(), i)))
        .collect();
    let mut t = Tally::default();
    for h in handles {
        t.merge(h.await?);
    }
    pool.close().await;
    Ok(t)
}

fn read_url() -> anyhow::Result<String> {
    if let Ok(u) = std::env::var("DATABASE_URL")
        && !u.trim().is_empty()
    {
        return Ok(u);
    }
    print!("DATABASE_URL pool (postgresql://user:pass@host:25061/pool?sslmode=require): ");
    std::io::stdout().flush()?;
    let mut u = String::new();
    std::io::stdin().read_line(&mut u)?;
    let u = u.trim().to_owned();
    anyhow::ensure!(!u.is_empty(), "DATABASE_URL kosong");
    Ok(u)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = read_url()?;
    let max_conn = std::env::var("RS_DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    // Opsi koneksi persis seperti server (`AppState::connect`).
    let opts = mz_core::db::connect_options(&url)?;

    println!("Tahap 1: 20 koneksi baru berurutan …");
    let t1 = stage_fresh_connections(&opts, 20).await?;
    println!("  {}", t1.line());

    println!("Tahap 2: pool {max_conn} koneksi, 300 tugas paralel (600 transaksi) …");
    let t2 = stage_pool(&opts, max_conn, 300).await?;
    println!("  {}", t2.line());

    let mut all = t1;
    all.merge(t2);
    println!();
    if all.prepared_err == 0 && all.other_err == 0 {
        println!(
            "=> AMAN: server Rust (sqlx, cache prepared statement aktif) bisa memakai URL ini."
        );
        return Ok(());
    }
    if all.prepared_err > 0 {
        println!("=> TIDAK AMAN: pgbouncer tidak melacak prepared statement.");
        println!("   Rust perlu pool mode session atau koneksi langsung.");
    } else {
        println!("=> Gagal karena hal lain (koneksi/SSL/password/timeout?).");
    }
    if let Some(e) = all.first_err {
        println!("   Error pertama: {e}");
    }
    std::process::exit(1);
}
