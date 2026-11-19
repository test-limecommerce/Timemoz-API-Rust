-- Table Field Visibility — Fase F1 (docs/table_field_visibility.md §3).
--
-- Nol ALTER pada tabel existing (§0.3): hanya enum & tabel baru. Aman dijalankan
-- pada database yang sedang melayani trafik.
--
-- DDL manual di bawah (dua partial unique index) TIDAK bisa dinyatakan di
-- `schema.prisma`. `prisma db push` — jalur yang dipakai di development — tidak
-- pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya (§3.5). `npm run db:sync` menjalankan keduanya berurutan.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum baru
-- ─────────────────────────────────────────────────────────────────────────────

-- Empat mode per kolom (§0.5). visible = apa adanya; revealable = disensor tapi
-- bisa dibuka per baris dengan jejak (§7); masked = disensor tanpa jalan buka;
-- hidden = kolomnya tidak dirender sama sekali.
CREATE TYPE "FieldVisibilityMode" AS ENUM ('visible', 'revealable', 'masked', 'hidden');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Tabel konfigurasi
-- ─────────────────────────────────────────────────────────────────────────────

-- Satu baris = satu penyimpangan dari default (§0.13). Tidak ada baris untuk
-- kolom yang `visible`: default hidup di registry di kode, bukan di sini.
CREATE TABLE "tableFieldVisibility" (
    "id"            SERIAL                NOT NULL,
    "uuid"          UUID                  NOT NULL DEFAULT gen_random_uuid(),
    "companyId"     INTEGER               NOT NULL,
    "accessLevelId" INTEGER,
    "tableKey"      VARCHAR(48)           NOT NULL,
    "columnKey"     VARCHAR(48)           NOT NULL,
    "mode"          "FieldVisibilityMode" NOT NULL,
    "updatedById"   INTEGER,
    "createdAt"     TIMESTAMP(3)          NOT NULL DEFAULT NOW(),
    "updatedAt"     TIMESTAMP(3)          DEFAULT NOW(),

    CONSTRAINT "tableFieldVisibility_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "tableFieldVisibility_uuid_key" ON "tableFieldVisibility"("uuid");

CREATE INDEX "tableFieldVisibility_companyId_tableKey_idx"
  ON "tableFieldVisibility"("companyId", "tableKey");

CREATE INDEX "tableFieldVisibility_accessLevelId_idx"
  ON "tableFieldVisibility"("accessLevelId");

ALTER TABLE "tableFieldVisibility"
  ADD CONSTRAINT "tableFieldVisibility_companyId_fkey"
  FOREIGN KEY ("companyId") REFERENCES "company"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "tableFieldVisibility"
  ADD CONSTRAINT "tableFieldVisibility_accessLevelId_fkey"
  FOREIGN KEY ("accessLevelId") REFERENCES "accessLevel"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Dua partial unique index (§3.4) — TIDAK dinyatakan sebagai @@unique biasa:
--    accessLevelId nullable, dan di Postgres NULL <> NULL, jadi dua baris default
--    untuk kolom yang sama akan lolos.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "tableFieldVisibility_company_default_key"
  ON "tableFieldVisibility" ("companyId", "tableKey", "columnKey")
  WHERE "accessLevelId" IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS "tableFieldVisibility_role_key"
  ON "tableFieldVisibility" ("companyId", "accessLevelId", "tableKey", "columnKey")
  WHERE "accessLevelId" IS NOT NULL;

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Tabel jejak reveal
-- ─────────────────────────────────────────────────────────────────────────────

-- Peristiwa baca, bukan perubahan (§0.9) — tabel sendiri, bukan "auditLog": volume
-- jauh lebih besar, dan "AuditAction" tidak punya nilai 'reveal' (§1.5). Tanpa FK
-- ke company/users, mengikuti §0.5 docs/audit_log.md: jejak tidak boleh ikut
-- terhapus bersama pelakunya.
CREATE TABLE "fieldRevealLog" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "tableKey"    VARCHAR(48)  NOT NULL,
    "columnKey"   VARCHAR(48)  NOT NULL,
    "recordId"    INTEGER      NOT NULL,
    "recordUuid"  UUID,
    "recordLabel" VARCHAR(160),
    "actorId"     INTEGER      NOT NULL,
    "actorName"   VARCHAR(160),
    "actorRole"   VARCHAR(32),
    "reason"      VARCHAR(255),
    "requestId"   UUID,
    "ipAddress"   VARCHAR(45),
    "userAgent"   VARCHAR(255),
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "fieldRevealLog_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "fieldRevealLog_uuid_key" ON "fieldRevealLog"("uuid");

CREATE INDEX "fieldRevealLog_companyId_createdAt_idx"
  ON "fieldRevealLog"("companyId", "createdAt");

CREATE INDEX "fieldRevealLog_companyId_actorId_createdAt_idx"
  ON "fieldRevealLog"("companyId", "actorId", "createdAt");

CREATE INDEX "fieldRevealLog_companyId_tableKey_columnKey_createdAt_idx"
  ON "fieldRevealLog"("companyId", "tableKey", "columnKey", "createdAt");
