-- CreateEnum
CREATE TYPE "LeaveType" AS ENUM ('ANNUAL_LEAVE', 'SICK_LEAVE', 'MARRIAGE_LEAVE', 'MATERNITY_LEAVE', 'BEREAVEMENT_LEAVE', 'UNPAID_LEAVE');

-- CreateEnum
CREATE TYPE "LeaveStatus" AS ENUM ('PENDING', 'APPROVED', 'REJECTED', 'NEEDS_REVISION');

-- CreateEnum
CREATE TYPE "LeaveDayType" AS ENUM ('FULL_DAY', 'HALF_DAY_MORNING', 'HALF_DAY_AFTERNOON');

-- CreateTable
CREATE TABLE "leavePolicy" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "annualLeaveDays" INTEGER NOT NULL DEFAULT 12,
    "sickLeaveDays" INTEGER,
    "carryOverEnabled" BOOLEAN NOT NULL DEFAULT false,
    "maxCarryOverDays" INTEGER,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "leavePolicy_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "employeeLeavePolicy" (
    "id" SERIAL NOT NULL,
    "userId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "annualLeaveDays" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "employeeLeavePolicy_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "leaveBalance" (
    "id" SERIAL NOT NULL,
    "userId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "year" INTEGER NOT NULL,
    "annualLeaveTotal" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "annualLeaveUsed" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "sickLeaveTotal" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "sickLeaveUsed" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "carryOverDays" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "carryOverUsed" DOUBLE PRECISION NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "leaveBalance_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "leaveRequest" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "userId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "leaveType" "LeaveType" NOT NULL,
    "startDate" DATE NOT NULL,
    "endDate" DATE NOT NULL,
    "startDayType" "LeaveDayType" NOT NULL DEFAULT 'FULL_DAY',
    "endDayType" "LeaveDayType" NOT NULL DEFAULT 'FULL_DAY',
    "totalDays" DOUBLE PRECISION NOT NULL,
    "reason" TEXT NOT NULL,
    "documentKey" VARCHAR(500),
    "status" "LeaveStatus" NOT NULL DEFAULT 'PENDING',
    "rejectionReason" TEXT,
    "revisionNote" TEXT,
    "reviewedById" INTEGER,
    "reviewedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "leaveRequest_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "leavePolicy_companyId_key" ON "leavePolicy"("companyId");

-- CreateIndex
CREATE UNIQUE INDEX "employeeLeavePolicy_userId_companyId_key" ON "employeeLeavePolicy"("userId", "companyId");

-- CreateIndex
CREATE INDEX "employeeLeavePolicy_companyId_idx" ON "employeeLeavePolicy"("companyId");

-- CreateIndex
CREATE UNIQUE INDEX "leaveBalance_userId_companyId_year_key" ON "leaveBalance"("userId", "companyId", "year");

-- CreateIndex
CREATE INDEX "leaveBalance_companyId_year_idx" ON "leaveBalance"("companyId", "year");

-- CreateIndex
CREATE UNIQUE INDEX "leaveRequest_uuid_key" ON "leaveRequest"("uuid");

-- CreateIndex
CREATE INDEX "leaveRequest_userId_status_idx" ON "leaveRequest"("userId", "status");

-- CreateIndex
CREATE INDEX "leaveRequest_companyId_status_idx" ON "leaveRequest"("companyId", "status");

-- CreateIndex
CREATE INDEX "leaveRequest_companyId_startDate_endDate_idx" ON "leaveRequest"("companyId", "startDate", "endDate");

-- CreateIndex
CREATE INDEX "leaveRequest_userId_startDate_idx" ON "leaveRequest"("userId", "startDate");

-- AddForeignKey
ALTER TABLE "leavePolicy" ADD CONSTRAINT "leavePolicy_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "employeeLeavePolicy" ADD CONSTRAINT "employeeLeavePolicy_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "employeeLeavePolicy" ADD CONSTRAINT "employeeLeavePolicy_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "leaveBalance" ADD CONSTRAINT "leaveBalance_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "leaveBalance" ADD CONSTRAINT "leaveBalance_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "leaveRequest" ADD CONSTRAINT "leaveRequest_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "leaveRequest" ADD CONSTRAINT "leaveRequest_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "leaveRequest" ADD CONSTRAINT "leaveRequest_reviewedById_fkey" FOREIGN KEY ("reviewedById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;
