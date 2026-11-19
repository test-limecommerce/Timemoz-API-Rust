-- Email `sales-trial-onboarding` pindah dari "langsung saat register" ke
-- "sekali, 24 jam setelah verifikasi email, kalau belum create organization".
--
-- - emailVerifiedAt: diisi saat konfirmasi email / register via Google. User
--   lama tetap NULL, jadi cron tidak pernah menjangkau mereka (tanpa backfill).
-- - salesOnboardingEmailEligible: ditandai saat register paket SALES.
-- - salesOnboardingEmailSentAt: klaim sekali-kirim oleh cron.
--
-- `IF NOT EXISTS` supaya aman dijalankan ulang pada database dev yang kolomnya
-- sudah lebih dulu masuk lewat `prisma db push`. `ADD COLUMN ... NOT NULL
-- DEFAULT <konstanta>` tidak me-rewrite tabel sejak PostgreSQL 11.
ALTER TABLE "users"
  ADD COLUMN IF NOT EXISTS "emailVerifiedAt" TIMESTAMP(3),
  ADD COLUMN IF NOT EXISTS "salesOnboardingEmailEligible" BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS "salesOnboardingEmailSentAt" TIMESTAMP(3);

CREATE INDEX IF NOT EXISTS "users_salesOnboardingEmailEligible_salesOnboardingEmailSentAt_idx"
  ON "users"("salesOnboardingEmailEligible", "salesOnboardingEmailSentAt");
