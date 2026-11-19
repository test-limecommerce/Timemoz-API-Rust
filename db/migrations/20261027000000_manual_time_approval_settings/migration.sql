-- Manual Time Approval Settings (specs/005-manual-time-approval-settings).
--
-- Nol perubahan perilaku: kolom baru di "company" berdefault ALL / DAY / true,
-- persis perilaku hari ini (semua manual time butuh approval). Tanpa backfill UPDATE.
--
-- Partial unique index di bagian 5 TIDAK bisa dinyatakan di `schema.prisma`.
-- Kembarannya yang idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah
-- satu diubah, ubah keduanya. `npm run db:sync` menjalankan keduanya berurutan.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum baru
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TYPE "ManualTimeApprovalMode" AS ENUM ('NONE', 'ALL', 'LIMIT');

CREATE TYPE "ManualTimeLimitPeriod" AS ENUM ('DAY', 'WEEK');

CREATE TYPE "ManualTimeApprovalSource" AS ENUM ('MANUAL', 'AUTO');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Kolom baru di "company"
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "company"
  ADD COLUMN "manualTimeApprovalMode"            "ManualTimeApprovalMode" NOT NULL DEFAULT 'ALL',
  ADD COLUMN "manualTimeAutoApproveLimitMinutes" INTEGER,
  ADD COLUMN "manualTimeAutoApprovePeriod"       "ManualTimeLimitPeriod"  NOT NULL DEFAULT 'DAY',
  ADD COLUMN "manualTimeAutoApproveNewEmployees" BOOLEAN                  NOT NULL DEFAULT true;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Kolom baru di "trackingTimeline" (nullable, tanpa backfill)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "trackingTimeline"
  ADD COLUMN "manualTimeApprovalSource" "ManualTimeApprovalSource";

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Tabel pengecualian
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "manualTimeApprovalException" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "userId"      INTEGER      NOT NULL,
    "createdById" INTEGER,
    "deletedAt"   TIMESTAMP(3),
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "manualTimeApprovalException_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "manualTimeApprovalException_uuid_key" ON "manualTimeApprovalException"("uuid");

CREATE INDEX "manualTimeApprovalException_companyId_deletedAt_idx"
  ON "manualTimeApprovalException"("companyId", "deletedAt");

CREATE INDEX "manualTimeApprovalException_userId_idx" ON "manualTimeApprovalException"("userId");

ALTER TABLE "manualTimeApprovalException" ADD CONSTRAINT "manualTimeApprovalException_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "manualTimeApprovalException" ADD CONSTRAINT "manualTimeApprovalException_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "manualTimeApprovalException" ADD CONSTRAINT "manualTimeApprovalException_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Partial unique index — TIDAK dinyatakan sebagai @@unique biasa: itu akan
--    menutup pintu bagi baris yang sudah dicabut (deletedAt IS NOT NULL) untuk
--    ditambahkan kembali.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "manualTimeApprovalException_unique_active"
  ON "manualTimeApprovalException" ("companyId", "userId")
  WHERE "deletedAt" IS NULL;
