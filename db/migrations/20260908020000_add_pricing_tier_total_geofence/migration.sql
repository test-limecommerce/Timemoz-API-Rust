-- Batas jumlah geofence per paket harga, pelengkap batas & gerbang fitur yang
-- masuk di 20260908010000_add_pricing_tier_feature_limits.
--
-- Satu kolom `NOT NULL DEFAULT 2`, dan default itulah seluruh isinya: paket
-- yang sudah ada mendarat di angka yang sama, jadi migration ini tidak mengubah
-- paket mana pun. Belum ada kode yang menegakkan batas ini; nilainya baru
-- dilaporkan ke aplikasi lewat GET /mobile/company/config.
--
-- `IF NOT EXISTS` supaya migration ini aman dijalankan ulang pada database dev
-- yang kolomnya sudah lebih dulu masuk lewat `prisma db push`.
--
-- Tidak ada rewrite tabel: sejak PostgreSQL 11, `ADD COLUMN ... NOT NULL
-- DEFAULT <konstanta>` ditulis ke katalog, bukan ke setiap baris.
ALTER TABLE "pricingTiers"
  ADD COLUMN IF NOT EXISTS "totalGeofence" INTEGER NOT NULL DEFAULT 2;
