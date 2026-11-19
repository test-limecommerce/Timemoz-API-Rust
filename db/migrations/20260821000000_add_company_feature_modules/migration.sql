-- AlterTable
-- Keduanya default true: company yang sudah ada tidak kehilangan satu endpoint pun
-- pada hari deploy. Pilihan modul baru berlaku setelah admin menyetelnya sendiri.
ALTER TABLE "company"
  ADD COLUMN "moduleTimeTrackingEnabled" BOOLEAN NOT NULL DEFAULT true,
  ADD COLUMN "moduleSalesTrackerEnabled" BOOLEAN NOT NULL DEFAULT true;

-- Company tanpa satu pun modul aktif adalah company yang tidak bisa dipakai untuk
-- apa pun — dan tidak ada layar untuk menghidupkannya kembali selain layar setting
-- yang juga ikut mati. Aturan ini dijaga di service (transaksi read-modify-write),
-- CHECK ini menutup jalur tulis lain: admin-company, seed, dan script manual.
ALTER TABLE "company"
  ADD CONSTRAINT "company_at_least_one_module"
  CHECK ("moduleTimeTrackingEnabled" OR "moduleSalesTrackerEnabled");
