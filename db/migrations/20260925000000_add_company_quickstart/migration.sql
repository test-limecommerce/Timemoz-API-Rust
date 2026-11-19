-- Quickstart onboarding gate: satu baris per company yang dibuat dengan modul
-- SALES_TRACKER. Tidak ada backfill — baris yang tidak ada berarti company
-- dikecualikan dari gerbang (company lama, atau tanpa Sales Tracker).
--
-- Stempel langkah hanya pernah NULL -> waktu (COALESCE di service), jadi
-- kemajuan tidak pernah mundur. CHECK di bawah adalah lapis pertahanan kedua
-- untuk invarian "finish hanya setelah enam langkah selesai".
CREATE TABLE IF NOT EXISTS "companyQuickstart" (
  "id" SERIAL NOT NULL,
  "companyId" INTEGER NOT NULL,
  "inviteTeamAt" TIMESTAMP(3),
  "companySettingAt" TIMESTAMP(3),
  "manageRegionAt" TIMESTAMP(3),
  "manageCustomerAt" TIMESTAMP(3),
  "manageStoreAt" TIMESTAMP(3),
  "visitPlanAt" TIMESTAMP(3),
  "finishAt" TIMESTAMP(3),
  "completedAt" TIMESTAMP(3),
  "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "updatedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

  CONSTRAINT "companyQuickstart_pkey" PRIMARY KEY ("id"),
  CONSTRAINT "company_quickstart_finish_requires_steps" CHECK (
    "finishAt" IS NULL OR (
      "inviteTeamAt" IS NOT NULL AND "companySettingAt" IS NOT NULL
      AND "manageRegionAt" IS NOT NULL AND "manageCustomerAt" IS NOT NULL
      AND "manageStoreAt" IS NOT NULL AND "visitPlanAt" IS NOT NULL
    )
  )
);

CREATE UNIQUE INDEX IF NOT EXISTS "companyQuickstart_companyId_key"
  ON "companyQuickstart"("companyId");

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'companyQuickstart_companyId_fkey'
  ) THEN
    ALTER TABLE "companyQuickstart"
      ADD CONSTRAINT "companyQuickstart_companyId_fkey"
      FOREIGN KEY ("companyId") REFERENCES "company"("id")
      ON DELETE CASCADE ON UPDATE CASCADE;
  END IF;
END $$;
