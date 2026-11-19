-- Tiga batas paket harga baru, pelengkap yang masuk lewat
-- 20260908010000_add_pricing_tier_feature_limits dan 20260908020000_add_pricing_tier_total_geofence:
--
--   - maxAutoCheckIns: jumlah check-in otomatis (ping GPS §3.1 di
--     api-mobile-store-visit-event.md) yang boleh dicatat SEUMUR langganan.
--   - maxFormTemplates: jumlah formulir kunjungan (storeVisitFormTemplate) yang boleh dibuat.
--   - visitHistoryDays: jendela riwayat kunjungan yang TERLIHAT, dalam hari. Data lama tidak
--     terhapus, cuma disunat dari query daftar/riwayat kunjungan.
--
-- Default 0 (tanpa batas) untuk ketiganya — BEDA dari default maxStores/totalGeofence yang
-- bukan 0, karena dua kolom itu belum ditegakkan kode apa pun saat migration-nya masuk. Ketiga
-- kolom di sini DITEGAKKAN oleh perubahan yang sama dengan migration ini, jadi kalau defaultnya
-- langsung angka FREE (100 / 1 / 7), setiap company berlangganan PAID pun akan mendarat di batas
-- itu sampai ada yang memperbaikinya — silent breakage untuk pelanggan berbayar. Default 0
-- membuat migration ini aman: tidak ada paket yang berubah perilakunya sampai baris FREE
-- disetel manual.
--
-- SUSULAN WAJIB setelah migration ini jalan: set paket FREE via SQL, misalnya
--   UPDATE "pricingTiers" SET "maxAutoCheckIns" = 100, "maxFormTemplates" = 1,
--     "visitHistoryDays" = 7 WHERE name = 'FREE';
-- (admin-pricing belum bisa menyetel kolom-kolom batas ini — sama seperti maxStores dkk.)
--
-- `IF NOT EXISTS` supaya aman dijalankan ulang pada database dev yang kolomnya sudah lebih
-- dulu masuk lewat `prisma db push`. `ADD COLUMN ... NOT NULL DEFAULT <konstanta>` tidak
-- me-rewrite tabel sejak PostgreSQL 11.
ALTER TABLE "pricingTiers"
  ADD COLUMN IF NOT EXISTS "maxAutoCheckIns" INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS "maxFormTemplates" INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS "visitHistoryDays" INTEGER NOT NULL DEFAULT 0;

-- "ping" (§3.1, background ping — dihitung ke maxAutoCheckIns) vs "bookend" (§3.2, efek
-- samping checkIn/checkOut eksplisit — tidak pernah dibatasi). Default "bookend" supaya baris
-- lama (dan baris manapun yang lupa mengisi kolom ini) jatuh ke sisi yang TIDAK dihitung, jadi
-- company yang sudah ada tidak tiba-tiba kehabisan jatah seumur langganan gara-gara riwayat
-- ping lamanya.
ALTER TABLE "storeVisitLocationEvent"
  ADD COLUMN IF NOT EXISTS "source" VARCHAR(10) NOT NULL DEFAULT 'bookend';
