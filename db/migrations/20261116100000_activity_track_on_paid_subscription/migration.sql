-- Subscription yang BERUBAH menjadi berbayar menyalakan `company.useActivityTrack`.
--
-- "Berbayar" = status ACTIVE pada tier ber-harga (`basePrice > 0 OR
-- pricePerUserUsd > 0`), aturan yang sama dengan `isPaidTier` di
-- src/plan-limits/company-plan-limits.ts. Sumber billing tidak dilihat: PayPal
-- (upgrade sendiri), MANUAL (dicatat admin), dan cron ganti paket semuanya lewat
-- tabel ini.
--
-- Trigger, bukan kode di tiap service: ada lebih dari 20 tempat yang menulis
-- `subscriptions.status`/`pricingTierId` (webhook PayPal, cron, invoice, manual
-- subscription, PATCH admin, termasuk `updateMany`). Satu trigger menangkap
-- semuanya, termasuk jalur yang ditambahkan nanti.
--
-- Hanya TRANSISI yang menyalakan flag: INSERT yang langsung berbayar, atau UPDATE
-- dari keadaan tidak-berbayar (status selain ACTIVE, atau tier gratis) ke
-- berbayar. Perpanjangan atau ganti antar-tier berbayar tidak menyentuh company,
-- jadi flag yang sengaja dimatikan admin untuk company berbayar tetap mati sampai
-- company itu keluar lalu masuk lagi ke paket berbayar.
--
-- Flag hanya dinyalakan, tidak pernah dimatikan (downgrade ke Free tidak
-- mengubahnya).
--
-- Tidak ada data yang dihapus. Company yang SUDAH berbayar sebelum migration ini
-- tidak diubah (tidak ada backfill).
--
-- Dipasang juga oleh prisma/apply-partial-indexes.js (entri
-- `subscriptions_paid_enables_activity_track`) — isi fungsi di sana wajib sama.

CREATE OR REPLACE FUNCTION enable_activity_track_on_paid_subscription() RETURNS TRIGGER AS $$
BEGIN
  IF NEW."status" <> 'ACTIVE' OR NOT EXISTS (
    SELECT 1 FROM "pricingTiers" t
     WHERE t."id" = NEW."pricingTierId"
       AND (t."basePrice" > 0 OR t."pricePerUserUsd" > 0)
  ) THEN
    RETURN NULL;
  END IF;

  IF TG_OP = 'UPDATE' AND OLD."status" = 'ACTIVE' AND EXISTS (
    SELECT 1 FROM "pricingTiers" t
     WHERE t."id" = OLD."pricingTierId"
       AND (t."basePrice" > 0 OR t."pricePerUserUsd" > 0)
  ) THEN
    RETURN NULL;
  END IF;

  UPDATE "company" SET "useActivityTrack" = true
   WHERE "id" = NEW."companyId" AND "useActivityTrack" = false;
  RETURN NULL;
END; $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS "subscriptions_paid_enables_activity_track" ON "subscriptions";
CREATE TRIGGER "subscriptions_paid_enables_activity_track"
  AFTER INSERT OR UPDATE OF "status", "pricingTierId" ON "subscriptions"
  FOR EACH ROW EXECUTE FUNCTION enable_activity_track_on_paid_subscription();
