-- Add source type enum and leave request link to leaveBalanceAdjustment

-- Make adjustedById nullable (future system-initiated changes won't have a human adjuster)
ALTER TABLE "leaveBalanceAdjustment" ALTER COLUMN "adjustedById" DROP NOT NULL;

-- Add source column to distinguish manual adjustments from auto-deductions on leave approval
ALTER TABLE "leaveBalanceAdjustment"
  ADD COLUMN "source" VARCHAR(20) NOT NULL DEFAULT 'ADJUSTMENT';

-- Add optional FK back to the leave request that triggered this deduction
ALTER TABLE "leaveBalanceAdjustment"
  ADD COLUMN "leaveRequestId" INT;

ALTER TABLE "leaveBalanceAdjustment"
  ADD CONSTRAINT "leaveBalanceAdjustment_leaveRequestId_fkey"
    FOREIGN KEY ("leaveRequestId") REFERENCES "leaveRequest" ("id") ON DELETE SET NULL;

CREATE INDEX "leaveBalanceAdjustment_leaveRequestId_idx"
  ON "leaveBalanceAdjustment" ("leaveRequestId");
