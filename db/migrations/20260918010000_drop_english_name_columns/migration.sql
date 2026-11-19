-- Nama/label berbahasa Inggris dihapus: master order, master kunjungan, dan custom field
-- (order & produk) sekarang cuma punya satu nama (`name`/`label`). Input-nya sudah dibuang dari
-- dashboard, DTO, dan response API (admin & mobile) di perubahan yang sama.
--
-- DESTRUKTIF: isi kolom-kolom ini hilang permanen. Tidak ada yang dipindah ke `name`/`label` —
-- nilai Indonesia di kolom itu tetap jadi satu-satunya nama.
--
-- `IF EXISTS` supaya aman dijalankan ulang pada database dev yang kolomnya sudah lebih dulu
-- hilang lewat `prisma db push`. DROP COLUMN di PostgreSQL cuma menandai kolom, tidak
-- me-rewrite tabel.
ALTER TABLE "orderType" DROP COLUMN IF EXISTS "nameEn";
ALTER TABLE "orderReason" DROP COLUMN IF EXISTS "nameEn";
ALTER TABLE "orderCustomFieldDefinition" DROP COLUMN IF EXISTS "labelEn";
ALTER TABLE "productCustomField" DROP COLUMN IF EXISTS "labelEn";
ALTER TABLE "visitType" DROP COLUMN IF EXISTS "nameEn";
ALTER TABLE "visitFrequency" DROP COLUMN IF EXISTS "nameEn";
