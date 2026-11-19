-- Email pengingat quickstart (paket SALES). Lihat src/quickstart-reminder/.
--
-- `reminderEligible` DEFAULT false: semua baris companyQuickstart yang sudah ada
-- (company lama) otomatis TIDAK dikirimi. Hanya baris yang dibuat
-- `QuickstartService.initForCompany` setelah rilis ini yang bernilai true.
ALTER TABLE "companyQuickstart"
  ADD COLUMN IF NOT EXISTS "reminderEligible" BOOLEAN NOT NULL DEFAULT false;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'QuickstartReminderType') THEN
    CREATE TYPE "QuickstartReminderType" AS ENUM (
      'COMPANY_SETTING',
      'MANAGE_REGION',
      'MANAGE_CUSTOMER',
      'MANAGE_STORE',
      'VISIT_PLAN',
      'APP_INSTALL'
    );
  END IF;
END $$;

CREATE TABLE IF NOT EXISTS "companyQuickstartEmail" (
  "id" SERIAL NOT NULL,
  "companyId" INTEGER NOT NULL,
  "type" "QuickstartReminderType" NOT NULL,
  "status" VARCHAR(20) NOT NULL DEFAULT 'SENDING',
  "attempts" INTEGER NOT NULL DEFAULT 1,
  "claimedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "sentAt" TIMESTAMP(3),
  "recipients" JSONB,
  "lastError" TEXT,
  "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "updatedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

  CONSTRAINT "companyQuickstartEmail_pkey" PRIMARY KEY ("id")
);

-- Klaim anti-duplikat: satu jenis email hanya punya satu baris per company.
CREATE UNIQUE INDEX IF NOT EXISTS "companyQuickstartEmail_companyId_type_key"
  ON "companyQuickstartEmail"("companyId", "type");

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'companyQuickstartEmail_companyId_fkey'
  ) THEN
    ALTER TABLE "companyQuickstartEmail"
      ADD CONSTRAINT "companyQuickstartEmail_companyId_fkey"
      FOREIGN KEY ("companyId") REFERENCES "company"("id")
      ON DELETE CASCADE ON UPDATE CASCADE;
  END IF;
END $$;
