-- Kontrak harga langganan MANUAL: harga per user per bulan, mata uang, panjang periode
-- tagihan, dan PPN per company. Dipakai untuk membuat invoice UNPAID periode berikutnya.
-- Data loss: none. Kolom baru nullable / DEFAULT konstan; baris lama tidak berubah.
-- Apply after pg_dump:
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261113100000_manual_contract_pricing/migration.sql

ALTER TABLE "subscriptions"
  ADD COLUMN "contractPricePerUser" DOUBLE PRECISION,
  ADD COLUMN "contractCurrency"     VARCHAR(3),
  ADD COLUMN "contractPeriodMonths" INTEGER,
  ADD COLUMN "contractTaxPercent"   DOUBLE PRECISION NOT NULL DEFAULT 0;
