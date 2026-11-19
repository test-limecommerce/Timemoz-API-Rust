-- CreateEnum
CREATE TYPE "StoreVisitAuditEvent" AS ENUM ('mockLocationBlocked', 'outOfRadius', 'overrideUsed');

-- CreateTable
CREATE TABLE "storeVisitAuditLog" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "storeId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    -- NULL untuk percobaan yang ditolak: tidak ada kunjungan yang terbentuk.
    "visitId" INTEGER,
    "event" "StoreVisitAuditEvent" NOT NULL,
    "lat" DOUBLE PRECISION NOT NULL,
    "lng" DOUBLE PRECISION NOT NULL,
    "accuracyMeters" DOUBLE PRECISION NOT NULL,
    "distanceMeters" INTEGER NOT NULL,
    "isMock" BOOLEAN NOT NULL DEFAULT false,
    "reason" VARCHAR(255),
    "clientRequestId" UUID,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitAuditLog_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitAuditLog_uuid_key" ON "storeVisitAuditLog"("uuid");

-- CreateIndex
CREATE INDEX "storeVisitAuditLog_companyId_createdAt_idx" ON "storeVisitAuditLog"("companyId", "createdAt");

-- CreateIndex
CREATE INDEX "storeVisitAuditLog_storeId_createdAt_idx" ON "storeVisitAuditLog"("storeId", "createdAt");

-- CreateIndex
CREATE INDEX "storeVisitAuditLog_userId_createdAt_idx" ON "storeVisitAuditLog"("userId", "createdAt");

-- CreateIndex
CREATE INDEX "storeVisitAuditLog_companyId_event_createdAt_idx" ON "storeVisitAuditLog"("companyId", "event", "createdAt");

-- AddForeignKey
ALTER TABLE "storeVisitAuditLog" ADD CONSTRAINT "storeVisitAuditLog_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitAuditLog" ADD CONSTRAINT "storeVisitAuditLog_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitAuditLog" ADD CONSTRAINT "storeVisitAuditLog_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
-- SET NULL, bukan CASCADE: audit yang ikut terhapus bersama objek yang diauditnya bukan audit.
ALTER TABLE "storeVisitAuditLog" ADD CONSTRAINT "storeVisitAuditLog_visitId_fkey" FOREIGN KEY ("visitId") REFERENCES "storeVisit"("id") ON DELETE SET NULL ON UPDATE CASCADE;
