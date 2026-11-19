-- AlterTable
ALTER TABLE "company" ADD COLUMN "storeVisitEnforceGroupScope" BOOLEAN NOT NULL DEFAULT false;

-- AlterTable
ALTER TABLE "store" ADD COLUMN "groupId" INTEGER;

-- AlterTable
ALTER TABLE "storeVisit" ADD COLUMN "storeGroupId" INTEGER;

-- CreateTable
CREATE TABLE "storeGroup" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "code" VARCHAR(32) NOT NULL,
    "name" VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "isActive" BOOLEAN NOT NULL DEFAULT true,
    "createdById" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeGroup_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeGroupUser" (
    "id" SERIAL NOT NULL,
    "groupId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeGroupUser_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "storeGroup_uuid_key" ON "storeGroup"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "storeGroup_companyId_code_key" ON "storeGroup"("companyId", "code");

-- CreateIndex
CREATE INDEX "storeGroup_companyId_isActive_idx" ON "storeGroup"("companyId", "isActive");

-- CreateIndex
CREATE UNIQUE INDEX "storeGroupUser_groupId_userId_key" ON "storeGroupUser"("groupId", "userId");

-- CreateIndex
CREATE INDEX "storeGroupUser_userId_idx" ON "storeGroupUser"("userId");

-- CreateIndex
-- Menopang filter admin per region DAN klausa scope mobile dalam satu index
-- (§4.1 docs/store_visit_grouping.md).
CREATE INDEX "store_companyId_groupId_isActive_idx" ON "store"("companyId", "groupId", "isActive");

-- CreateIndex
-- Menopang breakdown per region pada rentang tanggal (§4.3 docs/store_visit_grouping.md).
CREATE INDEX "storeVisit_companyId_storeGroupId_checkInAt_idx" ON "storeVisit"("companyId", "storeGroupId", "checkInAt");

-- AddForeignKey
ALTER TABLE "storeGroup" ADD CONSTRAINT "storeGroup_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeGroup" ADD CONSTRAINT "storeGroup_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
-- Cascade: baris assignment adalah konfigurasi, bukan bukti (§1.1 docs/store_visit_grouping.md).
ALTER TABLE "storeGroupUser" ADD CONSTRAINT "storeGroupUser_groupId_fkey" FOREIGN KEY ("groupId") REFERENCES "storeGroup"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
-- Restrict: tidak ada endpoint hard delete group (§0.10) — jaring pengaman terhadap
-- DELETE manual di database yang akan meninggalkan toko menunjuk baris hantu.
ALTER TABLE "store" ADD CONSTRAINT "store_groupId_fkey" FOREIGN KEY ("groupId") REFERENCES "storeGroup"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
-- Restrict: hard delete group tidak ada (§0.10) — storeVisit.storeGroupId adalah
-- snapshot historis, dan hard delete berarti angka report lama kehilangan namanya.
ALTER TABLE "storeVisit" ADD CONSTRAINT "storeVisit_storeGroupId_fkey" FOREIGN KEY ("storeGroupId") REFERENCES "storeGroup"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
