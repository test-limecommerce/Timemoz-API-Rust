-- Custom Roles & Permission Matrix — Fase F1 (docs/custom_roles.md §3).
--
-- Semuanya aditif: tidak ada kolom dihapus, tidak ada tipe berubah, tidak ada
-- `NOT NULL` baru di kolom yang sudah terisi. Aman dijalankan pada database yang
-- sedang melayani trafik.
--
-- URUTAN DI BAWAH BUKAN SELERA (§3.6). `accessLevel_shape_chk` menuntut tiap baris
-- punya bentuk yang benar; baris yang sudah ada belum memenuhinya (`isSystem` masih
-- `false` dari default kolom baru, sementara `companyId` dan `baseRole` NULL —
-- persis kombinasi yang dilarang). Jadi: kolom → DML yang membereskan baris lama →
-- baru constraint-nya.
--
-- PRA-TERBANG — jalankan DUA kueri ini di environment target SEBELUM migration ini:
--
--   1. §3.6 — baris `accessLevel` bercode di luar enam bawaan:
--        SELECT id, code, name, status FROM "accessLevel"
--        WHERE code NOT IN ('SYSTEM_ADMIN','SUPER_ADMIN','ADMIN','MANAGER','STAFF','GUEST');
--      Baris yang ketemu di-GRANDFATHER (`isSystem = true`) oleh langkah 6 di bawah,
--      bukan dihapus — nol perubahan perilaku. Konsekuensinya ia jadi immutable;
--      kalau ternyata sampah, HAPUS SEBELUM migration ini dijalankan.
--
--   2. §3.4a — baris `userAccessLevel` kembar:
--        SELECT "userId", "companyId", COUNT(*) AS n, array_agg("id" ORDER BY "id")
--        FROM "userAccessLevel" WHERE "status" = 1
--        GROUP BY "userId", "companyId" HAVING COUNT(*) > 1;
--      Kalau ada hasilnya, jalankan `node prisma/dedupe-user-access-level.js --apply`
--      LEBIH DULU. Migration ini sengaja TIDAK men-dedupe sendiri: memilih role mana
--      yang menang adalah keputusan yang harus dilihat manusia. Langkah 8 di bawah
--      berhenti dengan pesan yang menyebutkan skrip itu kalau duplikatnya masih ada.
--
-- DDL manual di bawah (tiga partial index, tiga CHECK, satu trigger) TIDAK bisa
-- dinyatakan di `schema.prisma`. `prisma db push` — jalur yang dipakai di development —
-- tidak pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah, ubah
-- keduanya. `npm run db:sync` menjalankan keduanya berurutan.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum baru
-- ─────────────────────────────────────────────────────────────────────────────

-- Tiga tingkat yang bisa dipilih admin (§0.2, §0.5). SYSTEM_ADMIN / SUPER_ADMIN /
-- GUEST sengaja tidak ditawarkan — mencegah eskalasi lewat pembuatan role.
CREATE TYPE "BaseRole" AS ENUM ('ADMIN', 'MANAGER', 'STAFF');

CREATE TYPE "PermissionAction" AS ENUM ('READ', 'WRITE', 'DELETE');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Kolom baru di "accessLevel" — semuanya nullable / ber-default
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "accessLevel"
  ADD COLUMN "companyId"   INTEGER,
  ADD COLUMN "isSystem"    BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN "baseRole"    "BaseRole",
  ADD COLUMN "description" TEXT,
  ADD COLUMN "createdById" INTEGER;

-- `createdById` sengaja tanpa FK: role bawaan tidak punya pembuat, dan menghapus
-- user tidak boleh menyeret role yang dibuatnya.
ALTER TABLE "accessLevel"
  ADD CONSTRAINT "accessLevel_companyId_fkey"
  FOREIGN KEY ("companyId") REFERENCES "company"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;

CREATE INDEX IF NOT EXISTS "accessLevel_companyId_status_idx"
  ON "accessLevel" ("companyId", "status");

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Tabel matriks
-- ─────────────────────────────────────────────────────────────────────────────

-- Satu baris = satu centang: role X boleh `action` pada `featureKey`.
-- `featureKey` VarChar, bukan enum: daftar fitur (§4.1) hidup di kode, dan menambah
-- fitur baru tidak boleh menuntut migration enum.
CREATE TABLE "rolePermission" (
  "id"            SERIAL             NOT NULL,
  "uuid"          UUID               NOT NULL DEFAULT gen_random_uuid(),
  "accessLevelId" INTEGER            NOT NULL,
  "featureKey"    VARCHAR(64)        NOT NULL,
  "action"        "PermissionAction" NOT NULL,
  "createdAt"     TIMESTAMP(3)       NOT NULL DEFAULT NOW(),

  CONSTRAINT "rolePermission_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "rolePermission_uuid_key" ON "rolePermission" ("uuid");

CREATE UNIQUE INDEX "rolePermission_accessLevelId_featureKey_action_key"
  ON "rolePermission" ("accessLevelId", "featureKey", "action");

CREATE INDEX "rolePermission_accessLevelId_idx"
  ON "rolePermission" ("accessLevelId");

ALTER TABLE "rolePermission"
  ADD CONSTRAINT "rolePermission_accessLevelId_fkey"
  FOREIGN KEY ("accessLevelId") REFERENCES "accessLevel"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. DML — bereskan baris lama LEBIH DULU (§3.6 langkah 2)
-- ─────────────────────────────────────────────────────────────────────────────

-- Dikunci ke `companyId IS NULL`, BUKAN ke daftar enam `code`: supaya baris ketujuh
-- apa pun ikut terangkut otomatis, bukan bergantung pada seseorang mengingat
-- menambahkannya. `POST /access-levels` (@Roles(SYSTEM_ADMIN)) masih hidup dan masih
-- bisa dipakai antara hari ini dan migration ini dijalankan.
UPDATE "accessLevel" SET "isSystem" = true WHERE "companyId" IS NULL;

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Partial unique index untuk `code` (§3.4)
-- ─────────────────────────────────────────────────────────────────────────────

-- `@@unique([companyId, code])` TIDAK cukup: di Postgres NULL <> NULL, jadi dua role
-- global bercode sama tetap lolos. Dua partial index inilah penggantinya.
CREATE UNIQUE INDEX IF NOT EXISTS "accessLevel_global_code_key"
  ON "accessLevel" ("code")
  WHERE "companyId" IS NULL AND "status" = 1;

CREATE UNIQUE INDEX IF NOT EXISTS "accessLevel_company_code_key"
  ON "accessLevel" ("companyId", "code")
  WHERE "companyId" IS NOT NULL AND "status" = 1;

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. Satu role aktif per user per company (§3.4a)
-- ─────────────────────────────────────────────────────────────────────────────

-- Berhenti dengan pesan yang bisa dibaca kalau datanya masih melanggar. Tanpa blok
-- ini, `CREATE UNIQUE INDEX` di bawah gagal dengan "could not create unique index"
-- plus satu contoh baris — benar, tapi tidak menyebutkan apa yang harus dilakukan.
DO $$
DECLARE
  "offenders" INTEGER;
BEGIN
  SELECT COUNT(*) INTO "offenders" FROM (
    SELECT 1 FROM "userAccessLevel" WHERE "status" = 1
    GROUP BY "userId", "companyId" HAVING COUNT(*) > 1
  ) AS "dupes";

  IF "offenders" > 0 THEN
    RAISE EXCEPTION
      'Ada % pasangan (userId, companyId) dengan lebih dari satu role aktif. Jalankan `node prisma/dedupe-user-access-level.js` untuk melihat daftarnya, lalu `--apply` untuk membereskannya, baru ulangi migration ini.',
      "offenders";
  END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS "userAccessLevel_one_active_per_company"
  ON "userAccessLevel" ("userId", "companyId")
  WHERE "status" = 1;

-- ─────────────────────────────────────────────────────────────────────────────
-- 7. CHECK constraint (§3.5) — dipasang SESUDAH langkah 4
-- ─────────────────────────────────────────────────────────────────────────────

-- Bentuk baris. Role bawaan selalu global dan tanpa base role; role custom selalu
-- milik satu company dan selalu punya base role. Tidak ada baris setengah jadi yang
-- bisa masuk dari jalur mana pun — termasuk jalur yang tidak lewat aplikasi.
ALTER TABLE "accessLevel" ADD CONSTRAINT "accessLevel_shape_chk" CHECK (
     ("isSystem" = true  AND "companyId" IS NULL     AND "baseRole" IS NULL)
  OR ("isSystem" = false AND "companyId" IS NOT NULL AND "baseRole" IS NOT NULL)
);

-- Role bawaan tidak bisa dinonaktifkan. Penghapusan di sistem ini soft delete
-- (`status = 0`), jadi satu CHECK menutup seluruh jalur UPDATE.
ALTER TABLE "accessLevel" ADD CONSTRAINT "accessLevel_system_always_active_chk"
  CHECK ("isSystem" = false OR "status" = 1);

-- Role custom tidak boleh memakai `code` bawaan (§1.10). Kalau bisa, satu baris
-- bercode 'ADMIN' — nama yang wajar dipilih orang, tanpa niat apa pun — akan lolos
-- setiap @Roles(ADMIN) begitu cabang kill switch dimatikan.
ALTER TABLE "accessLevel" ADD CONSTRAINT "accessLevel_reserved_code_chk" CHECK (
  "isSystem" = true
  OR "code" NOT IN ('SYSTEM_ADMIN','SUPER_ADMIN','ADMIN','MANAGER','STAFF','GUEST')
);

-- ─────────────────────────────────────────────────────────────────────────────
-- 8. Trigger — menutup jalur DELETE sungguhan (§3.5)
-- ─────────────────────────────────────────────────────────────────────────────

-- CHECK di atas hanya menutup UPDATE. `DELETE FROM "accessLevel"` tetap lolos, dan
-- `userAccessLevel` ber-ON DELETE CASCADE — menghapus satu baris bawaan menghapus
-- role seluruh user yang memegangnya.
CREATE OR REPLACE FUNCTION guard_system_access_level() RETURNS TRIGGER AS $$
BEGIN
  IF OLD."isSystem" THEN
    RAISE EXCEPTION 'accessLevel bawaan tidak boleh dihapus (code=%)', OLD."code";
  END IF;
  RETURN OLD;
END; $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS "accessLevel_no_delete_system" ON "accessLevel";
CREATE TRIGGER "accessLevel_no_delete_system" BEFORE DELETE ON "accessLevel"
  FOR EACH ROW EXECUTE FUNCTION guard_system_access_level();
