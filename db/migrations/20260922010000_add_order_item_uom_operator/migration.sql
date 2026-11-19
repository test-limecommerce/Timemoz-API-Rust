-- Snapshot konversi UOM di baris order: `operator` + `factor` disalin dari `productUom`
-- saat item ditulis, di samping `conversionRate` (base equivalent) yang sudah ada.
-- Tanpa ini baris order hanya menyimpan hasilnya (mis. 0.5), sehingga "1 PACK = 2 PCS
-- dibagi" tidak bisa dibaca ulang setelah konversi produknya diubah.
--
-- Bergantung pada tipe "UomOperator" dari 20260922000000_add_uom_conversion_operator.
-- `IF NOT EXISTS` supaya aman dijalankan ulang pada database dev yang kolomnya sudah
-- lebih dulu masuk lewat `prisma db push`.

ALTER TABLE "orderItem"
  ADD COLUMN IF NOT EXISTS "operator" "UomOperator" NOT NULL DEFAULT 'multiply',
  ADD COLUMN IF NOT EXISTS "factor" DECIMAL(18, 6) NOT NULL DEFAULT 1;

-- Baris lama: sebelum operator ada, setiap conversionRate adalah multiply × conversionRate.
-- Hanya menyentuh baris yang masih bernilai bawaan kolom baru, jadi aman diulang dan
-- tidak menimpa baris `divide` yang ditulis kode baru.
UPDATE "orderItem"
   SET "factor" = "conversionRate"
 WHERE "operator" = 'multiply'
   AND "factor" = 1
   AND "conversionRate" <> 1;
