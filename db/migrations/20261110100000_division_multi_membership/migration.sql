-- Employee Divisions, Amendment A1 (specs/006-employee-divisions plan.md A1.2).
--
-- Rules after this migration:
--   - a division can have several Leaders (at least one is still enforced in code);
--   - one person can be in several divisions, and lead several;
--   - one person still has at most ONE live row per division.
--
-- Data loss: none. Step 1 only soft-deletes duplicate live rows (no hard delete); on
-- production the old `divisionMember_company_user_unique_active` index already made
-- such duplicates impossible, so it should report `UPDATE 0`.
--
-- Apply with psql --single-transaction after pg_dump:
--
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261110100000_division_multi_membership/migration.sql
--
-- The new index cannot be expressed in `schema.prisma`; its idempotent twin (and the drop
-- of the two v1 indexes) lives in `prisma/apply-partial-indexes.js`. Change both together.
--
-- Rollback is NOT free: once a person is in two divisions, or a division has two Leaders,
-- recreating the v1 indexes fails. Rolling back needs a data cleanup decision first.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Duplicate live (divisionId, userId) rows: keep the newest, soft-delete the rest.
--    Impossible under the v1 index, but a dev DB built with `db push` has no partial
--    indexes.
-- ─────────────────────────────────────────────────────────────────────────────

UPDATE "divisionMember" SET "deletedAt" = NOW()
WHERE "id" IN (
  SELECT "id" FROM (
    SELECT "id", ROW_NUMBER() OVER (PARTITION BY "divisionId", "userId" ORDER BY "id" DESC) AS rn
    FROM "divisionMember" WHERE "deletedAt" IS NULL
  ) d WHERE d.rn > 1
);

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. New guard first, so there is no window without one (research R6 T9).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "divisionMember_division_user_unique_active" ON "divisionMember" ("divisionId", "userId") WHERE "deletedAt" IS NULL;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Drop the two v1 rules (one division per person, one Leader per division).
-- ─────────────────────────────────────────────────────────────────────────────

DROP INDEX IF EXISTS "divisionMember_company_user_unique_active";

DROP INDEX IF EXISTS "divisionMember_one_leader_active";
