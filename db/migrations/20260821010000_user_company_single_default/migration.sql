-- Satu company default per user — dijadikan jaminan, bukan kebetulan.
--
-- Sebelum ini "company mana yang berlaku untuk user ini" dijawab tiga cara berbeda
-- di dalam kode (lihat docs/organization_settings.md §Company default). Dua di
-- antaranya bersandar pada `isDefault`, dan tidak ada apa pun yang menjamin sebuah
-- user punya tepat satu baris ber-`isDefault`.

-- 1. User tanpa satu pun baris default mendapatkannya di keanggotaan tertua.
--    Idempoten: baris yang sudah punya default tidak tersentuh.
--
--    Tanpa ini `GET /auth/me` menjawab "user not found" untuk mereka — nested
--    select-nya memfilter `isDefault: true` dan tidak punya jalan mundur. Kode
--    sekarang punya (users.service.ts#findOne mengurutkan, bukan memfilter), jadi
--    baris ini merapikan datanya, bukan menambal aplikasinya.
UPDATE "userCompany"
SET "isDefault" = true
WHERE "id" IN (
  SELECT MIN("id")
  FROM "userCompany"
  GROUP BY "userId"
  HAVING COUNT(*) FILTER (WHERE "isDefault") = 0
);

-- 2. Dua default untuk satu user tidak bisa lagi terjadi.
--
--    Partial index tidak bisa dinyatakan di schema.prisma, jadi ia hanya hidup di
--    sini dan di prisma/apply-partial-indexes.js. `prisma db push` MENGHAPUSNYA
--    tanpa bunyi — jalankan `npm run db:partial-indexes` sesudahnya.
--
--    Ini "paling banyak satu", bukan "tepat satu": nol default tetap sah, dan
--    ditangani urutan di users.service.ts#findOne.
CREATE UNIQUE INDEX IF NOT EXISTS "userCompany_one_default_per_user"
  ON "userCompany" ("userId")
  WHERE "isDefault";
