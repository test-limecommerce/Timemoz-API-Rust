-- Konversi UOM ber-operator: admin mengisi `operator` (multiply/divide) + `factor`,
-- dan `productUom.conversionRate` (base equivalent) jadi HASIL keduanya:
--   multiply → conversionRate = factor           (1 BOX = 24 PCS → factor 24)
--   divide   → conversionRate = ROUND(1/factor, 6)
-- Perhitungannya di aplikasi (src/products/uom-conversion.util.ts); conversionRate tetap
-- disimpan karena order, price list, dan katalog mobile membacanya langsung.
--
-- `uom.defaultOperator`/`defaultFactor` = nilai awal saat UOM itu ditambahkan ke produk.
-- Disalin, bukan direferensikan: mengubah default master tidak mengubah produk lama.
--
-- `IF NOT EXISTS` / `DO $$ ... $$` supaya aman dijalankan ulang pada database dev yang
-- kolomnya sudah lebih dulu masuk lewat `prisma db push`.

DO $$
BEGIN
  CREATE TYPE "UomOperator" AS ENUM ('multiply', 'divide');
EXCEPTION
  WHEN duplicate_object THEN NULL;
END $$;

ALTER TABLE "uom"
  ADD COLUMN IF NOT EXISTS "defaultOperator" "UomOperator" NOT NULL DEFAULT 'multiply',
  ADD COLUMN IF NOT EXISTS "defaultFactor" DECIMAL(18, 6) NOT NULL DEFAULT 1;

ALTER TABLE "productUom"
  ADD COLUMN IF NOT EXISTS "operator" "UomOperator" NOT NULL DEFAULT 'multiply',
  ADD COLUMN IF NOT EXISTS "factor" DECIMAL(18, 6) NOT NULL DEFAULT 1;

-- Baris lama: conversionRate selama ini diketik langsung = multiply × conversionRate.
-- Hanya menyentuh baris yang masih bernilai bawaan kolom baru, jadi aman diulang.
UPDATE "productUom"
   SET "factor" = "conversionRate"
 WHERE "operator" = 'multiply'
   AND "factor" = 1
   AND "conversionRate" <> 1;

-- Default untuk UOM bawaan (src/products/default-uoms.ts) di company yang sudah ada.
-- Hanya baris yang default-nya belum pernah diubah (masih 1).
UPDATE "uom" SET "defaultFactor" = 6  WHERE "code" = 'PACK'   AND "defaultFactor" = 1;
UPDATE "uom" SET "defaultFactor" = 24 WHERE "code" = 'BOX'    AND "defaultFactor" = 1;
UPDATE "uom" SET "defaultFactor" = 48 WHERE "code" = 'CARTON' AND "defaultFactor" = 1;
UPDATE "uom" SET "defaultFactor" = 12 WHERE "code" = 'DOZEN'  AND "defaultFactor" = 1;

-- CHECK constraint — juga dipasang idempoten oleh prisma/apply-partial-indexes.js
-- (`prisma db push` tidak mengenal CHECK dan menghapusnya).
ALTER TABLE "uom" DROP CONSTRAINT IF EXISTS "uom_default_factor_positive_chk";
ALTER TABLE "uom"
  ADD CONSTRAINT "uom_default_factor_positive_chk" CHECK ("defaultFactor" > 0);

ALTER TABLE "productUom" DROP CONSTRAINT IF EXISTS "productUom_factor_positive_chk";
ALTER TABLE "productUom"
  ADD CONSTRAINT "productUom_factor_positive_chk" CHECK ("factor" > 0);

-- Base UOM selalu multiply × 1 — pasangan dari productUom_base_rate_one_chk.
ALTER TABLE "productUom" DROP CONSTRAINT IF EXISTS "productUom_base_factor_one_chk";
ALTER TABLE "productUom"
  ADD CONSTRAINT "productUom_base_factor_one_chk"
  CHECK (NOT "isBase" OR ("operator" = 'multiply' AND "factor" = 1));
