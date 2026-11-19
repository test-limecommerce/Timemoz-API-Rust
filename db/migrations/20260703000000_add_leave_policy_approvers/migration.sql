CREATE TABLE "leavePolicyApprover" (
  "id"        SERIAL PRIMARY KEY,
  "policyId"  INTEGER NOT NULL,
  "userId"    INTEGER NOT NULL,
  "companyId" INTEGER NOT NULL,
  "createdAt" TIMESTAMP NOT NULL DEFAULT NOW(),
  CONSTRAINT "leavePolicyApprover_policyId_fkey" FOREIGN KEY ("policyId") REFERENCES "leavePolicy"("id") ON DELETE CASCADE,
  CONSTRAINT "leavePolicyApprover_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE,
  CONSTRAINT "leavePolicyApprover_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id")
);

CREATE UNIQUE INDEX "leavePolicyApprover_policyId_userId_key" ON "leavePolicyApprover"("policyId", "userId");

CREATE INDEX "leavePolicyApprover_companyId_idx" ON "leavePolicyApprover"("companyId");
