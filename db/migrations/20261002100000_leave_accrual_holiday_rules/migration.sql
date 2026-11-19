-- Leave accrual, carry-over & holiday deduction rules (specs/002-leave-accrual-holiday-rules)

-- CreateEnum
CREATE TYPE "LeaveAccrualMode" AS ENUM ('UPFRONT', 'MONTHLY');

-- Extend LeaveBalanceChangeSource. Migration-chain databases store the column as
-- VARCHAR(20) (see 20260618000001), so the type may not exist; the new values are all
-- <= 20 chars and fit either way.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_type WHERE typname = 'LeaveBalanceChangeSource') THEN
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'MONTHLY_ACCRUAL';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'CARRY_OVER';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'FORFEIT';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'CARRY_OVER_EXPIRY';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'HOLIDAY_DEDUCTION';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'HOLIDAY_REVERSAL';
    ALTER TYPE "LeaveBalanceChangeSource" ADD VALUE IF NOT EXISTS 'LEAVE_RECALC';
  END IF;
END
$$;

-- leavePolicy
ALTER TABLE "leavePolicy"
  ADD COLUMN "accrualMode" "LeaveAccrualMode" NOT NULL DEFAULT 'UPFRONT',
  ADD COLUMN "monthlyAccrualDays" DOUBLE PRECISION;

-- leaveBalance
ALTER TABLE "leaveBalance"
  ADD COLUMN "accruedThroughMonth" INTEGER NOT NULL DEFAULT 0;

-- companyHoliday
ALTER TABLE "companyHoliday"
  ADD COLUMN "deductsLeave" BOOLEAN NOT NULL DEFAULT false;

-- leaveBalanceAdjustment
ALTER TABLE "leaveBalanceAdjustment"
  ADD COLUMN "dedupeKey" VARCHAR(120),
  ADD COLUMN "holidayId" INTEGER,
  ADD COLUMN "intendedDelta" DOUBLE PRECISION;

CREATE UNIQUE INDEX "leaveBalanceAdjustment_dedupeKey_key" ON "leaveBalanceAdjustment"("dedupeKey");
CREATE INDEX "leaveBalanceAdjustment_holidayId_idx" ON "leaveBalanceAdjustment"("holidayId");

ALTER TABLE "leaveBalanceAdjustment"
  ADD CONSTRAINT "leaveBalanceAdjustment_holidayId_fkey"
    FOREIGN KEY ("holidayId") REFERENCES "companyHoliday"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- googleCalendarEvent
ALTER TABLE "googleCalendarEvent" ADD COLUMN "leaveRequestId" INTEGER;

CREATE UNIQUE INDEX "googleCalendarEvent_leaveRequestId_key" ON "googleCalendarEvent"("leaveRequestId");

ALTER TABLE "googleCalendarEvent"
  ADD CONSTRAINT "googleCalendarEvent_leaveRequestId_fkey"
    FOREIGN KEY ("leaveRequestId") REFERENCES "leaveRequest"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- holidayLeaveDeduction
CREATE TABLE "holidayLeaveDeduction" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "holidayId" INTEGER,
    "userId" INTEGER NOT NULL,
    "balanceId" INTEGER NOT NULL,
    "holidayName" VARCHAR(200) NOT NULL,
    "holidayDate" DATE NOT NULL,
    "status" VARCHAR(16) NOT NULL,
    "intendedDays" DOUBLE PRECISION NOT NULL,
    "carryOverDeducted" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "annualDeducted" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "appliedAt" TIMESTAMP(3) NOT NULL,
    "reversedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3),

    CONSTRAINT "holidayLeaveDeduction_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "holidayLeaveDeduction_holidayId_userId_key" ON "holidayLeaveDeduction"("holidayId", "userId");
CREATE INDEX "holidayLeaveDeduction_companyId_status_idx" ON "holidayLeaveDeduction"("companyId", "status");
CREATE INDEX "holidayLeaveDeduction_balanceId_idx" ON "holidayLeaveDeduction"("balanceId");

ALTER TABLE "holidayLeaveDeduction"
  ADD CONSTRAINT "holidayLeaveDeduction_companyId_fkey"
    FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "holidayLeaveDeduction_holidayId_fkey"
    FOREIGN KEY ("holidayId") REFERENCES "companyHoliday"("id") ON DELETE SET NULL ON UPDATE CASCADE,
  ADD CONSTRAINT "holidayLeaveDeduction_userId_fkey"
    FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "holidayLeaveDeduction_balanceId_fkey"
    FOREIGN KEY ("balanceId") REFERENCES "leaveBalance"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- Backfill: existing joint-leave days deduct leave (idempotent)
UPDATE "companyHoliday" SET "deductsLeave" = true WHERE "type" = 'JOINT_LEAVE';
