-- Kunci company sebuah share report saat dibuat. Sebelumnya cakupan share dibaca dari
-- company default pembuat SAAT link dibuka: pembuat multi-company yang mengganti company
-- default membuat link lamanya menampilkan data company lain.
--
-- Nullable + backfill dari keanggotaan aktif pembuat dengan urutan yang sama dengan
-- `userCompanyScope` (isDefault desc, id asc), jadi link yang masih berlaku tetap
-- menampilkan company yang sama dengan sebelum migration. Baris yang pembuatnya tak punya
-- keanggotaan aktif tetap NULL (link itu memang sudah tidak bisa dibuka).
-- Jalankan SEBELUM deploy kode yang membacanya.

-- AlterTable
ALTER TABLE "shareReport" ADD COLUMN "companyId" INTEGER;

-- Backfill
UPDATE "shareReport" AS s
SET "companyId" = m."companyId"
FROM (
  SELECT DISTINCT ON (uc."userId") uc."userId", uc."companyId"
  FROM "userCompany" AS uc
  WHERE uc."status" = 1
  ORDER BY uc."userId", uc."isDefault" DESC, uc."id" ASC
) AS m
WHERE m."userId" = s."userId"
  AND s."companyId" IS NULL;

-- CreateIndex
CREATE INDEX "shareReport_companyId_idx" ON "shareReport"("companyId");

-- AddForeignKey
ALTER TABLE "shareReport" ADD CONSTRAINT "shareReport_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;
