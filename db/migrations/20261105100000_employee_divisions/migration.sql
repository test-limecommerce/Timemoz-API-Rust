-- Employee Divisions (specs/006-employee-divisions).
--
-- Additive only, no data loss; apply with psql --single-transaction after pg_dump.
--
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261105100000_employee_divisions/migration.sql
--
-- The three unique indexes in section 6 cannot be expressed in `schema.prisma`. Their
-- idempotent twins live in `prisma/apply-partial-indexes.js`; change both together.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. New enum
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TYPE "DivisionMemberRole" AS ENUM ('LEADER', 'MEMBER');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. New value for AuditEntityType (nothing below uses it)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'division';

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Per-company pilot switch
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "company" ADD COLUMN IF NOT EXISTS "divisionsEnabled" BOOLEAN NOT NULL DEFAULT false;

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Plan entitlement, gated like Audit Log
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "pricingTiers" ADD COLUMN IF NOT EXISTS "divisions" BOOLEAN NOT NULL DEFAULT false;

UPDATE "pricingTiers" SET "divisions" = "auditLog";

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Tables
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "division" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "name"        VARCHAR(60)  NOT NULL,
    "description" VARCHAR(200),
    "version"     INTEGER      NOT NULL DEFAULT 0,
    "createdById" INTEGER,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "deletedAt"   TIMESTAMP(3),

    CONSTRAINT "division_pkey" PRIMARY KEY ("id")
);

CREATE TABLE "divisionMember" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "divisionId"  INTEGER      NOT NULL,
    "userId"      INTEGER      NOT NULL,
    "role"        "DivisionMemberRole" NOT NULL,
    "createdById" INTEGER,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "deletedAt"   TIMESTAMP(3),

    CONSTRAINT "divisionMember_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "division_uuid_key" ON "division"("uuid");

CREATE UNIQUE INDEX "division_id_companyId_key" ON "division"("id", "companyId");

CREATE INDEX "division_companyId_deletedAt_idx" ON "division"("companyId", "deletedAt");

CREATE UNIQUE INDEX "divisionMember_uuid_key" ON "divisionMember"("uuid");

CREATE INDEX "divisionMember_divisionId_deletedAt_idx" ON "divisionMember"("divisionId", "deletedAt");

CREATE INDEX "divisionMember_companyId_userId_deletedAt_idx" ON "divisionMember"("companyId", "userId", "deletedAt");

ALTER TABLE "division" ADD CONSTRAINT "division_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "division" ADD CONSTRAINT "division_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

ALTER TABLE "divisionMember" ADD CONSTRAINT "divisionMember_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "divisionMember" ADD CONSTRAINT "divisionMember_divisionId_companyId_fkey" FOREIGN KEY ("divisionId", "companyId") REFERENCES "division"("id", "companyId") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "divisionMember" ADD CONSTRAINT "divisionMember_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "divisionMember" ADD CONSTRAINT "divisionMember_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. Partial / expression unique indexes (research R2). Not plain @@unique: a
--    soft-deleted row must not block a new live row.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "division_company_name_unique_active" ON "division" ("companyId", LOWER("name")) WHERE "deletedAt" IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS "divisionMember_company_user_unique_active" ON "divisionMember" ("companyId", "userId") WHERE "deletedAt" IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS "divisionMember_one_leader_active" ON "divisionMember" ("divisionId") WHERE "role" = 'LEADER' AND "deletedAt" IS NULL;
