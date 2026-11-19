-- specs/007-system-admin-operations, migration 2 (data-model.md §2): manual subscription.
-- Data loss: none. Constant DEFAULTs only; every existing subscription becomes PAYPAL, every invoice USD.
-- The partial unique index below cannot be expressed in schema.prisma; its idempotent twin lives in
-- prisma/apply-partial-indexes.js. Change both together.
-- Apply after pg_dump:
--   psql "$DB" --single-transaction -v ON_ERROR_STOP=1 -f prisma/migrations/20261112100100_manual_subscription/migration.sql

CREATE TYPE "SubscriptionBillingSource" AS ENUM ('PAYPAL', 'MANUAL');
ALTER TABLE "subscriptions"
  ADD COLUMN "billingSource" "SubscriptionBillingSource" NOT NULL DEFAULT 'PAYPAL',
  ADD COLUMN "manualReminderForEndDate" TIMESTAMP(3);
CREATE INDEX "subscriptions_billingSource_status_endDate_idx"
  ON "subscriptions"("billingSource", "status", "endDate");

CREATE TABLE "companyPlanOverride" (
  "id"                         SERIAL PRIMARY KEY,
  "companyId"                  INTEGER NOT NULL,
  "maxStores"                  INTEGER,
  "totalGeofence"              INTEGER,
  "maxAutoCheckIns"            INTEGER,
  "maxFormTemplates"           INTEGER,
  "visitHistoryDays"           INTEGER,
  "auditLog"                   BOOLEAN,
  "form"                       BOOLEAN,
  "timelineRoute"              BOOLEAN,
  "product"                    BOOLEAN,
  "order"                      BOOLEAN,
  "divisions"                  BOOLEAN,
  "maxProject"                 INTEGER,
  "dataHistoryMonths"          INTEGER,
  "reportExportsPerMonth"      INTEGER,
  "screenshotRetentionDays"    INTEGER,
  "screenshotResolution"       VARCHAR(20),
  "budgetAlertDeliveryChannel" "BudgetAlertDeliveryChannel",
  "clientProofExportAllowed"   BOOLEAN,
  "note"                       TEXT,
  "updatedByOperatorId"        INTEGER,
  "createdAt"                  TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "updatedAt"                  TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT "companyPlanOverride_companyId_fkey" FOREIGN KEY ("companyId")
    REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  CONSTRAINT "companyPlanOverride_updatedByOperatorId_fkey" FOREIGN KEY ("updatedByOperatorId")
    REFERENCES "operators"("id") ON DELETE SET NULL ON UPDATE CASCADE
);
CREATE UNIQUE INDEX "companyPlanOverride_companyId_key" ON "companyPlanOverride"("companyId");

ALTER TABLE "invoices"
  ADD COLUMN "currency"             VARCHAR(3) NOT NULL DEFAULT 'USD',
  ADD COLUMN "paymentMethod"        VARCHAR(30),
  ADD COLUMN "paymentReference"     VARCHAR(100),
  ADD COLUMN "paidAt"               TIMESTAMP(3),
  ADD COLUMN "periodStart"          TIMESTAMP(3),
  ADD COLUMN "periodEnd"            TIMESTAMP(3),
  ADD COLUMN "note"                 TEXT,
  ADD COLUMN "recordedByOperatorId" INTEGER,
  ADD CONSTRAINT "invoices_recordedByOperatorId_fkey" FOREIGN KEY ("recordedByOperatorId")
    REFERENCES "operators"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- partial (schema.prisma cannot express it; also in prisma/apply-partial-indexes.js)
CREATE UNIQUE INDEX "invoices_manual_reference_key"
  ON "invoices"("subscriptionId", lower("paymentReference"))
  WHERE "paymentProvider" = 'manual' AND "status" <> 'CANCELLED' AND "paymentReference" IS NOT NULL;
