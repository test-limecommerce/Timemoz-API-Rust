-- Store Visit Form Builder — docs/store_visit_form_builder.md §3.
--
-- Catatan yang berlaku untuk seluruh file ini (§1.2): kolom "companyId", "userId",
-- "visitId", "storeId", "createdById", "publishedById", "lastEditedById", dan
-- "editedById" SENGAJA tidak punya FOREIGN KEY. Integritasnya dijaga di service,
-- bukan di database, supaya perubahan pada company/users/store/storeVisit tidak
-- pernah merambat ke modul ini. Jangan menambahkan FK ke tabel-tabel itu di
-- migration berikutnya tanpa membaca §1.2 lebih dulu.
--
-- Tidak ada partial index maupun trigger di modul ini, jadi tidak ada DDL yang bisa
-- hilang senyap saat dev DB di-`db push` (§8.1).

-- CreateEnum
CREATE TYPE "StoreVisitFormScope" AS ENUM ('all', 'targeted');

-- CreateTable
CREATE TABLE "storeVisitFormTemplate" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "name" VARCHAR(120) NOT NULL,
    "description" VARCHAR(500),
    "sortOrder" INTEGER NOT NULL DEFAULT 0,
    "scope" "StoreVisitFormScope" NOT NULL DEFAULT 'all',
    "isActive" BOOLEAN NOT NULL DEFAULT true,
    "createdById" INTEGER NOT NULL,
    "deletedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeVisitFormTemplate_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitFormTemplateVersion" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "templateId" INTEGER NOT NULL,
    "version" INTEGER NOT NULL,
    -- JSONB, bukan JSON (§3.7): urutan tampilan datang dari array "fields[]" jadi
    -- hilangnya urutan key object tidak merugikan, dan isinya jadi bisa di-query.
    "schema" JSONB NOT NULL,
    "publishedAt" TIMESTAMP(3),
    "publishedById" INTEGER,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormTemplateVersion_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitFormTemplateUser" (
    "id" SERIAL NOT NULL,
    "templateId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormTemplateUser_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitFormSubmission" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "visitId" INTEGER NOT NULL,
    "visitUuid" UUID NOT NULL,
    "userId" INTEGER NOT NULL,
    -- Denormalisasi write-once (§0.18, §1.2): ditulis saat baris dibuat dan tidak
    -- pernah diperbarui, supaya laporan menampilkan keadaan pada saat kunjungan.
    "storeId" INTEGER NOT NULL,
    "storeName" VARCHAR(120) NOT NULL,
    "visitCheckInAt" TIMESTAMP(3) NOT NULL,
    "templateId" INTEGER NOT NULL,
    "templateVersionId" INTEGER NOT NULL,
    "answers" JSONB NOT NULL DEFAULT '{}',
    "submittedAt" TIMESTAMP(3),
    "clientRequestId" UUID,
    "revisionCount" INTEGER NOT NULL DEFAULT 0,
    "lastEditedAt" TIMESTAMP(3),
    "lastEditedById" INTEGER,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeVisitFormSubmission_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitFormFile" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "submissionId" INTEGER NOT NULL,
    -- Advisory saja (§0.22) — rekonsiliasi file yatim memakai fileUuid, bukan ini.
    "fieldPath" VARCHAR(200),
    "objectKey" TEXT NOT NULL,
    "contentType" VARCHAR(128) NOT NULL,
    "sizeBytes" INTEGER,
    "isConfirmed" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormFile_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "storeVisitFormSubmissionRevision" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "submissionId" INTEGER NOT NULL,
    "revision" INTEGER NOT NULL,
    -- BEFORE-image: isi answers SEBELUM edit ini (§3.6).
    "answersBefore" JSONB NOT NULL,
    "reason" VARCHAR(500) NOT NULL,
    "editedById" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormSubmissionRevision_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormTemplate_uuid_key" ON "storeVisitFormTemplate"("uuid");

-- CreateIndex
CREATE INDEX "storeVisitFormTemplate_companyId_isActive_deletedAt_idx" ON "storeVisitFormTemplate"("companyId", "isActive", "deletedAt");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormTemplateVersion_uuid_key" ON "storeVisitFormTemplateVersion"("uuid");

-- CreateIndex
-- Nomor versi rapat & anti-balapan pada PEMBUATAN draft (§6.4).
CREATE UNIQUE INDEX "storeVisitFormTemplateVersion_templateId_version_key" ON "storeVisitFormTemplateVersion"("templateId", "version");

-- CreateIndex
CREATE INDEX "storeVisitFormTemplateVersion_templateId_publishedAt_idx" ON "storeVisitFormTemplateVersion"("templateId", "publishedAt");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormTemplateUser_templateId_userId_key" ON "storeVisitFormTemplateUser"("templateId", "userId");

-- CreateIndex
CREATE INDEX "storeVisitFormTemplateUser_userId_idx" ON "storeVisitFormTemplateUser"("userId");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormSubmission_uuid_key" ON "storeVisitFormSubmission"("uuid");

-- CreateIndex
-- Satu template maksimal sekali per kunjungan (§0.6), sekaligus sumber idempotensi draft.
CREATE UNIQUE INDEX "storeVisitFormSubmission_visitId_templateId_key" ON "storeVisitFormSubmission"("visitId", "templateId");

-- CreateIndex
-- NULL dianggap berbeda satu sama lain di Postgres, jadi draft yang belum submit
-- tidak saling bertabrakan di sini.
CREATE UNIQUE INDEX "storeVisitFormSubmission_userId_clientRequestId_key" ON "storeVisitFormSubmission"("userId", "clientRequestId");

-- CreateIndex
CREATE INDEX "storeVisitFormSubmission_companyId_submittedAt_idx" ON "storeVisitFormSubmission"("companyId", "submittedAt");

-- CreateIndex
CREATE INDEX "storeVisitFormSubmission_companyId_storeId_submittedAt_idx" ON "storeVisitFormSubmission"("companyId", "storeId", "submittedAt");

-- CreateIndex
CREATE INDEX "storeVisitFormSubmission_templateId_submittedAt_idx" ON "storeVisitFormSubmission"("templateId", "submittedAt");

-- CreateIndex
CREATE INDEX "storeVisitFormSubmission_userId_createdAt_idx" ON "storeVisitFormSubmission"("userId", "createdAt");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormFile_uuid_key" ON "storeVisitFormFile"("uuid");

-- CreateIndex
CREATE INDEX "storeVisitFormFile_submissionId_idx" ON "storeVisitFormFile"("submissionId");

-- CreateIndex
CREATE INDEX "storeVisitFormFile_isConfirmed_createdAt_idx" ON "storeVisitFormFile"("isConfirmed", "createdAt");

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitFormSubmissionRevision_uuid_key" ON "storeVisitFormSubmissionRevision"("uuid");

-- CreateIndex
-- Anti-balapan: dua edit paralel tidak bisa menghasilkan dua baris ber-revision sama (§6.8).
CREATE UNIQUE INDEX "storeVisitFormSubmissionRevision_submissionId_revision_key" ON "storeVisitFormSubmissionRevision"("submissionId", "revision");

-- CreateIndex
CREATE INDEX "storeVisitFormSubmissionRevision_submissionId_createdAt_idx" ON "storeVisitFormSubmissionRevision"("submissionId", "createdAt");

-- AddForeignKey
-- RESTRICT, bukan CASCADE (§0.16): submission menyimpan templateVersionId tanpa FK,
-- jadi cascade di sini akan menghapus satu-satunya salinan schema yang dipakai
-- merender jawaban lama.
ALTER TABLE "storeVisitFormTemplateVersion" ADD CONSTRAINT "storeVisitFormTemplateVersion_templateId_fkey" FOREIGN KEY ("templateId") REFERENCES "storeVisitFormTemplate"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
-- CASCADE, berbeda dengan version di atas: baris assignment adalah konfigurasi,
-- bukan bukti (§3.3).
ALTER TABLE "storeVisitFormTemplateUser" ADD CONSTRAINT "storeVisitFormTemplateUser_templateId_fkey" FOREIGN KEY ("templateId") REFERENCES "storeVisitFormTemplate"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitFormFile" ADD CONSTRAINT "storeVisitFormFile_submissionId_fkey" FOREIGN KEY ("submissionId") REFERENCES "storeVisitFormSubmission"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
-- RESTRICT: riwayat revisi adalah bukti, dan submission memang tidak punya jalur
-- penghapusan sama sekali (§6.6).
ALTER TABLE "storeVisitFormSubmissionRevision" ADD CONSTRAINT "storeVisitFormSubmissionRevision_submissionId_fkey" FOREIGN KEY ("submissionId") REFERENCES "storeVisitFormSubmission"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
