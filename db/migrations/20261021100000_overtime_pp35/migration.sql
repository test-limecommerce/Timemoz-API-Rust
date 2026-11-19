-- Lembur per PP 35/2021 (specs/004-overtime-pp35, data-model.md).
--
-- Empat enum baru, empat tabel baru ("companyWorkPattern", "companyOvertimeSetting",
-- "overtimeApprover", "overtimeRequest"), satu partial unique index, dan lima CHECK.
--
-- Bagian 5–6 (partial unique index + CHECK) tidak bisa dinyatakan di schema.prisma.
-- `prisma db push` tidak membuatnya dan MENGHAPUSNYA kalau ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum
-- ─────────────────────────────────────────────────────────────────────────────

-- CreateEnum
CREATE TYPE "OvertimeRequestStatus" AS ENUM ('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED', 'REJECTED', 'CANCELLED', 'COMPLETED');

-- CreateEnum
CREATE TYPE "OvertimeRequestKind" AS ENUM ('SELF', 'ORDER');

-- CreateEnum
CREATE TYPE "OvertimeCancelReason" AS ENUM ('BY_REQUESTER', 'NO_RESPONSE', 'EMPLOYEE_ON_LEAVE', 'FEATURE_DISABLED');

-- CreateEnum
CREATE TYPE "OvertimeDayType" AS ENUM ('WORKDAY', 'HOLIDAY');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Tabel
-- ─────────────────────────────────────────────────────────────────────────────

-- CreateTable
CREATE TABLE "companyWorkPattern" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "workDays" SMALLINT NOT NULL,
    "offDays" INTEGER[],
    "shortDay" INTEGER,
    "effectiveFrom" DATE NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "createdById" INTEGER,
    "supersededAt" TIMESTAMP(3),

    CONSTRAINT "companyWorkPattern_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "companyOvertimeSetting" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "retroWindowDays" INTEGER NOT NULL DEFAULT 7,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedById" INTEGER,

    CONSTRAINT "companyOvertimeSetting_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "overtimeApprover" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "createdById" INTEGER,

    CONSTRAINT "overtimeApprover_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "overtimeRequest" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "requestedById" INTEGER NOT NULL,
    "kind" "OvertimeRequestKind" NOT NULL,
    "date" DATE NOT NULL,
    "requestedSec" INTEGER NOT NULL,
    "reason" VARCHAR(500) NOT NULL,
    "status" "OvertimeRequestStatus" NOT NULL,
    "retroWindowDays" INTEGER NOT NULL,
    "responseDeadline" TIMESTAMP(3) NOT NULL,
    "finalAt" TIMESTAMP(3) NOT NULL,
    "decidedById" INTEGER,
    "decidedAt" TIMESTAMP(3),
    "rejectReason" VARCHAR(500),
    "cancelReason" "OvertimeCancelReason",
    "cancelledAt" TIMESTAMP(3),
    "cancelledById" INTEGER,
    "approvedAt" TIMESTAMP(3),
    "dayType" "OvertimeDayType",
    "baseNormalSec" INTEGER,
    "dailyLimitSec" INTEGER,
    "patternWorkDays" SMALLINT,
    "workedSec" INTEGER,
    "normalSec" INTEGER,
    "overtimeSec" INTEGER,
    "computedAt" TIMESTAMP(3),
    "finalizedAt" TIMESTAMP(3),
    "exceedsDailyLimit" BOOLEAN NOT NULL DEFAULT false,
    "exceedsWeeklyLimit" BOOLEAN NOT NULL DEFAULT false,
    "exceedsLimit" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "overtimeRequest_pkey" PRIMARY KEY ("id")
);

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Index
-- ─────────────────────────────────────────────────────────────────────────────

-- CreateIndex
CREATE INDEX "companyWorkPattern_companyId_effectiveFrom_idx" ON "companyWorkPattern"("companyId", "effectiveFrom");

-- CreateIndex
CREATE UNIQUE INDEX "companyOvertimeSetting_companyId_key" ON "companyOvertimeSetting"("companyId");

-- CreateIndex
CREATE INDEX "overtimeApprover_companyId_idx" ON "overtimeApprover"("companyId");

-- CreateIndex
CREATE UNIQUE INDEX "overtimeApprover_companyId_userId_key" ON "overtimeApprover"("companyId", "userId");

-- CreateIndex
CREATE UNIQUE INDEX "overtimeRequest_uuid_key" ON "overtimeRequest"("uuid");

-- CreateIndex
CREATE INDEX "overtimeRequest_companyId_status_date_idx" ON "overtimeRequest"("companyId", "status", "date");

-- CreateIndex
CREATE INDEX "overtimeRequest_companyId_userId_date_idx" ON "overtimeRequest"("companyId", "userId", "date");

-- CreateIndex
CREATE INDEX "overtimeRequest_companyId_date_idx" ON "overtimeRequest"("companyId", "date");

-- CreateIndex
CREATE INDEX "overtimeRequest_requestedById_status_idx" ON "overtimeRequest"("requestedById", "status");

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Foreign key
-- ─────────────────────────────────────────────────────────────────────────────

-- AddForeignKey
ALTER TABLE "companyWorkPattern" ADD CONSTRAINT "companyWorkPattern_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "companyWorkPattern" ADD CONSTRAINT "companyWorkPattern_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "companyOvertimeSetting" ADD CONSTRAINT "companyOvertimeSetting_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "companyOvertimeSetting" ADD CONSTRAINT "companyOvertimeSetting_updatedById_fkey" FOREIGN KEY ("updatedById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeApprover" ADD CONSTRAINT "overtimeApprover_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeApprover" ADD CONSTRAINT "overtimeApprover_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeApprover" ADD CONSTRAINT "overtimeApprover_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeRequest" ADD CONSTRAINT "overtimeRequest_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeRequest" ADD CONSTRAINT "overtimeRequest_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeRequest" ADD CONSTRAINT "overtimeRequest_requestedById_fkey" FOREIGN KEY ("requestedById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeRequest" ADD CONSTRAINT "overtimeRequest_decidedById_fkey" FOREIGN KEY ("decidedById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "overtimeRequest" ADD CONSTRAINT "overtimeRequest_cancelledById_fkey" FOREIGN KEY ("cancelledById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Partial unique index (FR-019) — satu permintaan aktif per karyawan per hari
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "overtimeRequest_one_active_per_day"
  ON "overtimeRequest" ("companyId", "userId", "date")
  WHERE "status" IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED', 'COMPLETED');

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. CHECK constraint
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "companyWorkPattern"
  ADD CONSTRAINT "companyWorkPattern_shape_chk"
  CHECK (
    "workDays" IN (5, 6)
    AND (
      ("workDays" = 5 AND "shortDay" IS NULL)
      OR ("workDays" = 6 AND "shortDay" IS NOT NULL)
    )
  );

ALTER TABLE "overtimeRequest"
  ADD CONSTRAINT "overtimeRequest_requested_sec_chk"
  CHECK ("requestedSec" BETWEEN 60 AND 86400);

ALTER TABLE "overtimeRequest"
  ADD CONSTRAINT "overtimeRequest_reject_reason_chk"
  CHECK ("status" <> 'REJECTED' OR "rejectReason" IS NOT NULL);

ALTER TABLE "overtimeRequest"
  ADD CONSTRAINT "overtimeRequest_cancel_reason_chk"
  CHECK ("status" <> 'CANCELLED' OR "cancelReason" IS NOT NULL);

ALTER TABLE "overtimeRequest"
  ADD CONSTRAINT "overtimeRequest_overtime_sec_chk"
  CHECK ("overtimeSec" IS NULL OR ("overtimeSec" >= 0 AND "overtimeSec" <= "requestedSec"));
