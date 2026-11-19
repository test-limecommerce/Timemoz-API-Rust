-- CreateEnum
CREATE TYPE "AuditEntityType" AS ENUM ('company', 'employee', 'store', 'storeGroup', 'storeVisitForm', 'storeVisitPlan', 'geofence', 'negativeZone');

-- CreateEnum
CREATE TYPE "AuditAction" AS ENUM ('create', 'update', 'delete', 'restore');

-- CreateEnum
CREATE TYPE "AuditActorType" AS ENUM ('user', 'system', 'webhook');

-- CreateTable
-- Tanpa FK ke company/users (§0.5 docs/audit_log.md) — audit yang ikut terhapus
-- bersama objek yang diauditnya bukan audit, jadi nama pelaku & label objek
-- didenormalisasi ke kolom actorName/entityLabel di bawah.
CREATE TABLE "auditLog" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "entityType" "AuditEntityType" NOT NULL,
    "entityId" INTEGER NOT NULL,
    "entityUuid" UUID,
    "entityLabel" VARCHAR(160),
    "subEntity" VARCHAR(40),
    "action" "AuditAction" NOT NULL,
    "actorType" "AuditActorType" NOT NULL DEFAULT 'user',
    "actorId" INTEGER,
    "actorName" VARCHAR(160),
    "actorRole" VARCHAR(32),
    "requestId" UUID,
    "route" VARCHAR(160),
    "ipAddress" VARCHAR(45),
    "userAgent" VARCHAR(255),
    "before" JSONB,
    "after" JSONB,
    "changedFields" TEXT[],
    "isRestorable" BOOLEAN NOT NULL DEFAULT false,
    "restoredAt" TIMESTAMP(3),
    "restoredById" INTEGER,
    "restoredByLogId" UUID,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "auditLog_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "auditLog_uuid_key" ON "auditLog"("uuid");

-- CreateIndex
-- "apa saja yang terjadi hari ini" (layar utama) — §2.2.1 docs/audit_log.md.
CREATE INDEX "auditLog_companyId_createdAt_idx" ON "auditLog"("companyId", "createdAt");

-- CreateIndex
-- Dipakai SAAT TULIS, bukan saat baca: penandaan restoredAt massal §5.5 memfilter
-- persis keempat kolom ini — §2.2.2 docs/audit_log.md.
CREATE INDEX "auditLog_companyId_entityType_entityId_createdAt_idx" ON "auditLog"("companyId", "entityType", "entityId", "createdAt");

-- CreateIndex
-- "apa saja yang diubah orang ini" — §2.2.3 docs/audit_log.md.
CREATE INDEX "auditLog_companyId_actorId_createdAt_idx" ON "auditLog"("companyId", "actorId", "createdAt");

-- CreateIndex
-- Daftar sampah (action = delete) — §2.2.4 docs/audit_log.md.
CREATE INDEX "auditLog_companyId_entityType_action_createdAt_idx" ON "auditLog"("companyId", "entityType", "action", "createdAt");

-- CreateIndex
-- "riwayat toko ini" (§6.5) dan filter entityId di §6.1. Kunci publik modul ini
-- adalah entityUuid, bukan entityId — §2.2.5 docs/audit_log.md.
CREATE INDEX "auditLog_companyId_entityType_entityUuid_createdAt_idx" ON "auditLog"("companyId", "entityType", "entityUuid", "createdAt");
