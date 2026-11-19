-- specs/007-system-admin-operations, migration 3 (data-model.md §3): company owner + global data.
-- Data loss: none. The owner backfill only fills NULLs.
-- The partial unique index below cannot be expressed in schema.prisma; its idempotent twin lives in
-- prisma/apply-partial-indexes.js. Change both together.
-- Apply after pg_dump:
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261112100200_admin_global_data/migration.sql

ALTER TABLE "company" ADD COLUMN "ownerUserId" INTEGER;
ALTER TABLE "company" ADD CONSTRAINT "company_ownerUserId_fkey" FOREIGN KEY ("ownerUserId")
  REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;
-- backfill: same rule as /auth/me is_owner (active SUPER_ADMIN, earliest userCompany row)
UPDATE "company" c SET "ownerUserId" = o."userId"
FROM (
  SELECT DISTINCT ON (uc."companyId") uc."companyId", uc."userId"
  FROM "userCompany" uc
  JOIN "userAccessLevel" ual ON ual."userId" = uc."userId" AND ual."companyId" = uc."companyId" AND ual."status" = 1
  JOIN "accessLevel" al ON al."id" = ual."accessLevelId" AND al."code" = 'SUPER_ADMIN'
  ORDER BY uc."companyId", uc."createdAt" ASC, uc."id" ASC
) o
WHERE o."companyId" = c."id" AND c."ownerUserId" IS NULL;

CREATE TABLE "nationalHoliday" (
  "id"          SERIAL PRIMARY KEY,
  "uuid"        UUID NOT NULL DEFAULT gen_random_uuid(),
  "date"        DATE NOT NULL,
  "name"        VARCHAR(200) NOT NULL,
  "type"        "HolidayType" NOT NULL,
  "year"        INTEGER NOT NULL,
  "isEstimated" BOOLEAN NOT NULL DEFAULT false,
  "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "updatedAt"   TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE UNIQUE INDEX "nationalHoliday_uuid_key"      ON "nationalHoliday"("uuid");
CREATE UNIQUE INDEX "nationalHoliday_date_name_key" ON "nationalHoliday"("date", "name");
CREATE INDEX "nationalHoliday_year_idx"             ON "nationalHoliday"("year");

CREATE TYPE "AdminJobType"   AS ENUM ('BACKUP_COMPANY', 'RESTORE_COMPANY', 'MERGE_COMPANY', 'APPLY_HOLIDAYS');
CREATE TYPE "AdminJobStatus" AS ENUM ('QUEUED', 'RUNNING', 'DONE', 'FAILED');
CREATE TABLE "adminJob" (
  "id"         SERIAL PRIMARY KEY,
  "uuid"       UUID NOT NULL DEFAULT gen_random_uuid(),
  "type"       "AdminJobType" NOT NULL,
  "companyId"  INTEGER,
  "params"     JSONB NOT NULL,
  "paramsHash" VARCHAR(64) NOT NULL,
  "dryRun"     BOOLEAN NOT NULL,
  "status"     "AdminJobStatus" NOT NULL DEFAULT 'QUEUED',
  "result"     JSONB,
  "log"        TEXT,
  "operatorId" INTEGER NOT NULL,
  "startedAt"  TIMESTAMP(3),
  "finishedAt" TIMESTAMP(3),
  "createdAt"  TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT "adminJob_operatorId_fkey" FOREIGN KEY ("operatorId")
    REFERENCES "operators"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT "adminJob_companyId_fkey" FOREIGN KEY ("companyId")
    REFERENCES "company"("id") ON DELETE SET NULL ON UPDATE CASCADE
);
CREATE UNIQUE INDEX "adminJob_uuid_key" ON "adminJob"("uuid");
CREATE INDEX "adminJob_companyId_status_idx" ON "adminJob"("companyId", "status");
CREATE INDEX "adminJob_type_paramsHash_finishedAt_idx" ON "adminJob"("type", "paramsHash", "finishedAt");
-- partial (also in prisma/apply-partial-indexes.js)
CREATE UNIQUE INDEX "adminJob_one_active_per_company"
  ON "adminJob"("companyId") WHERE "status" IN ('QUEUED', 'RUNNING');
