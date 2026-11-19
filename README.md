# mudah-rs (rust-timemoz-api)

Port Rust (axum + sqlx + Tokio) dari **Mudah-API** (NestJS), backend Timemoz.
Kontrak HTTP sama persis dengan Node, jadi dashboard dan aplikasi desktop tidak
perlu diubah. Rust **menggantikan** Node dalam satu peralihan (cutover): keduanya
tidak pernah melayani bersamaan, tetapi memakai Postgres, Redis, GCS, dan
`SECRET_TOKEN` yang sama sehingga rollback ke Node tetap mungkin (lihat
[plan/PLAN.md](plan/PLAN.md) dan [plan/CUTOVER.md](plan/CUTOVER.md)).

- Binary: `mudah-rs` (crate `crates/mz-server`), port bawaan **3100**.
- Skema database dimiliki Mudah-API (Prisma). Rust **tidak** menjalankan migrasi.
- Cek hidup: `GET /health` → `{"status":"ok","permissions":"ok","runtime":"rust"}`.

## Daftar isi

1. [Prasyarat](#prasyarat)
2. [Konfigurasi](#konfigurasi)
3. [Build](#build)
4. [Menjalankan untuk development](#menjalankan-untuk-development)
5. [Test](#test)
6. [Deploy ke produksi](#deploy-ke-produksi)
7. [Masalah yang sering muncul](#masalah-yang-sering-muncul)

## Prasyarat

| Kebutuhan | Catatan |
|---|---|
| Rust **1.99.1** | Dipasang otomatis oleh `rustup` dari `rust-toolchain.toml` (beserta `rustfmt` dan `clippy`). |
| PostgreSQL | Database yang sama dengan Mudah-API, skema sudah dimigrasi Prisma. Uji integrasi dan paritas butuh PostgreSQL **17**. |
| Redis | Yang sama dengan Mudah-API. |
| Disk | Folder `target/` bisa puluhan GB. Lihat [Masalah yang sering muncul](#masalah-yang-sering-muncul). |

## Konfigurasi

Semua konfigurasi lewat variabel lingkungan dengan nama yang **sama** dengan
Mudah-API, jadi satu `.env` bisa dipakai kedua proses.

```bash
cp .env.example .env
# isi minimal: DATABASE_URL, SECRET_TOKEN, REDIS_HOST, REDIS_PORT, REDIS_PASSWORD
```

`.env` dibaca aplikasi dari folder kerja saat start (variabel yang sudah ada di
lingkungan proses tidak ditimpa). Penjelasan tiap variabel ada di
[.env.example](.env.example). Yang khusus Rust:

| Variabel | Bawaan | Fungsi |
|---|---|---|
| `RS_PORT` | `PORT`, lalu `3100` | Port HTTP |
| `RS_DB_MAX_CONNECTIONS` | `20` | Ukuran pool koneksi Postgres |
| `RS_PROJECTS_LIST_CONCURRENCY` | `RS_DB_MAX_CONNECTIONS / 3` | Maks. `GET /projects` yang memakai koneksi DB bersamaan |
| `RS_PROJECTS_DEVICE_LITE` | `true` | Desktop menerima daftar project tanpa relasi (±4 KB, bukan ±250 KB) |
| `RS_LOG_DIR` | `logs` | Folder log harian `mudah-rs.log.YYYY-MM-DD` (JSON) |
| `MAIL_TEMPLATE_DIR` | `templates/mail` | Template email |
| `RUST_LOG` | `info,sqlx=warn,tower_http=info` | Tingkat log |
| `REDIS_DB` | `0` | Nomor database Redis |
| `MZ_JOBS` | kosong (mati) | Cron & kerja latar yang dijalankan (`all` atau daftar nama). Produksi: `all`, diset saat cutover sesudah Node mati |
| `SHUTDOWN_DRAIN_SECS` | `8` | Lama menunggu tugas latar (email, audit, …) saat berhenti |
| `MONITORING_CRON_ENABLED` | kosong (mati) | Laporan monitoring ke Discord (nama & arti sama dengan Node, bersama `DISCORD_MONITORING_WEBHOOK`, `MONITORING_INTERVAL_MINUTES` (10), `MONITORING_ALERT_DISCORD_USER_IDS`, `DO_TOKEN`, `DO_DROPLET_ID`, `DO_DATABASE_ID`). Hanya `true` yang menyalakan; **tidak** diatur `MZ_JOBS` |

Log ditulis ke stdout **dan** ke `RS_LOG_DIR`, keduanya JSON.

## Build

```bash
# Development (cepat dikompilasi, lambat dijalankan)
cargo build --bin mudah-rs              # → target/debug/mudah-rs

# Produksi
cargo build --release --bin mudah-rs    # → target/release/mudah-rs
```

**Versi glibc.** Binary Rust bergantung pada glibc sistem tempat ia dibuild.
Build di mesin dengan glibc yang **sama atau lebih lama** dari server produksi
(paling aman: OS yang sama dengan server). Cek kebutuhan binary dan versi server:

```bash
objdump -T target/release/mudah-rs | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1   # yang dibutuhkan
ldd --version | head -1                                                              # di server
```

Image Docker juga tersedia (`docker build -t mudah/api-rs:latest .`), tetapi
produksi tidak memakai Docker. Lihat [Deploy ke produksi](#deploy-ke-produksi).

## Menjalankan untuk development

Butuh Postgres dan Redis lokal yang sama dengan Mudah-API lokal.

```bash
cargo run --bin mudah-rs
curl -s localhost:3100/health
```

Lainnya:

```bash
RUST_LOG=debug,sqlx=info cargo run --bin mudah-rs     # log lebih rinci (termasuk query)
cargo run --bin mudah-rs -- job <nama>                # jalankan satu cron sekali, lalu keluar
cargo run --bin mudah-rs -- job x                     # nama job salah → cetak daftar job
```

Request dianggap dari **desktop** bila token JWT berisi `sid` dan tidak ada
header `Origin` (aturan yang sama dengan Node). Untuk mencoba respons dashboard
dari `curl`, tambahkan `-H 'Origin: http://localhost:3001'`.

`deploy/nginx-strangler.conf` dan `deploy/traefik-strangler.toml` hanya arsip
masa strangler (per route); cutover memakai satu upstream, lihat
[plan/CUTOVER.md](plan/CUTOVER.md).

## Test

### Sebelum commit

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

Tanpa database test, `cargo test` hanya menjalankan unit test. Uji integrasi
dicetak `SKIP` dan tidak dihitung gagal.

### Uji integrasi (Postgres dan Redis sungguhan)

File `crates/mz-server/tests/it_*.rs` memakai harness `crates/mz-testkit`:

- Tiap file test membuat database sendiri, `mz_test_<nama>`, hasil salinan
  template `mudah_test_template`. Database itu dihapus dan dibuat ulang setiap
  kali test dijalankan, jadi jangan jalankan file test yang sama dua kali
  bersamaan.
- Redis memakai DB 15 supaya tidak bercampur dengan data lokal Node (DB 0).

**Siapkan template sekali.** Template berisi data uji yang **sudah dianonimkan**
dan skema Prisma terbaru:

```bash
createdb -T <database dump lokal yang sudah dianonimkan> mudah_test_template
# di repo Mudah-API, terapkan migrasi Prisma ke template:
DATABASE_URL=postgres://…/mudah_test_template npx prisma migrate deploy
```

Setiap ada migrasi Prisma baru, terapkan juga ke `mudah_test_template`.
Jangan mengubah `mudah_parity_template` atau `mudah_parity` karena dipakai
harness paritas.

**Jalankan:**

```bash
export MZ_TEST_DATABASE_URL=postgres://postgres:<pw>@127.0.0.1:5432/postgres   # nama DB diabaikan

cargo test -p mz-server --test it_projects            # satu file
cargo test -p mz-server --test it_projects list_      # filter nama test
cargo test --workspace                                # semua
```

**Lokal: pakai Postgres di RAM (disarankan).** Setiap binary test menyalin
template (±500 MB). Di cluster biasa, satu `cargo test --workspace` menulis
puluhan GB ke SSD. `tools/test-pg-tmpfs.sh` menjalankan cluster Postgres
terpisah (port 5434) di `/dev/shm` dengan fsync/WAL dimatikan, jadi salinan
itu hanya ditulis ke RAM (±1–2 GB terpakai). Isinya hilang saat WSL atau
komputer restart, jadi jalankan `up` lagi setelah restart.

```bash
MZ_TEST_TEMPLATE_SOURCE=postgres://<user>:<pw>@127.0.0.1:5432/mudah_test_template \
  tools/test-pg-tmpfs.sh up               # start + salin template (±20 detik)
eval "$(tools/test-pg-tmpfs.sh env)"      # MZ_TEST_DATABASE_URL → cluster RAM
cargo test -p mz-orders --test it_orders_api
tools/test-pg-tmpfs.sh status | stop | destroy
```

Setelah ada migrasi Prisma baru di template sumber, jalankan `destroy` lalu
`up` supaya salinan di RAM ikut diperbarui.

| Variabel | Bawaan | Fungsi |
|---|---|---|
| `MZ_TEST_DATABASE_URL` | – | Server Postgres untuk test. Tanpa ini uji integrasi di-`SKIP` |
| `MZ_TEST_TEMPLATE` | `mudah_test_template` | Database template |
| `MZ_TEST_REDIS_DB` | `15` | Database Redis untuk test |
| `MZ_TEST_REQUIRE_DB` | – | `1` = gagal (bukan `SKIP`) bila database test tidak siap. Untuk CI |

Variabel `MZ_TEST_*` diset di shell, **jangan** di `.env`. Harness menolak
jalan bila kredensial layanan luar (`MAILTRAP_TOKEN`, `MIXPANEL_PROJECT_TOKEN`,
`DISCORD_WEBHOOK_URL`, …) ada di lingkungan proses, supaya test tidak mengirim
email atau event sungguhan.

### Uji paritas Node vs Rust

Memutar request yang sama ke Mudah-API dan mudah-rs di atas salinan dump yang
identik, lalu membandingkan responsnya. Lihat
[tools/parity/README.md](tools/parity/README.md).

```bash
tools/parity/up.sh
tools/parity/run.sh --only-read
tools/parity/down.sh
```

## Deploy ke produksi

Produksi menjalankan binary langsung lewat **systemd**, tanpa Docker. Contoh
unit lengkap ada di [deploy/mudah-rs.service](deploy/mudah-rs.service).

### Pemasangan pertama

```bash
# 1. Build (lihat catatan glibc di bagian Build)
cargo build --release --bin mudah-rs

# 2. Di server
sudo useradd --system --home /opt/mudah-rs --shell /usr/sbin/nologin mudah
sudo mkdir -p /opt/mudah-rs/logs
sudo cp mudah-rs /opt/mudah-rs/
sudo cp -r templates /opt/mudah-rs/
sudo cp .env /opt/mudah-rs/.env && sudo chmod 600 /opt/mudah-rs/.env
sudo chown -R mudah:mudah /opt/mudah-rs

sudo cp mudah-rs.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now mudah-rs
```

`.env` produksi = salinan `.env` Node (nama variabel sama) ditambah variabel
khusus Rust di atas. `MZ_JOBS=all` diset saat cutover, sesudah Node dimatikan;
urutan lengkap dan rollback ada di [plan/CUTOVER.md](plan/CUTOVER.md).

### Memperbarui versi

Binary lama disimpan sebagai `mudah-rs.prev` untuk rollback:

```bash
sudo install -o mudah -g mudah -m 755 mudah-rs /opt/mudah-rs/mudah-rs.new
sudo cp -p /opt/mudah-rs/mudah-rs /opt/mudah-rs/mudah-rs.prev
sudo mv /opt/mudah-rs/mudah-rs.new /opt/mudah-rs/mudah-rs
sudo systemctl restart mudah-rs
```

Saat `restart`, server berhenti menerima request baru, menyelesaikan request
yang sedang jalan, lalu menunggu tugas latar sampai `SHUTDOWN_DRAIN_SECS`.
Selama jeda restart (±1–2 detik) port tidak menerima koneksi, jadi lakukan di
luar jam ramai.

**Rollback:**

```bash
sudo cp -p /opt/mudah-rs/mudah-rs.prev /opt/mudah-rs/mudah-rs
sudo systemctl restart mudah-rs
```

### Cek setelah deploy

```bash
systemctl status mudah-rs
curl -s localhost:3100/health
journalctl -u mudah-rs -n 50 --no-pager
journalctl -u mudah-rs | grep "file terbuka"      # batas file terbuka yang dipakai
```

Saat start, server menaikkan sendiri batas file terbuka ke batas keras
(`LimitNOFILE=65535` di unit). Bila log berisi peringatan
`batas file terbuka rendah`, server bisa gagal dengan `Too many open files`
saat ±1000 desktop tersambung bersamaan. Periksa `LimitNOFILE` di unit.

### Cutover dari Node

Satu kali, semua path sekaligus: kuras antrean Bull Node, matikan Node,
nyalakan `mudah-rs` dengan `MZ_JOBS=all`, alihkan upstream nginx. Langkah,
pemeriksaan, dan rollback: [plan/CUTOVER.md](plan/CUTOVER.md). Penyimpangan
perilaku yang disengaja dari Node dicatat di [plan/deviations/](plan/deviations/).

## Masalah yang sering muncul

| Gejala | Penyebab dan solusi |
|---|---|
| `GLIBC_2.xx not found` saat start di server | Binary dibuild di mesin dengan glibc lebih baru. Build di OS yang sama dengan server. |
| `Too many open files` di log | Batas file terbuka proses terlalu kecil. Pastikan `LimitNOFILE=65535` di unit systemd, lalu `daemon-reload` dan `restart`. |
| `DATABASE_URL wajib diisi` | `.env` tidak ditemukan. Jalankan dari folder yang berisi `.env` (unit systemd memakai `WorkingDirectory=/opt/mudah-rs`). |
| Uji integrasi hanya mencetak `SKIP` | `MZ_TEST_DATABASE_URL` belum diset di shell. |
| Uji integrasi gagal membuat database | Template `mudah_test_template` belum ada, atau file test yang sama sedang jalan di terminal lain. |
| Build gagal `No space left on device` | `target/` membengkak. Hapus `target/debug/incremental` atau jalankan `cargo clean`. Build dengan `CARGO_INCREMENTAL=0` supaya lebih hemat disk. |
