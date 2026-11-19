-- Mode cuti sakit per company: DEDUCT_ANNUAL (potong cuti tahunan), UNLIMITED, LIMITED (pakai sickLeaveDays).

-- CreateEnum
CREATE TYPE "LeaveSickMode" AS ENUM ('DEDUCT_ANNUAL', 'UNLIMITED', 'LIMITED');

-- Default kolom sengaja DEDUCT_ANNUAL: perilaku lama untuk sickLeaveDays = NULL. Company baru
-- diberi UNLIMITED oleh aplikasi (auth.service + LeavePoliciesService.upsertCompanyPolicy).
ALTER TABLE "leavePolicy"
  ADD COLUMN "sickLeaveMode" "LeaveSickMode" NOT NULL DEFAULT 'DEDUCT_ANNUAL';

-- Backfill: policy yang sudah punya jatah cuti sakit tetap berjalan seperti sebelumnya.
-- (db push tidak menjalankan UPDATE ini; jalankan manual di DB yang tidak memakai migrate.)
UPDATE "leavePolicy"
SET "sickLeaveMode" = 'LIMITED'
WHERE "sickLeaveDays" IS NOT NULL;
