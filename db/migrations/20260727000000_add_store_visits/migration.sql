-- CreateEnum
CREATE TYPE "StoreVisitStatus" AS ENUM ('success', 'closed', 'moved', 'noOwner', 'rejected', 'other');

-- CreateEnum
CREATE TYPE "StoreVisitState" AS ENUM ('active', 'completed', 'cancelled');

-- CreateEnum
CREATE TYPE "StoreVisitMethod" AS ENUM ('gps', 'qr', 'manual');

-- CreateEnum
CREATE TYPE "GpsValidation" AS ENUM ('valid', 'outside', 'mock', 'lowAccuracy');

-- CreateEnum
CREATE TYPE "StoreVisitPhotoKind" AS ENUM ('store', 'selfie', 'result');

-- AlterTable
ALTER TABLE "company"
  ADD COLUMN "storeVisitDefaultRadiusMeters" INTEGER NOT NULL DEFAULT 150,
  ADD COLUMN "storeVisitMaxAccuracyMeters"   INTEGER NOT NULL DEFAULT 100,
  ADD COLUMN "storeVisitBlockMockLocation"   BOOLEAN NOT NULL DEFAULT true,
  ADD COLUMN "storeVisitRequirePhotos"       BOOLEAN NOT NULL DEFAULT true,
  ADD COLUMN "storeVisitAllowOverride"       BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN "storeVisitMaxDurationHours"    INTEGER NOT NULL DEFAULT 8;

-- CreateTable
CREATE TABLE "store" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "code" VARCHAR(32) NOT NULL,
    "name" VARCHAR(120) NOT NULL,
    "ownerName" VARCHAR(80) NOT NULL,
    "phone" VARCHAR(20),
    "address" VARCHAR(255) NOT NULL,
    "lat" DOUBLE PRECISION NOT NULL,
    "lng" DOUBLE PRECISION NOT NULL,
    "radiusMeters" INTEGER,
    "qrToken" VARCHAR(64),
    "isActive" BOOLEAN NOT NULL DEFAULT true,
    "totalVisits" INTEGER NOT NULL DEFAULT 0,
    "lastVisitAt" TIMESTAMP(3),
    "lastVisitStatus" "StoreVisitStatus",
    "createdById" INTEGER NOT NULL,
    "deletedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "store_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisit" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "storeId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "visitNumber" VARCHAR(32) NOT NULL,
    "clientRequestId" UUID NOT NULL,
    "state" "StoreVisitState" NOT NULL DEFAULT 'active',
    "status" "StoreVisitStatus",
    "note" TEXT,
    "tags" TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    "durationSeconds" INTEGER,
    "checkInAt" TIMESTAMP(3) NOT NULL,
    "checkInClientAt" TIMESTAMP(3),
    "checkInLat" DOUBLE PRECISION NOT NULL,
    "checkInLng" DOUBLE PRECISION NOT NULL,
    "checkInAccuracy" DOUBLE PRECISION NOT NULL,
    "checkInDistance" INTEGER NOT NULL,
    "checkInMethod" "StoreVisitMethod" NOT NULL DEFAULT 'gps',
    "checkInGpsValidation" "GpsValidation" NOT NULL,
    "checkInIsMock" BOOLEAN NOT NULL DEFAULT false,
    "checkInIsOverride" BOOLEAN NOT NULL DEFAULT false,
    "checkInOverrideReason" VARCHAR(255),
    "hasGpsIssue" BOOLEAN NOT NULL DEFAULT false,
    "checkOutAt" TIMESTAMP(3),
    "checkOutClientAt" TIMESTAMP(3),
    "checkOutLat" DOUBLE PRECISION,
    "checkOutLng" DOUBLE PRECISION,
    "checkOutAccuracy" DOUBLE PRECISION,
    "checkOutDistance" INTEGER,
    "checkOutGpsValidation" "GpsValidation",
    "checkOutIsMock" BOOLEAN,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeVisit_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitPhoto" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "visitId" INTEGER NOT NULL,
    "kind" "StoreVisitPhotoKind" NOT NULL,
    "objectKey" TEXT NOT NULL,
    "sizeBytes" INTEGER,
    "contentType" VARCHAR(32) NOT NULL,
    "isConfirmed" BOOLEAN NOT NULL DEFAULT false,
    "takenAt" TIMESTAMP(3),
    "lat" DOUBLE PRECISION,
    "lng" DOUBLE PRECISION,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitPhoto_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitCounter" (
    "companyId" INTEGER NOT NULL,
    "lastNumber" INTEGER NOT NULL DEFAULT 0,

    CONSTRAINT "storeVisitCounter_pkey" PRIMARY KEY ("companyId")
);

-- CreateIndex
CREATE UNIQUE INDEX "store_uuid_key" ON "store"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "store_qrToken_key" ON "store"("qrToken");

-- CreateIndex
CREATE INDEX "store_companyId_isActive_idx" ON "store"("companyId", "isActive");

-- CreateIndex
CREATE INDEX "store_companyId_deletedAt_idx" ON "store"("companyId", "deletedAt");

-- CreateIndex
CREATE INDEX "store_companyId_lat_lng_idx" ON "store"("companyId", "lat", "lng");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisit_uuid_key" ON "storeVisit"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisit_companyId_visitNumber_key" ON "storeVisit"("companyId", "visitNumber");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisit_userId_clientRequestId_key" ON "storeVisit"("userId", "clientRequestId");

-- CreateIndex
CREATE INDEX "storeVisit_companyId_checkInAt_idx" ON "storeVisit"("companyId", "checkInAt");

-- CreateIndex
CREATE INDEX "storeVisit_userId_checkInAt_idx" ON "storeVisit"("userId", "checkInAt");

-- CreateIndex
CREATE INDEX "storeVisit_storeId_checkInAt_idx" ON "storeVisit"("storeId", "checkInAt");

-- CreateIndex
CREATE INDEX "storeVisit_companyId_state_idx" ON "storeVisit"("companyId", "state");

-- CreateIndex
CREATE INDEX "storeVisit_companyId_hasGpsIssue_idx" ON "storeVisit"("companyId", "hasGpsIssue");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitPhoto_uuid_key" ON "storeVisitPhoto"("uuid");

-- CreateIndex
CREATE INDEX "storeVisitPhoto_visitId_kind_idx" ON "storeVisitPhoto"("visitId", "kind");

-- CreateIndex
CREATE INDEX "storeVisitPhoto_isConfirmed_createdAt_idx" ON "storeVisitPhoto"("isConfirmed", "createdAt");

-- Partial index (1): satu kunjungan aktif per sales.
-- Prisma tidak mendukung partial index, jadi index ini ditulis manual dan TIDAK boleh
-- dihapus saat schema di-regenerate. Ini satu-satunya penjamin nyata aturan §5.2.2 —
-- pengecekan findFirst() di aplikasi adalah TOCTOU. Sengaja hanya ("userId"), bukan
-- ("companyId", "userId"): satu orang hanya bisa berada di satu tempat.
CREATE UNIQUE INDEX "storeVisit_one_active_per_user"
  ON "storeVisit" ("userId")
  WHERE "state" = 'active';

-- Partial index (2): kode store unik per company, HANYA untuk baris yang belum di-soft-delete,
-- supaya kode store yang sudah dihapus bisa dipakai ulang.
-- Konsekuensi: semua query "store" wajib menyertakan "deletedAt" IS NULL.
CREATE UNIQUE INDEX "store_code_unique_per_company"
  ON "store" ("companyId", "code")
  WHERE "deletedAt" IS NULL;

-- AddForeignKey
ALTER TABLE "store" ADD CONSTRAINT "store_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "store" ADD CONSTRAINT "store_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisit" ADD CONSTRAINT "storeVisit_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisit" ADD CONSTRAINT "storeVisit_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisit" ADD CONSTRAINT "storeVisit_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitPhoto" ADD CONSTRAINT "storeVisitPhoto_visitId_fkey" FOREIGN KEY ("visitId") REFERENCES "storeVisit"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitCounter" ADD CONSTRAINT "storeVisitCounter_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
