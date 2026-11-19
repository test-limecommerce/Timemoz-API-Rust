ALTER TABLE "users"
  ADD COLUMN "productTourStatus"    VARCHAR(20) DEFAULT 'NOT_STARTED',
  ADD COLUMN "productTourStep"      VARCHAR(64),
  ADD COLUMN "productTourUpdatedAt" TIMESTAMP(3);

-- Existing customers must never be dropped into the tour on deploy.
UPDATE "users"
   SET "productTourStatus" = 'COMPLETED',
       "productTourUpdatedAt" = NOW()
 WHERE "createdAt" < NOW();

CREATE INDEX "users_productTourStatus_idx" ON "users"("productTourStatus");
