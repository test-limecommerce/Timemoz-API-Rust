-- docs/audit_log.md §2.4 (Fase 0b) — konversi tiga tabel terakhir yang masih hard
-- delete menjadi soft delete, supaya restore (§5) selalu berarti hal yang sama:
-- mematikan satu penanda, bukan menyisipkan ulang baris.
--
-- Tiga hal terjadi di sini, dan yang ketiga yang paling gampang terlupa:
--   1. kolom "deletedAt" pada storeVisitPlan, storeGroupUser, storeVisitFormTemplateUser;
--   2. unique constraint penuh milik ketiganya DIBUANG (§2.4.2) — begitu baris tidak
--      lagi pergi, constraint penuh menolak penulisan yang sah: jadwal baru untuk slot
--      yang jadwal lamanya sudah dihapus, atau sales yang dimasukkan lagi ke region
--      yang pernah mencabutnya;
--   3. penggantinya partial unique index. Prisma TIDAK bisa menyatakan partial index,
--      jadi ketiganya juga hidup di prisma/apply-partial-indexes.js dengan definisi
--      identik kata per kata — `prisma db push` akan menghapusnya tanpa bunyi, dan
--      script itulah yang memasangnya kembali.

-- AlterTable
ALTER TABLE "storeVisitPlan" ADD COLUMN "deletedAt" TIMESTAMP(3);

-- AlterTable
ALTER TABLE "storeGroupUser" ADD COLUMN "deletedAt" TIMESTAMP(3);

-- AlterTable
ALTER TABLE "storeVisitFormTemplateUser" ADD COLUMN "deletedAt" TIMESTAMP(3);

-- DropIndex + CreateIndex: storeVisitPlan (menggantikan @@unique([userId, visitDate, storeId]))
DROP INDEX IF EXISTS "storeVisitPlan_userId_visitDate_storeId_key";
CREATE UNIQUE INDEX IF NOT EXISTS "storeVisitPlan_slot_unique_active"
  ON "storeVisitPlan" ("userId", "visitDate", "storeId")
  WHERE "deletedAt" IS NULL;

-- DropIndex + CreateIndex: storeGroupUser (menggantikan @@unique([groupId, userId]))
DROP INDEX IF EXISTS "storeGroupUser_groupId_userId_key";
CREATE UNIQUE INDEX IF NOT EXISTS "storeGroupUser_member_unique_active"
  ON "storeGroupUser" ("groupId", "userId")
  WHERE "deletedAt" IS NULL;

-- DropIndex + CreateIndex: storeVisitFormTemplateUser (menggantikan @@unique([templateId, userId]))
DROP INDEX IF EXISTS "storeVisitFormTemplateUser_templateId_userId_key";
CREATE UNIQUE INDEX IF NOT EXISTS "formTemplateUser_assignment_unique_active"
  ON "storeVisitFormTemplateUser" ("templateId", "userId")
  WHERE "deletedAt" IS NULL;
