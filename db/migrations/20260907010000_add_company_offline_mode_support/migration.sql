-- Dukungan mode offline aplikasi mobile per company.
--
-- Satu kolom, `NOT NULL DEFAULT false`, dan default itulah seluruh isinya:
-- setiap company yang sudah ada mendarat dalam keadaan mati, jadi migration ini
-- tidak mengubah perilaku satu company pun. Admin internal menyalakannya satu
-- company sekaligus lewat `PATCH /admin-company/set/offline-mode/:uuid`, atau
-- langsung:
--
--   UPDATE "company" SET "isSupportOfflineMode" = true WHERE id = <company>;
--
-- `IF NOT EXISTS` supaya migration ini aman dijalankan ulang pada database dev
-- yang kolomnya sudah lebih dulu masuk lewat `prisma db push`.
--
-- Tidak ada rewrite tabel: sejak PostgreSQL 11, `ADD COLUMN ... NOT NULL
-- DEFAULT <konstanta>` ditulis ke katalog, bukan ke setiap baris.
ALTER TABLE "company"
  ADD COLUMN IF NOT EXISTS "isSupportOfflineMode" BOOLEAN NOT NULL DEFAULT false;
