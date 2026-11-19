-- CreateEnum
CREATE TYPE "LeaveBalanceSource" AS ENUM ('ANNUAL', 'SICK');

-- CreateTable companyLeaveType
CREATE TABLE "companyLeaveType" (
  "id"               SERIAL                NOT NULL,
  "companyId"        INTEGER               NOT NULL,
  "name"             VARCHAR(100)          NOT NULL,
  "code"             VARCHAR(50)           NOT NULL,
  "deductBalance"    BOOLEAN               NOT NULL DEFAULT true,
  "balanceSource"    "LeaveBalanceSource"  NOT NULL DEFAULT 'ANNUAL',
  "requiresDocument" BOOLEAN               NOT NULL DEFAULT false,
  "annualQuota"      INTEGER,
  "isActive"         BOOLEAN               NOT NULL DEFAULT true,
  "isDefault"        BOOLEAN               NOT NULL DEFAULT false,
  "createdAt"        TIMESTAMP(3)          NOT NULL DEFAULT NOW(),
  "updatedAt"        TIMESTAMP(3),

  CONSTRAINT "companyLeaveType_pkey" PRIMARY KEY ("id")
);

-- Foreign key: companyLeaveType -> company
ALTER TABLE "companyLeaveType"
  ADD CONSTRAINT "companyLeaveType_companyId_fkey"
  FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- Unique + index
CREATE UNIQUE INDEX "companyLeaveType_companyId_code_key" ON "companyLeaveType"("companyId", "code");
CREATE INDEX "companyLeaveType_companyId_idx" ON "companyLeaveType"("companyId");

-- Make leaveRequest.leaveType nullable (new records will use leaveTypeId instead)
ALTER TABLE "leaveRequest" ALTER COLUMN "leaveType" DROP NOT NULL;

-- Add leaveTypeId column to leaveRequest
ALTER TABLE "leaveRequest" ADD COLUMN "leaveTypeId" INTEGER;

ALTER TABLE "leaveRequest"
  ADD CONSTRAINT "leaveRequest_leaveTypeId_fkey"
  FOREIGN KEY ("leaveTypeId") REFERENCES "companyLeaveType"("id") ON DELETE SET NULL ON UPDATE CASCADE;

CREATE INDEX "leaveRequest_leaveTypeId_idx" ON "leaveRequest"("leaveTypeId");
