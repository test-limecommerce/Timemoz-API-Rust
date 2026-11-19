-- `accessLevel_company_override_key` hanya berlaku untuk override AKTIF.
--
-- Index lama (20260903130000_add_access_level_override) tidak menyaring `status`,
-- padahal `RolesService.revertBuiltInOverride` hanya soft delete (`status = 0`).
-- Akibatnya, sesudah satu kali revert, `PUT /v1/admin/roles/:uuid/customize` atas
-- role bawaan yang sama di company yang sama selalu bertabrakan dengan baris yang
-- sudah dihapus → 409 ROLE_SAVE_CONFLICT, dan company itu tidak pernah bisa
-- meng-customize role tersebut lagi.
--
-- Aturan baru: paling banyak satu override AKTIF per role bawaan per company — sama
-- dengan yang dicari `customizeBuiltIn` (`findFirst ... status: ACTIVE`). Predikat
-- baru lebih longgar dari yang lama, jadi data yang lolos index lama pasti lolos
-- index ini. Definisi WAJIB identik dengan entri di prisma/apply-partial-indexes.js.

DROP INDEX IF EXISTS "accessLevel_company_override_key";

CREATE UNIQUE INDEX "accessLevel_company_override_key"
  ON "accessLevel" ("companyId", "overridesRoleId")
  WHERE "overridesRoleId" IS NOT NULL AND "status" = 1;
