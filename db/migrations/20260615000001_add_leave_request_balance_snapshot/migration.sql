-- AlterTable: add approver note + leave balance snapshot (before/after approval) to leaveRequest
ALTER TABLE "leaveRequest" ADD COLUMN "approvalNote" TEXT;
ALTER TABLE "leaveRequest" ADD COLUMN "balanceBefore" DOUBLE PRECISION;
ALTER TABLE "leaveRequest" ADD COLUMN "balanceAfter" DOUBLE PRECISION;
