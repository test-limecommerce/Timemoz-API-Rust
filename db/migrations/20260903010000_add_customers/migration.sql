-- Customer (induk toko) — Fase F1 (docs/customers.md §3, §11).
--
-- Lima tabel baru (customerType, customerGroup, customerChannel,
-- customerSubChannel, customer), satu kolom baru di "store" (customerId), dan
-- lima nilai baru di "AuditEntityType" (§0.8: bukan satu `customerMaster`
-- ber-subEntity — lihat catatan di schema.prisma).
--
-- Partial unique index di bagian akhir TIDAK bisa dinyatakan di `schema.prisma`.
-- `prisma db push` — jalur yang dipakai di development — tidak pernah
-- membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang idempoten
-- ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah, ubah
-- keduanya. `npm run db:sync` menjalankan keduanya berurutan.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Nilai baru di "AuditEntityType"
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'customer';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'customerType';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'customerGroup';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'customerChannel';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'customerSubChannel';

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Tabel master (identik bentuknya: type, group, channel)
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "customerType" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "customerType_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "customerType_uuid_key" ON "customerType"("uuid");
CREATE UNIQUE INDEX "customerType_companyId_code_key" ON "customerType"("companyId", "code");
CREATE INDEX "customerType_companyId_isActive_idx" ON "customerType"("companyId", "isActive");

CREATE TABLE "customerGroup" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "customerGroup_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "customerGroup_uuid_key" ON "customerGroup"("uuid");
CREATE UNIQUE INDEX "customerGroup_companyId_code_key" ON "customerGroup"("companyId", "code");
CREATE INDEX "customerGroup_companyId_isActive_idx" ON "customerGroup"("companyId", "isActive");

CREATE TABLE "customerChannel" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "customerChannel_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "customerChannel_uuid_key" ON "customerChannel"("uuid");
CREATE UNIQUE INDEX "customerChannel_companyId_code_key" ON "customerChannel"("companyId", "code");
CREATE INDEX "customerChannel_companyId_isActive_idx" ON "customerChannel"("companyId", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. customerSubChannel — anak customerChannel. `code` unik per company, bukan
--    per channel (docs/customers.md §3.2): kode sama di dua channel beda bikin
--    dropdown dan import by-code ambigu.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "customerSubChannel" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "channelId"   INTEGER      NOT NULL,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "customerSubChannel_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "customerSubChannel_uuid_key" ON "customerSubChannel"("uuid");
CREATE UNIQUE INDEX "customerSubChannel_companyId_code_key" ON "customerSubChannel"("companyId", "code");
CREATE INDEX "customerSubChannel_companyId_isActive_idx" ON "customerSubChannel"("companyId", "isActive");
CREATE INDEX "customerSubChannel_companyId_channelId_isActive_idx" ON "customerSubChannel"("companyId", "channelId", "isActive");

ALTER TABLE "customerSubChannel"
  ADD CONSTRAINT "customerSubChannel_channelId_fkey"
  FOREIGN KEY ("channelId") REFERENCES "customerChannel"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. customer — level account. `deletedAt` (soft delete), beda dari keempat
--    tabel master di atas yang cukup diarsip lewat "isActive" (docs/customers.md §0.5).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "customer" (
    "id"           SERIAL       NOT NULL,
    "uuid"         UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"    INTEGER      NOT NULL,
    "code"         VARCHAR(32)  NOT NULL,
    "name"         VARCHAR(160) NOT NULL,
    "typeId"       INTEGER      NOT NULL,
    "groupId"      INTEGER,
    "channelId"    INTEGER,
    "subChannelId" INTEGER,
    "taxNumber"    VARCHAR(32),
    "phone"        VARCHAR(20),
    "isActive"     BOOLEAN      NOT NULL DEFAULT true,
    "createdById"  INTEGER      NOT NULL,
    "deletedAt"    TIMESTAMP(3),
    "createdAt"    TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"    TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "customer_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "customer_uuid_key" ON "customer"("uuid");
CREATE INDEX "customer_companyId_isActive_idx" ON "customer"("companyId", "isActive");
CREATE INDEX "customer_companyId_deletedAt_idx" ON "customer"("companyId", "deletedAt");
CREATE INDEX "customer_companyId_typeId_idx" ON "customer"("companyId", "typeId");
CREATE INDEX "customer_companyId_groupId_idx" ON "customer"("companyId", "groupId");
CREATE INDEX "customer_companyId_channelId_idx" ON "customer"("companyId", "channelId");

ALTER TABLE "customer"
  ADD CONSTRAINT "customer_typeId_fkey"
  FOREIGN KEY ("typeId") REFERENCES "customerType"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "customer"
  ADD CONSTRAINT "customer_groupId_fkey"
  FOREIGN KEY ("groupId") REFERENCES "customerGroup"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "customer"
  ADD CONSTRAINT "customer_channelId_fkey"
  FOREIGN KEY ("channelId") REFERENCES "customerChannel"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "customer"
  ADD CONSTRAINT "customer_subChannelId_fkey"
  FOREIGN KEY ("subChannelId") REFERENCES "customerSubChannel"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Kolom baru di "store" — opsional, tetap opsional (docs/customers.md §0.10).
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "store" ADD COLUMN "customerId" INTEGER;

CREATE INDEX "store_companyId_customerId_idx" ON "store"("companyId", "customerId");

ALTER TABLE "store"
  ADD CONSTRAINT "store_customerId_fkey"
  FOREIGN KEY ("customerId") REFERENCES "customer"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. FK ke "company" / "users" untuk kelima tabel baru
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "customerType"
  ADD CONSTRAINT "customerType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "customerType_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "customerGroup"
  ADD CONSTRAINT "customerGroup_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "customerGroup_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "customerChannel"
  ADD CONSTRAINT "customerChannel_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "customerChannel_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "customerSubChannel"
  ADD CONSTRAINT "customerSubChannel_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "customerSubChannel_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "customer"
  ADD CONSTRAINT "customer_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "customer_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 7. Partial unique index (§3.5 docs/customers.md) — TIDAK dinyatakan sebagai
--    @@unique biasa: itu akan menutup pintu bagi kode customer yang sudah
--    di-soft-delete untuk dipakai ulang. Kembarannya yang idempoten ada di
--    prisma/apply-partial-indexes.js — kalau salah satu diubah, ubah keduanya.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "customer_code_unique_per_company"
  ON "customer" ("companyId", "code")
  WHERE "deletedAt" IS NULL;
