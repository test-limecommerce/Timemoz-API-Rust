-- Add LeaveResetType enum
CREATE TYPE "LeaveResetType" AS ENUM ('ANNUAL', 'ANNIVERSARY');

-- Add LeaveEntitlementType enum
CREATE TYPE "LeaveEntitlementType" AS ENUM ('UNIFORM', 'TIER_BASED');

-- Add new columns to leavePolicy
ALTER TABLE "leavePolicy"
  ADD COLUMN "carryOverExpiryMonth" INTEGER,
  ADD COLUMN "resetType"            "LeaveResetType"        NOT NULL DEFAULT 'ANNUAL',
  ADD COLUMN "entitlementType"      "LeaveEntitlementType"  NOT NULL DEFAULT 'UNIFORM';

-- Create leaveTier table
CREATE TABLE "leaveTier" (
  "id"              SERIAL          PRIMARY KEY,
  "companyId"       INTEGER         NOT NULL,
  "policyId"        INTEGER         NOT NULL,
  "name"            VARCHAR(100)    NOT NULL,
  "minTenureMonths" INTEGER         NOT NULL DEFAULT 0,
  "maxTenureMonths" INTEGER,
  "annualLeaveDays" INTEGER         NOT NULL,
  "createdAt"       TIMESTAMP(3)    NOT NULL DEFAULT NOW(),
  "updatedAt"       TIMESTAMP(3),
  CONSTRAINT "leaveTier_policyId_fkey"  FOREIGN KEY ("policyId")  REFERENCES "leavePolicy"("id") ON DELETE CASCADE,
  CONSTRAINT "leaveTier_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id")
);

CREATE INDEX "leaveTier_companyId_idx" ON "leaveTier"("companyId");
CREATE INDEX "leaveTier_policyId_idx"  ON "leaveTier"("policyId");

-- Create employeeLeaveTier table
CREATE TABLE "employeeLeaveTier" (
  "id"         SERIAL        PRIMARY KEY,
  "userId"     INTEGER       NOT NULL,
  "companyId"  INTEGER       NOT NULL,
  "tierId"     INTEGER       NOT NULL,
  "assignedAt" TIMESTAMP(3)  NOT NULL DEFAULT NOW(),
  CONSTRAINT "employeeLeaveTier_tierId_fkey"           FOREIGN KEY ("tierId")    REFERENCES "leaveTier"("id") ON DELETE CASCADE,
  CONSTRAINT "employeeLeaveTier_userId_fkey"            FOREIGN KEY ("userId")    REFERENCES "users"("id")     ON DELETE CASCADE,
  CONSTRAINT "employeeLeaveTier_companyId_fkey"         FOREIGN KEY ("companyId") REFERENCES "company"("id"),
  CONSTRAINT "employeeLeaveTier_userId_companyId_key"   UNIQUE ("userId", "companyId")
);

CREATE INDEX "employeeLeaveTier_companyId_idx" ON "employeeLeaveTier"("companyId");
CREATE INDEX "employeeLeaveTier_tierId_idx"    ON "employeeLeaveTier"("tierId");
