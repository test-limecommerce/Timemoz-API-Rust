-- Produk yang dijual sebuah paket harga: Sales Tracker saja, Time Tracking
-- saja, atau keduanya. Dibaca endpoint publik
-- GET /v1/settings/pricing-lists?product=SALES|TIME|FULL.
--
-- Default `FULL` sekaligus jadi nilai backfill: semua paket yang sudah ada
-- selama ini menjual produk lengkap, jadi migration ini tidak mengubah paket
-- mana pun. Tanpa `product` di query, endpoint tetap mengembalikan semuanya,
-- jadi klien lama juga tidak terpengaruh.
--
-- `IF NOT EXISTS` / `DO $$` supaya migration ini aman dijalankan ulang pada
-- database dev yang kolomnya sudah lebih dulu masuk lewat `prisma db push`.
--
-- Tidak ada rewrite tabel: sejak PostgreSQL 11, `ADD COLUMN ... NOT NULL
-- DEFAULT <konstanta>` ditulis ke katalog, bukan ke setiap baris.

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'PricingProductType') THEN
    CREATE TYPE "PricingProductType" AS ENUM ('SALES', 'TIME', 'FULL');
  END IF;
END
$$;

ALTER TABLE "pricingTiers"
  ADD COLUMN IF NOT EXISTS "productType" "PricingProductType" NOT NULL DEFAULT 'FULL';
