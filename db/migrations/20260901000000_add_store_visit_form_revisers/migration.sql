-- Reviser Submission Form — Fase F1 (docs/store_visit_form_revisers.md §3).
--
-- Nol perubahan perilaku (§0.4, Risiko §12 nomor 1): kolom baru di "company"
-- berdefault "allPrivileged", persis gerbang yang berlaku hari ini. Aman
-- dijalankan pada database yang sedang melayani trafik.
--
-- DDL manual di bawah (partial unique index) TIDAK bisa dinyatakan di
-- `schema.prisma`. `prisma db push` — jalur yang dipakai di development — tidak
-- pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya (§3.4). `npm run db:sync` menjalankan keduanya berurutan.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum baru
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TYPE "StoreVisitFormReviseMode" AS ENUM ('allPrivileged', 'selected');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Kolom baru di "company"
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "company"
  ADD COLUMN "storeVisitFormReviseMode" "StoreVisitFormReviseMode" NOT NULL DEFAULT 'allPrivileged';

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Tabel daftar reviser
-- ─────────────────────────────────────────────────────────────────────────────

-- Tanpa FK ke company/users (§1.2 docs/store_visit_form_builder.md): kolom
-- rujukan lintas modul di sini adalah Int polos, mengikuti seluruh tabel
-- storeVisitForm* lain.
CREATE TABLE "storeVisitFormReviser" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "userId"      INTEGER      NOT NULL,
    "createdById" INTEGER      NOT NULL,
    "deletedAt"   TIMESTAMP(3),
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitFormReviser_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "storeVisitFormReviser_uuid_key" ON "storeVisitFormReviser"("uuid");

CREATE INDEX "storeVisitFormReviser_companyId_deletedAt_idx"
  ON "storeVisitFormReviser"("companyId", "deletedAt");

CREATE INDEX "storeVisitFormReviser_userId_idx" ON "storeVisitFormReviser"("userId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Partial unique index (§3.4) — TIDAK dinyatakan sebagai @@unique biasa:
--    itu akan menutup pintu bagi baris yang sudah dicabut (deletedAt IS NOT NULL)
--    untuk ditambahkan kembali.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "storeVisitFormReviser_unique_active"
  ON "storeVisitFormReviser" ("companyId", "userId")
  WHERE "deletedAt" IS NULL;
