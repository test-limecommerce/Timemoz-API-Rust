-- Product Master (sales product) — Fase F1 (docs/product_master.md §3, §13.1, §16).
--
-- Tiga belas tabel baru, empat tipe enum baru, dua kolom baru pada tabel yang
-- sudah jalan ("company"."catalogEpoch", "customer"."priceListId"), dan sepuluh
-- nilai baru di "AuditEntityType" (§13.1: `productUom` dan `productBarcode`
-- sengaja TIDAK dapat nilai sendiri — keduanya bagian dari produknya).
--
-- Bagian 10–12 di bawah (partial index, GIN, CHECK, trigger) TIDAK bisa dinyatakan
-- di `schema.prisma`. `prisma db push` — jalur yang dipakai di development —
-- tidak pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya
-- yang idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu
-- diubah, ubah keduanya. `npm run db:sync` menjalankan keduanya berurutan.
--
-- Yang paling mahal kalau hilang adalah TRIGGER, bukan index: tanpa
-- `touch_updated_at`, kolom `updatedAt` berhenti naik dan delta sync katalog
-- (§10) DIAM-DIAM melewatkan baris. Gejalanya harga salah di HP sales, bukan
-- error di log (§0.16, §17.1).

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Tipe enum baru
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TYPE "UomType" AS ENUM ('unit', 'weight', 'volume', 'length');

-- §0.7 — satu nilai di v1; `customer`/`store` menyusul sebagai penambahan nilai,
-- bukan perubahan arti kunci yang sudah tersimpan.
CREATE TYPE "CustomFieldEntity" AS ENUM ('product');

CREATE TYPE "CustomFieldType" AS ENUM ('text', 'number', 'date', 'boolean', 'select', 'multiselect');

CREATE TYPE "ProductImportStatus" AS ENUM ('queued', 'running', 'completed', 'failed');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Nilai baru di "AuditEntityType" (§0.29 — semua tabel katalog masuk audit)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'product';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'productCategory';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'productSubCategory';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'productBrand';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'productPrincipal';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'uom';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'taxRate';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'priceList';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'priceListItem';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'productCustomField';

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Master data — enam tabel yang diarsip lewat "isActive", TANPA "deletedAt"
--    (§0.15: volumenya puluhan baris, dikirim penuh setiap sync, jadi baris
--    terarsip tetap terkirim sebagai baris dan tidak ada yang perlu di-tombstone)
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "productCategory" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "sortOrder"   INTEGER      NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "productCategory_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productCategory_uuid_key" ON "productCategory"("uuid");
CREATE UNIQUE INDEX "productCategory_companyId_code_key" ON "productCategory"("companyId", "code");
CREATE INDEX "productCategory_companyId_isActive_idx" ON "productCategory"("companyId", "isActive");
CREATE INDEX "productCategory_companyId_updatedAt_id_idx" ON "productCategory"("companyId", "updatedAt", "id");

-- `code` unik per COMPANY, bukan per kategori (§3.1) — kode sama di dua kategori
-- membuat dropdown dan import by-code ambigu, dan import by-code adalah jalur
-- utama modul ini.
CREATE TABLE "productSubCategory" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "categoryId"  INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "sortOrder"   INTEGER      NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "productSubCategory_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productSubCategory_uuid_key" ON "productSubCategory"("uuid");
CREATE UNIQUE INDEX "productSubCategory_companyId_code_key" ON "productSubCategory"("companyId", "code");
CREATE INDEX "productSubCategory_companyId_isActive_idx" ON "productSubCategory"("companyId", "isActive");
CREATE INDEX "productSubCategory_companyId_categoryId_isActive_idx" ON "productSubCategory"("companyId", "categoryId", "isActive");
CREATE INDEX "productSubCategory_companyId_updatedAt_id_idx" ON "productSubCategory"("companyId", "updatedAt", "id");

CREATE TABLE "productBrand" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "description" VARCHAR(255),
    "sortOrder"   INTEGER      NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "productBrand_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productBrand_uuid_key" ON "productBrand"("uuid");
CREATE UNIQUE INDEX "productBrand_companyId_code_key" ON "productBrand"("companyId", "code");
CREATE INDEX "productBrand_companyId_isActive_idx" ON "productBrand"("companyId", "isActive");
CREATE INDEX "productBrand_companyId_updatedAt_id_idx" ON "productBrand"("companyId", "updatedAt", "id");

-- "contactPerson"/"phone"/"email"/"address" adalah data orang: TIDAK ikut ke
-- mobile (§10.3) dan tidak masuk table-field.registry.ts di v1 (§12).
CREATE TABLE "productPrincipal" (
    "id"            SERIAL       NOT NULL,
    "uuid"          UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"     INTEGER      NOT NULL,
    "code"          VARCHAR(32)  NOT NULL,
    "name"          VARCHAR(200) NOT NULL,
    "contactPerson" VARCHAR(150),
    "phone"         VARCHAR(50),
    "email"         VARCHAR(150),
    "address"       TEXT,
    "isActive"      BOOLEAN      NOT NULL DEFAULT true,
    "createdById"   INTEGER      NOT NULL,
    "createdAt"     TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"     TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "productPrincipal_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productPrincipal_uuid_key" ON "productPrincipal"("uuid");
CREATE UNIQUE INDEX "productPrincipal_companyId_code_key" ON "productPrincipal"("companyId", "code");
CREATE INDEX "productPrincipal_companyId_isActive_idx" ON "productPrincipal"("companyId", "isActive");
CREATE INDEX "productPrincipal_companyId_updatedAt_id_idx" ON "productPrincipal"("companyId", "updatedAt", "id");

CREATE TABLE "uom" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "uomType"     "UomType"    NOT NULL DEFAULT 'unit',
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "uom_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "uom_uuid_key" ON "uom"("uuid");
CREATE UNIQUE INDEX "uom_companyId_code_key" ON "uom"("companyId", "code");
CREATE INDEX "uom_companyId_isActive_idx" ON "uom"("companyId", "isActive");
CREATE INDEX "uom_companyId_updatedAt_id_idx" ON "uom"("companyId", "updatedAt", "id");

-- "rate" adalah PERSEN (11.0000 = 11%), bukan pecahan; "isInclusive" = true
-- berarti harga di price list SUDAH mengandung pajak (§3.4). Perhitungannya
-- tidak dikerjakan di sini — yang dikunci hanya artinya.
CREATE TABLE "taxRate" (
    "id"          SERIAL        NOT NULL,
    "uuid"        UUID          NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER       NOT NULL,
    "code"        VARCHAR(32)   NOT NULL,
    "name"        VARCHAR(120)  NOT NULL,
    "rate"        DECIMAL(8, 4) NOT NULL,
    "isInclusive" BOOLEAN       NOT NULL DEFAULT false,
    "isActive"    BOOLEAN       NOT NULL DEFAULT true,
    "createdById" INTEGER       NOT NULL,
    "createdAt"   TIMESTAMP(3)  NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)  DEFAULT NOW(),

    CONSTRAINT "taxRate_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "taxRate_uuid_key" ON "taxRate"("uuid");
CREATE UNIQUE INDEX "taxRate_companyId_code_key" ON "taxRate"("companyId", "code");
CREATE INDEX "taxRate_companyId_isActive_idx" ON "taxRate"("companyId", "isActive");
CREATE INDEX "taxRate_companyId_updatedAt_id_idx" ON "taxRate"("companyId", "updatedAt", "id");

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Definisi custom field — BARIS TABEL, bukan satu dokumen JSON schema per
--    company (§0.6). "key" dan "type" IMMUTABLE setelah dibuat (§0.8); menghapus
--    definisi = mengarsipkan, nilainya di "product"."customFields" TIDAK PERNAH
--    dihapus otomatis (§0.9).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "productCustomField" (
    "id"          SERIAL              NOT NULL,
    "uuid"        UUID                NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER             NOT NULL,
    "entity"      "CustomFieldEntity" NOT NULL DEFAULT 'product',
    "key"         VARCHAR(40)         NOT NULL,
    "label"       VARCHAR(120)        NOT NULL,
    "labelEn"     VARCHAR(120),
    "type"        "CustomFieldType"   NOT NULL,
    "options"     JSONB,
    "isRequired"  BOOLEAN             NOT NULL DEFAULT false,
    "isSensitive" BOOLEAN             NOT NULL DEFAULT false,
    "sortOrder"   INTEGER             NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN             NOT NULL DEFAULT true,
    "createdById" INTEGER             NOT NULL,
    "createdAt"   TIMESTAMP(3)        NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)        DEFAULT NOW(),

    CONSTRAINT "productCustomField_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productCustomField_uuid_key" ON "productCustomField"("uuid");
CREATE UNIQUE INDEX "productCustomField_companyId_entity_key_key" ON "productCustomField"("companyId", "entity", "key");
CREATE INDEX "productCustomField_companyId_entity_isActive_idx" ON "productCustomField"("companyId", "entity", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. product & anak-anaknya
--
--    TIDAK ada kolom "barcode" di "product" maupun "productUom" (§0.25/§3.9):
--    barcode di dua tempat membuat keunikan mustahil ditegakkan database.
--    "productUom"/"productBarcode" tanpa "deletedAt" (§0.15) — keduanya dikirim
--    BERSARANG di baris produk, jadi "product"."updatedAt" yang jadi tombstone-nya
--    (trigger "bump_product_updated_at", bagian 9).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "product" (
    "id"             SERIAL         NOT NULL,
    "uuid"           UUID           NOT NULL DEFAULT gen_random_uuid(),
    "companyId"      INTEGER        NOT NULL,
    "sku"            VARCHAR(100)   NOT NULL,
    "name"           VARCHAR(200)   NOT NULL,
    "description"    TEXT,
    "categoryId"     INTEGER        NOT NULL,
    "subCategoryId"  INTEGER,
    "brandId"        INTEGER,
    "principalId"    INTEGER,
    "baseUomId"      INTEGER        NOT NULL,
    "taxRateId"      INTEGER,
    "weight"         DECIMAL(18, 4),
    "volume"         DECIMAL(18, 4),
    "imageUrl"       TEXT,
    "imageObjectKey" TEXT,
    "imageUpdatedAt" TIMESTAMP(3),
    "isOrderable"    BOOLEAN        NOT NULL DEFAULT true,
    "isActive"       BOOLEAN        NOT NULL DEFAULT true,
    "customFields"   JSONB          NOT NULL DEFAULT '{}',
    "createdById"    INTEGER        NOT NULL,
    "deletedAt"      TIMESTAMP(3),
    "createdAt"      TIMESTAMP(3)   NOT NULL DEFAULT NOW(),
    "updatedAt"      TIMESTAMP(3)   DEFAULT NOW(),

    CONSTRAINT "product_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "product_uuid_key" ON "product"("uuid");
CREATE INDEX "product_companyId_isActive_deletedAt_idx" ON "product"("companyId", "isActive", "deletedAt");
CREATE INDEX "product_companyId_categoryId_idx" ON "product"("companyId", "categoryId");
CREATE INDEX "product_companyId_brandId_idx" ON "product"("companyId", "brandId");
CREATE INDEX "product_companyId_principalId_idx" ON "product"("companyId", "principalId");
CREATE INDEX "product_companyId_updatedAt_id_idx" ON "product"("companyId", "updatedAt", "id");

CREATE TABLE "productUom" (
    "id"             SERIAL          NOT NULL,
    "uuid"           UUID            NOT NULL DEFAULT gen_random_uuid(),
    "productId"      INTEGER         NOT NULL,
    "uomId"          INTEGER         NOT NULL,
    "conversionRate" DECIMAL(18, 6)  NOT NULL,
    "isBase"         BOOLEAN         NOT NULL DEFAULT false,
    "isSalesUom"     BOOLEAN         NOT NULL DEFAULT true,
    "isPurchaseUom"  BOOLEAN         NOT NULL DEFAULT false,
    "createdAt"      TIMESTAMP(3)    NOT NULL DEFAULT NOW(),
    "updatedAt"      TIMESTAMP(3)    DEFAULT NOW(),

    CONSTRAINT "productUom_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productUom_uuid_key" ON "productUom"("uuid");
CREATE UNIQUE INDEX "productUom_productId_uomId_key" ON "productUom"("productId", "uomId");
CREATE INDEX "productUom_productId_idx" ON "productUom"("productId");

CREATE TABLE "productBarcode" (
    "id"        SERIAL       NOT NULL,
    "uuid"      UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER      NOT NULL,
    "productId" INTEGER      NOT NULL,
    "uomId"     INTEGER,
    "barcode"   VARCHAR(100) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "productBarcode_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productBarcode_uuid_key" ON "productBarcode"("uuid");
CREATE UNIQUE INDEX "productBarcode_companyId_barcode_key" ON "productBarcode"("companyId", "barcode");
CREATE INDEX "productBarcode_productId_idx" ON "productBarcode"("productId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. Price list. "priceListItem"."companyId" DENORMALISASI — dipakai delta sync
--    per company tanpa join ke "priceList" (§3.8). Keduanya punya "deletedAt"
--    (§0.15): barisnya benar-benar bisa hilang dari pandangan device.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "priceList" (
    "id"           SERIAL       NOT NULL,
    "uuid"         UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"    INTEGER      NOT NULL,
    "code"         VARCHAR(32)  NOT NULL,
    "name"         VARCHAR(150) NOT NULL,
    "currencyCode" VARCHAR(10)  NOT NULL,
    "startDate"    DATE,
    "endDate"      DATE,
    "isDefault"    BOOLEAN      NOT NULL DEFAULT false,
    "isActive"     BOOLEAN      NOT NULL DEFAULT true,
    "createdById"  INTEGER      NOT NULL,
    "deletedAt"    TIMESTAMP(3),
    "createdAt"    TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"    TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "priceList_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "priceList_uuid_key" ON "priceList"("uuid");
CREATE INDEX "priceList_companyId_isActive_deletedAt_idx" ON "priceList"("companyId", "isActive", "deletedAt");
CREATE INDEX "priceList_companyId_updatedAt_id_idx" ON "priceList"("companyId", "updatedAt", "id");

CREATE TABLE "priceListItem" (
    "id"          SERIAL         NOT NULL,
    "uuid"        UUID           NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER        NOT NULL,
    "priceListId" INTEGER        NOT NULL,
    "productId"   INTEGER        NOT NULL,
    "uomId"       INTEGER        NOT NULL,
    "minQty"      DECIMAL(18, 4) NOT NULL DEFAULT 1,
    "maxQty"      DECIMAL(18, 4),
    "price"       DECIMAL(18, 2) NOT NULL,
    "deletedAt"   TIMESTAMP(3),
    "createdAt"   TIMESTAMP(3)   NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)   DEFAULT NOW(),

    CONSTRAINT "priceListItem_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "priceListItem_uuid_key" ON "priceListItem"("uuid");
CREATE INDEX "priceListItem_priceListId_productId_idx" ON "priceListItem"("priceListId", "productId");
CREATE INDEX "priceListItem_companyId_updatedAt_id_idx" ON "priceListItem"("companyId", "updatedAt", "id");

-- ─────────────────────────────────────────────────────────────────────────────
-- 7. Job import Excel. Statusnya dibaca dari BARIS INI, bukan dari state Bull —
--    Bull di repo ini menghapus job selesai maupun gagal (§9.2). "objectKey"
--    wajib GCS, bukan disk lokal: job diambil instance mana pun (§18.C3).
--    "createdById" sengaja tanpa FK ke "users" (§3 pembuka).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "productImportJob" (
    "id"          SERIAL                NOT NULL,
    "uuid"        UUID                  NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER               NOT NULL,
    "createdById" INTEGER               NOT NULL,
    "fileName"    VARCHAR(255)          NOT NULL,
    "objectKey"   TEXT                  NOT NULL,
    "dryRun"      BOOLEAN               NOT NULL DEFAULT false,
    "status"      "ProductImportStatus" NOT NULL DEFAULT 'queued',
    "totalRows"   INTEGER               NOT NULL DEFAULT 0,
    "processed"   INTEGER               NOT NULL DEFAULT 0,
    "succeeded"   INTEGER               NOT NULL DEFAULT 0,
    "failed"      INTEGER               NOT NULL DEFAULT 0,
    "errors"      JSONB                 NOT NULL DEFAULT '[]',
    "warnings"    JSONB                 NOT NULL DEFAULT '[]',
    "startedAt"   TIMESTAMP(3),
    "finishedAt"  TIMESTAMP(3),
    "createdAt"   TIMESTAMP(3)          NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)          DEFAULT NOW(),

    CONSTRAINT "productImportJob_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "productImportJob_uuid_key" ON "productImportJob"("uuid");
CREATE INDEX "productImportJob_companyId_createdAt_idx" ON "productImportJob"("companyId", "createdAt");

-- ─────────────────────────────────────────────────────────────────────────────
-- 8. Kolom baru pada tabel yang sudah jalan (§3.10). Keduanya nullable atau
--    berdefault, jadi tidak ada backfill yang dibutuhkan.
-- ─────────────────────────────────────────────────────────────────────────────

-- §0.18 — naik HANYA untuk perubahan definisi custom field. Apa pun yang punya
-- baris ber-"updatedAt" TIDAK BOLEH menaikkannya (§18.A3).
ALTER TABLE "company" ADD COLUMN "catalogEpoch" INTEGER NOT NULL DEFAULT 1;

-- §0.27 — satu customer menunjuk TEPAT SATU price list; NULL = pakai default
-- company.
ALTER TABLE "customer" ADD COLUMN "priceListId" INTEGER;

CREATE INDEX "customer_companyId_priceListId_idx" ON "customer"("companyId", "priceListId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 9. Foreign key
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "productCategory"
  ADD CONSTRAINT "productCategory_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productCategory_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productSubCategory"
  ADD CONSTRAINT "productSubCategory_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productSubCategory_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productSubCategory_categoryId_fkey" FOREIGN KEY ("categoryId") REFERENCES "productCategory"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productBrand"
  ADD CONSTRAINT "productBrand_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productBrand_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productPrincipal"
  ADD CONSTRAINT "productPrincipal_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productPrincipal_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "uom"
  ADD CONSTRAINT "uom_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "uom_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "taxRate"
  ADD CONSTRAINT "taxRate_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "taxRate_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productCustomField"
  ADD CONSTRAINT "productCustomField_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productCustomField_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "product"
  ADD CONSTRAINT "product_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_categoryId_fkey" FOREIGN KEY ("categoryId") REFERENCES "productCategory"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_subCategoryId_fkey" FOREIGN KEY ("subCategoryId") REFERENCES "productSubCategory"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_brandId_fkey" FOREIGN KEY ("brandId") REFERENCES "productBrand"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_principalId_fkey" FOREIGN KEY ("principalId") REFERENCES "productPrincipal"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_baseUomId_fkey" FOREIGN KEY ("baseUomId") REFERENCES "uom"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "product_taxRateId_fkey" FOREIGN KEY ("taxRateId") REFERENCES "taxRate"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ON DELETE CASCADE di sini AMAN dan disengaja (§3.7): baris "productUom" adalah
-- bagian dari produknya, bukan bukti terpisah. Karena tidak ada hard delete
-- produk (§0.14), cascade-nya praktis tidak pernah berjalan dari aplikasi.
ALTER TABLE "productUom"
  ADD CONSTRAINT "productUom_productId_fkey" FOREIGN KEY ("productId") REFERENCES "product"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "productUom_uomId_fkey" FOREIGN KEY ("uomId") REFERENCES "uom"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productBarcode"
  ADD CONSTRAINT "productBarcode_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "productBarcode_productId_fkey" FOREIGN KEY ("productId") REFERENCES "product"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "priceList"
  ADD CONSTRAINT "priceList_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "priceList_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "priceListItem"
  ADD CONSTRAINT "priceListItem_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "priceListItem_priceListId_fkey" FOREIGN KEY ("priceListId") REFERENCES "priceList"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "priceListItem_productId_fkey" FOREIGN KEY ("productId") REFERENCES "product"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "priceListItem_uomId_fkey" FOREIGN KEY ("uomId") REFERENCES "uom"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "productImportJob"
  ADD CONSTRAINT "productImportJob_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "customer"
  ADD CONSTRAINT "customer_priceListId_fkey" FOREIGN KEY ("priceListId") REFERENCES "priceList"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 10. Partial unique index + GIN (§3.11 nomor 15–20)
--
--     Kelima partial index di bawah TIDAK bisa ditulis sebagai `@@unique` di
--     schema.prisma: itu akan menutup pintu bagi baris yang sudah di-soft-delete
--     untuk dipakai ulang (sku, code, tier harga) atau menyalahartikan kolom
--     boolean sebagai bagian dari kunci (isBase, isDefault).
--
--     Kembarannya yang idempoten ada di prisma/apply-partial-indexes.js — kalau
--     salah satu diubah, ubah keduanya.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "product_sku_unique_per_company"
  ON "product" ("companyId", "sku")
  WHERE "deletedAt" IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS "productUom_one_base_per_product"
  ON "productUom" ("productId")
  WHERE "isBase";

CREATE UNIQUE INDEX IF NOT EXISTS "priceList_one_default_per_company"
  ON "priceList" ("companyId")
  WHERE "isDefault" AND "deletedAt" IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS "priceListItem_tier_unique_active"
  ON "priceListItem" ("priceListId", "productId", "uomId", "minQty")
  WHERE "deletedAt" IS NULL;

-- F6 (§16.1 mencatatnya sebagai lubang yang sengaja dibiarkan terbuka untuk
-- keputusan user — ditutup di F6, disetujui user): satu-satunya tabel ber-`code`
-- di modul ini yang sebelumnya tanpa jaminan keunikan sama sekali.
CREATE UNIQUE INDEX IF NOT EXISTS "priceList_code_unique_per_company"
  ON "priceList" ("companyId", "code")
  WHERE "deletedAt" IS NULL;

-- GIN, tidak bisa dinyatakan schema.prisma sama sekali. `jsonb_path_ops` HANYA
-- melayani operator containment `@>` — filter custom field yang ditulis dengan
-- `->>` akan SELALU seq scan dan membuat index ini jadi hiasan yang memperlambat
-- setiap tulis (§18.A1). §5.5 karena itu memakai `@>`.
CREATE INDEX IF NOT EXISTS "product_customFields_gin"
  ON "product" USING GIN ("customFields" jsonb_path_ops);

-- ─────────────────────────────────────────────────────────────────────────────
-- 11. CHECK constraint (§3.11 nomor 5–9)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "productUom"
  ADD CONSTRAINT "productUom_conversion_positive_chk" CHECK ("conversionRate" > 0);

-- Base UOM konversinya selalu 1 — kalau tidak, seluruh konversi produk itu
-- berputar di sekitar titik yang salah tanpa satu pun error.
ALTER TABLE "productUom"
  ADD CONSTRAINT "productUom_base_rate_one_chk" CHECK (NOT "isBase" OR "conversionRate" = 1);

ALTER TABLE "priceListItem"
  ADD CONSTRAINT "priceListItem_qty_range_chk"
  CHECK ("minQty" > 0 AND ("maxQty" IS NULL OR "maxQty" >= "minQty"));

ALTER TABLE "priceListItem"
  ADD CONSTRAINT "priceListItem_price_nonneg_chk" CHECK ("price" >= 0);

-- customFields WAJIB objek, bukan array/skalar (§0.5 flat). Tanpa ini, satu
-- `[]` yang lolos validasi aplikasi membuat setiap pembaca jsonb harus punya
-- cabang kedua selamanya.
ALTER TABLE "product"
  ADD CONSTRAINT "product_customFields_object_chk" CHECK (jsonb_typeof("customFields") = 'object');

-- ─────────────────────────────────────────────────────────────────────────────
-- 12. Trigger — §0.16 & §0.17, dua keluarga.
--
--     touch_updated_at MENIMPA nilai yang dikirim aplikasi. Konsekuensi yang
--     diterima sadar: script restore tidak bisa mempertahankan "updatedAt" asli.
--     Untuk katalog itu justru yang diinginkan.
--
--     "customer" adalah tabel LAMA, dan ia masuk daftar karena §10.5 menjadikan
--     `assignments` (customerId → priceListId) stream delta tersendiri. Ini
--     satu-satunya tabel di luar domain katalog yang disentuh. Yang berubah HANYA
--     kolom "updatedAt" jadi terisi otomatis — tidak ada guard, kolom, atau
--     perilaku endpoint yang berubah. Efek sampingnya: baris audit "customer"
--     yang sebelumnya tidak mencatat "updatedAt" pada jalur yang lupa mengisinya
--     kini selalu mencatatnya (§18.C5 — jalan keluarnya menambah kolom itu ke
--     daftar field yang diabaikan diff audit, BUKAN melepas trigger-nya).
--
--     "productImportJob" sengaja TIDAK dapat trigger: barisnya ditulis job-nya
--     sendiri dan tidak pernah masuk stream delta.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION touch_updated_at() RETURNS TRIGGER AS $$
BEGIN
  NEW."updatedAt" = NOW();
  RETURN NEW;
END; $$ LANGUAGE plpgsql;

CREATE TRIGGER "productCategory_touch_updated_at"    BEFORE UPDATE ON "productCategory"    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productSubCategory_touch_updated_at" BEFORE UPDATE ON "productSubCategory" FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productBrand_touch_updated_at"       BEFORE UPDATE ON "productBrand"       FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productPrincipal_touch_updated_at"   BEFORE UPDATE ON "productPrincipal"   FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "uom_touch_updated_at"                BEFORE UPDATE ON "uom"                FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "taxRate_touch_updated_at"            BEFORE UPDATE ON "taxRate"            FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productCustomField_touch_updated_at" BEFORE UPDATE ON "productCustomField" FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "product_touch_updated_at"            BEFORE UPDATE ON "product"            FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productUom_touch_updated_at"         BEFORE UPDATE ON "productUom"         FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "productBarcode_touch_updated_at"     BEFORE UPDATE ON "productBarcode"     FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "priceList_touch_updated_at"          BEFORE UPDATE ON "priceList"          FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "priceListItem_touch_updated_at"      BEFORE UPDATE ON "priceListItem"      FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "customer_touch_updated_at"           BEFORE UPDATE ON "customer"           FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- §0.17 — STATEMENT-level dengan transition table: satu UPDATE per statement,
-- bukan per baris. Penting untuk import yang menulis ribuan baris anak dalam
-- beberapa createMany. TIDAK perlu penjaga rekursi: ia menulis ke tabel LAIN,
-- dan "product_touch_updated_at" hanyalah BEFORE UPDATE biasa.
CREATE OR REPLACE FUNCTION bump_product_updated_at() RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    UPDATE "product" SET "updatedAt" = NOW()
     WHERE "id" IN (SELECT DISTINCT "productId" FROM old_rows);
  ELSE
    UPDATE "product" SET "updatedAt" = NOW()
     WHERE "id" IN (SELECT DISTINCT "productId" FROM new_rows);
  END IF;
  RETURN NULL;
END; $$ LANGUAGE plpgsql;

CREATE TRIGGER "productUom_bump_product_ins"
  AFTER INSERT ON "productUom" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();
CREATE TRIGGER "productUom_bump_product_upd"
  AFTER UPDATE ON "productUom" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();
CREATE TRIGGER "productUom_bump_product_del"
  AFTER DELETE ON "productUom" REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();

CREATE TRIGGER "productBarcode_bump_product_ins"
  AFTER INSERT ON "productBarcode" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();
CREATE TRIGGER "productBarcode_bump_product_upd"
  AFTER UPDATE ON "productBarcode" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();
CREATE TRIGGER "productBarcode_bump_product_del"
  AFTER DELETE ON "productBarcode" REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_product_updated_at();
