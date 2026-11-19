-- Override company-scoped untuk role bawaan (docs/custom_roles.md §F16).
--
-- Aditif: satu kolom nullable, satu FK nullable, satu partial unique index. Tidak
-- ada kolom dihapus, tidak ada tipe berubah, tidak ada `NOT NULL` baru di kolom
-- yang sudah terisi. Aman dijalankan pada database yang sedang melayani trafik.
--
-- Baris "override" adalah baris `accessLevel` CUSTOM biasa (companyId terisi,
-- isSystem = false, baseRole terisi) — jadi ia SUDAH memenuhi `accessLevel_shape_chk`
-- (prisma/migrations/20260826000000_add_custom_roles/migration.sql) tanpa perlu
-- diubah. `overridesRoleId` cuma menambah satu penanda: baris mana yang digantikannya.
--
-- Partial unique index-nya TIDAK bisa dinyatakan di schema.prisma (WHERE-nya).
-- Kembarannya yang idempoten ada di prisma/apply-partial-indexes.js — kalau salah
-- satu diubah, ubah keduanya.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Kolom baru — nullable, terisi hanya pada baris override
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "accessLevel"
  ADD COLUMN "overridesRoleId" INTEGER;

-- Self-relation. Tanpa `ON DELETE CASCADE`: role bawaan tidak pernah benar-benar
-- di-DELETE (trigger `accessLevel_no_delete_system`), jadi cascade di sini tidak
-- pernah punya kesempatan bekerja; override sendiri hanya soft delete
-- (`status = 0`, RolesService.revertBuiltInOverride).
ALTER TABLE "accessLevel"
  ADD CONSTRAINT "accessLevel_overridesRoleId_fkey"
  FOREIGN KEY ("overridesRoleId") REFERENCES "accessLevel"("id")
  ON DELETE NO ACTION ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Partial unique index — paling banyak satu override per role bawaan per company
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "accessLevel_company_override_key"
  ON "accessLevel" ("companyId", "overridesRoleId")
  WHERE "overridesRoleId" IS NOT NULL;
