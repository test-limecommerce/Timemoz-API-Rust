-- Visit Master — Fase F1 (docs/visit_masters.md §3).
--
-- Dua tabel master baru ("visitType", "visitFrequency"), dua kolom baru
-- ("storeVisitPlan"."visitTypeId", "storeVisit"."visitTypeId"), dan dua nilai
-- baru di "AuditEntityType" (pola orderPaymentMethod).
--
-- Bagian 6 di bawah (partial unique index + CHECK) TIDAK bisa dinyatakan di
-- `schema.prisma`. `prisma db push` — jalur yang dipakai di development — tidak
-- pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya. `npm run db:sync` menjalankan keduanya berurutan.
--
-- Keunikan "(companyId, code)" kedua tabel SENGAJA @@unique biasa di
-- schema.prisma (§0.3, §0.4 — pola customerType), BUKAN partial index manual:
-- tidak ada `deletedAt` di modul ini, diarsip lewat "isActive".

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Nilai baru di "AuditEntityType" (§8.4 — pola orderPaymentMethod)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'visitType';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'visitFrequency';

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. "visitType" (§3.1) — diarsip lewat "isActive", satu default per company
--    (partial index manual, bagian 6).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "visitType" (
    "id"            SERIAL       NOT NULL,
    "uuid"          UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"     INTEGER      NOT NULL,
    "code"          VARCHAR(32)  NOT NULL,
    "name"          VARCHAR(120) NOT NULL,
    "nameEn"        VARCHAR(120),
    "description"   VARCHAR(255),
    "requiresOrder" BOOLEAN      NOT NULL DEFAULT false,
    "requiresPhoto" BOOLEAN      NOT NULL DEFAULT false,
    "requiresGps"   BOOLEAN      NOT NULL DEFAULT true,
    "sortOrder"     INTEGER      NOT NULL DEFAULT 0,
    "isDefault"     BOOLEAN      NOT NULL DEFAULT false,
    "isActive"      BOOLEAN      NOT NULL DEFAULT true,
    "createdById"   INTEGER      NOT NULL,
    "createdAt"     TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"     TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "visitType_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "visitType_uuid_key" ON "visitType"("uuid");
CREATE UNIQUE INDEX "visitType_companyId_code_key" ON "visitType"("companyId", "code");
CREATE INDEX "visitType_companyId_isActive_idx" ON "visitType"("companyId", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. "visitFrequency" (§3.2) — belum ada konsumen di v1 (§0.10). "intervalDays"
--    dijaga CHECK manual (bagian 6): 1..366.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "visitFrequency" (
    "id"           SERIAL       NOT NULL,
    "uuid"         UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"    INTEGER      NOT NULL,
    "code"         VARCHAR(32)  NOT NULL,
    "name"         VARCHAR(120) NOT NULL,
    "nameEn"       VARCHAR(120),
    "description"  VARCHAR(255),
    "intervalDays" INTEGER      NOT NULL,
    "sortOrder"    INTEGER      NOT NULL DEFAULT 0,
    "isActive"     BOOLEAN      NOT NULL DEFAULT true,
    "createdById"  INTEGER      NOT NULL,
    "createdAt"    TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"    TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "visitFrequency_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "visitFrequency_uuid_key" ON "visitFrequency"("uuid");
CREATE UNIQUE INDEX "visitFrequency_companyId_code_key" ON "visitFrequency"("companyId", "code");
CREATE INDEX "visitFrequency_companyId_isActive_idx" ON "visitFrequency"("companyId", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Kolom baru pada tabel yang sudah jalan (§3.3). Nullable tanpa DEFAULT =
--    perubahan metadata saja di PG >= 11, tidak menulis ulang tabel "storeVisit".
--
--    "storeVisit" SENGAJA tidak diberi index "visitTypeId" di v1 (§3.3) — belum
--    ada query yang menyaring kunjungan per tipe, dan `CREATE INDEX`
--    non-concurrent di tabel terbesar modul ini mengunci tulis selama migrasi.
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "storeVisitPlan" ADD COLUMN "visitTypeId" INTEGER;
ALTER TABLE "storeVisit"     ADD COLUMN "visitTypeId" INTEGER;

CREATE INDEX "storeVisitPlan_companyId_visitTypeId_idx"
  ON "storeVisitPlan"("companyId", "visitTypeId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Foreign key
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "visitType"
  ADD CONSTRAINT "visitType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "visitType_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "visitFrequency"
  ADD CONSTRAINT "visitFrequency_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "visitFrequency_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- Restrict, bukan Cascade/SetNull: API tidak pernah menghapus baris master
-- (§0.3), dan ini hanya jaring pengaman terhadap DELETE manual di DB (§3.3).
ALTER TABLE "storeVisitPlan"
  ADD CONSTRAINT "storeVisitPlan_visitTypeId_fkey" FOREIGN KEY ("visitTypeId") REFERENCES "visitType"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "storeVisit"
  ADD CONSTRAINT "storeVisit_visitTypeId_fkey" FOREIGN KEY ("visitTypeId") REFERENCES "visitType"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. Manual (§3.5) — SAMA PERSIS dengan entri di prisma/apply-partial-indexes.js.
--    Kalau salah satu diubah, ubah keduanya.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "visitType_one_default_per_company"
  ON "visitType" ("companyId")
  WHERE "isDefault";

ALTER TABLE "visitFrequency"
  ADD CONSTRAINT "visitFrequency_interval_positive_chk" CHECK ("intervalDays" BETWEEN 1 AND 366);
