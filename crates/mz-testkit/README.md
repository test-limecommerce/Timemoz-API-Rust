# mz-testkit

Harness uji integrasi: router penuh `mz-server` di atas Postgres & Redis **nyata**.
Test ditulis di `crates/mz-server/tests/it_*.rs`.

## Menjalankan

```sh
export MZ_TEST_DATABASE_URL=postgres://postgres:<pw>@127.0.0.1:5432/postgres
cargo test -p mz-server --test it_projects      # satu crate
cargo test -p mz-server                          # semua
```

Tanpa `MZ_TEST_DATABASE_URL` setiap test mencetak `SKIP` dan lulus;
`MZ_TEST_REQUIRE_DB=1` (CI) membuatnya gagal. Harness menolak berjalan bila
kredensial layanan luar (`MAILTRAP_TOKEN`, `MIXPANEL_PROJECT_TOKEN`, `DISCORD_MONITORING_WEBHOOK`, `DO_TOKEN`, PayPal,
Google, Jira, Discord, `GCP_KEY_PATH`) ada di env.

## Database

- Tiap binary test → database `mz_test_<nama binary>` = `CREATE DATABASE … TEMPLATE
  mudah_test_template` (±5 dtk, ±500 MB), di-drop & dibuat ulang tiap run. Jangan
  jalankan binary yang sama dua kali bersamaan.
- Semua test dalam satu binary berbagi database itu dan berjalan paralel: buat data
  sendiri (nama unik), jangan menghapus/menonaktifkan user/company fixture.
- Redis memakai DB 15 (`MZ_TEST_REDIS_DB`). Setiap request diberi `x-forwarded-for`
  unik supaya limiter per IP tidak saling ganggu (`.ip()` untuk menguji limiter).

### Membuat ulang `mudah_test_template`

Template = dump lokal Mudah-API (`mudah-local-*.dump`, sudah dianonimkan →
`mudah_parity_template`) + migrasi Prisma yang belum ada di dump:

```sh
psql "$PG/postgres" -c 'CREATE DATABASE mudah_test_template TEMPLATE mudah_parity_template'
for m in <migrasi yang belum ada>; do
  psql "$PG/mudah_test_template" -v ON_ERROR_STOP=1 -1 -f ../Mudah-API/prisma/migrations/$m/migration.sql
done
# verifikasi: tidak boleh ada CREATE TABLE / ADD COLUMN
npx prisma migrate diff --from-url "$PG/mudah_test_template?schema=public" \
  --to-schema-datamodel prisma/schema.prisma --script
```

Per 2026-11-16 migrasi yang ditambahkan: `20261112100000_operator_roles_and_audit`,
`20261112100100_manual_subscription`, `20261112100200_admin_global_data`,
`20261113100000_manual_contract_pricing`, `20261116100000_activity_track_on_paid_subscription`.
Setiap migrasi Prisma baru harus diterapkan juga ke template.

## Fixture

`app.company().await` → company 2 (`MZ_TEST_COMPANY_ID`) dengan user aktif
`super_admin`, `admin`, `manager`, `staff`, `staff2` (+ JWT `{userId}` tanpa `sid`).
Saat dimuat: langganan terkini dibuat `ACTIVE` s.d. +1 tahun, modul
TIME_TRACKING & SALES_TRACKER dinyalakan.

Asersi: `assert_ok()` (2xx + `success` ≠ false), `assert_status(n)`,
`assert_no_sql_error()` (bukan 5xx dan tidak ada jejak galat SQL di body).
