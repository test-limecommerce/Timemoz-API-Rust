-- Gambar unggahan admin untuk kop/kaki PDF form kunjungan toko
-- (schema.headers / schema.footers, blok image ber-source "upload").
--
-- Tabel baru tanpa baris awal; schema lama tanpa headers/footers tetap tercetak
-- dengan kop & kaki bawaan.

-- CreateTable
CREATE TABLE IF NOT EXISTS "storeVisitFormTemplateAsset" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "templateId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "objectKey" TEXT NOT NULL,
    "contentType" VARCHAR(32) NOT NULL,
    "sizeBytes" INTEGER,
    "isConfirmed" BOOLEAN NOT NULL DEFAULT false,
    "createdById" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormTemplateAsset_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX IF NOT EXISTS "storeVisitFormTemplateAsset_uuid_key" ON "storeVisitFormTemplateAsset"("uuid");

-- CreateIndex
CREATE INDEX IF NOT EXISTS "storeVisitFormTemplateAsset_templateId_idx" ON "storeVisitFormTemplateAsset"("templateId");

-- CreateIndex
CREATE INDEX IF NOT EXISTS "storeVisitFormTemplateAsset_isConfirmed_createdAt_idx" ON "storeVisitFormTemplateAsset"("isConfirmed", "createdAt");

-- AddForeignKey
ALTER TABLE "storeVisitFormTemplateAsset" ADD CONSTRAINT "storeVisitFormTemplateAsset_templateId_fkey" FOREIGN KEY ("templateId") REFERENCES "storeVisitFormTemplate"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
