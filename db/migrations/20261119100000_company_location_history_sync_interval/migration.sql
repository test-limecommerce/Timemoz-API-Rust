-- Jeda sync riwayat lokasi aplikasi mobile, per company, dalam menit.
-- ADD COLUMN dengan DEFAULT konstan: Postgres 11+ hanya menulis metadata, tanpa
-- rewrite tabel; baris lama langsung terbaca 15. Tidak ada data yang hilang.
ALTER TABLE "company"
  ADD COLUMN "locationHistorySyncIntervalMinutes" INTEGER NOT NULL DEFAULT 15;
