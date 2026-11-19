-- specs/007-system-admin-operations, migration 1 (data-model.md §1): operator roles + audit log.
-- Data loss: none. Existing operators are backfilled to OWNER, then the column becomes NOT NULL.
-- Apply after pg_dump:
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261112100000_operator_roles_and_audit/migration.sql

CREATE TYPE "OperatorRole" AS ENUM ('OWNER', 'FINANCE', 'SUPPORT');
ALTER TABLE "operators" ADD COLUMN "role" "OperatorRole";
UPDATE "operators" SET "role" = 'OWNER' WHERE "role" IS NULL;
ALTER TABLE "operators" ALTER COLUMN "role" SET NOT NULL;
ALTER TABLE "operators" ALTER COLUMN "role" SET DEFAULT 'SUPPORT';

CREATE TABLE "operatorAuditLog" (
  "id"         SERIAL PRIMARY KEY,
  "uuid"       UUID NOT NULL DEFAULT gen_random_uuid(),
  "operatorId" INTEGER NOT NULL,
  "action"     VARCHAR(64) NOT NULL,
  "targetType" VARCHAR(32) NOT NULL,
  "targetUuid" UUID,
  "companyId"  INTEGER,
  "before"     JSONB,
  "after"      JSONB,
  "reason"     TEXT,
  "ipAddress"  VARCHAR(45),
  "userAgent"  VARCHAR(255),
  "createdAt"  TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT "operatorAuditLog_operatorId_fkey" FOREIGN KEY ("operatorId")
    REFERENCES "operators"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT "operatorAuditLog_companyId_fkey" FOREIGN KEY ("companyId")
    REFERENCES "company"("id") ON DELETE SET NULL ON UPDATE CASCADE
);
CREATE UNIQUE INDEX "operatorAuditLog_uuid_key" ON "operatorAuditLog"("uuid");
CREATE INDEX "operatorAuditLog_companyId_createdAt_idx"  ON "operatorAuditLog"("companyId", "createdAt");
CREATE INDEX "operatorAuditLog_operatorId_createdAt_idx" ON "operatorAuditLog"("operatorId", "createdAt");
CREATE INDEX "operatorAuditLog_targetUuid_createdAt_idx" ON "operatorAuditLog"("targetUuid", "createdAt");
CREATE INDEX "operatorAuditLog_action_createdAt_idx"     ON "operatorAuditLog"("action", "createdAt");
