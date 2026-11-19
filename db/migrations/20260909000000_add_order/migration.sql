-- Order — Fase F1 (docs/order_design.md §3, §19.1).
--
-- Sembilan tabel baru, empat tipe enum baru, satu kolom baru pada "company"
-- ("maxOrderDiscountPercent"), dan enam nilai baru di "AuditEntityType"
-- (`orderItem` sengaja TIDAK dapat nilai sendiri — pola productUom/productBarcode,
-- §3.1).
--
-- Bagian 10–12 di bawah (partial index, GIN, CHECK, trigger) TIDAK bisa dinyatakan
-- di `schema.prisma`. `prisma db push` — jalur yang dipakai di development — tidak
-- pernah membuatnya dan MENGHAPUSNYA kalau sebelumnya ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya. `npm run db:sync` menjalankan keduanya berurutan.
--
-- Sebagian besar constraint keunikan modul ini (nomor order, kode master, "satu
-- default per company") SENGAJA dijadikan manual (bagian 10) walau beberapa di
-- antaranya bukan partial index — §3.10 docs/order_design.md menaruh seluruh
-- jaminan keunikan modul order di satu tempat yang sama, bukan dicampur antara
-- schema.prisma dan berkas ini.
--
-- Yang paling mahal kalau hilang adalah TRIGGER, bukan index: tanpa
-- `touch_updated_at`, kolom `updatedAt` berhenti naik dan delta sync (§10.3)
-- DIAM-DIAM melewatkan baris.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Tipe enum baru
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TYPE "OrderStatus" AS ENUM ('draft', 'pending_approval', 'approved', 'rejected', 'cancelled');

CREATE TYPE "OrderSource" AS ENUM ('web', 'mobile');

-- spec-sfa.md §38 — reason generik, dibatasi pada dua yang dipakai v1.
CREATE TYPE "OrderReasonType" AS ENUM ('no_order', 'order_cancel');

-- Diskriminator "orderCustomFieldDefinition" (§0.11, §3.2) — enum TERPISAH dari
-- "CustomFieldEntity" milik produk, karena tabelnya juga terpisah.
CREATE TYPE "OrderCustomFieldEntity" AS ENUM ('order', 'orderItem');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Nilai baru di "AuditEntityType" (§3.1 — enam nilai, "orderItem" sengaja
--    tidak dapat nilai sendiri)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'order';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'orderType';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'orderReason';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'orderPaymentTerm';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'orderPaymentMethod';
ALTER TYPE "AuditEntityType" ADD VALUE IF NOT EXISTS 'orderCustomFieldDefinition';

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Definisi custom field order/orderItem — BARIS TABEL, TERPISAH dari
--    "productCustomField" (§0.11). Bentuk kolomnya sengaja identik.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "orderCustomFieldDefinition" (
    "id"          SERIAL                   NOT NULL,
    "uuid"        UUID                     NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER                  NOT NULL,
    "entity"      "OrderCustomFieldEntity" NOT NULL,
    "key"         VARCHAR(40)              NOT NULL,
    "label"       VARCHAR(120)             NOT NULL,
    "labelEn"     VARCHAR(120),
    "type"        "CustomFieldType"        NOT NULL,
    "options"     JSONB,
    "isRequired"  BOOLEAN                  NOT NULL DEFAULT false,
    "isSensitive" BOOLEAN                  NOT NULL DEFAULT false,
    "sortOrder"   INTEGER                  NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN                  NOT NULL DEFAULT true,
    "createdById" INTEGER                  NOT NULL,
    "createdAt"   TIMESTAMP(3)             NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)             DEFAULT NOW(),

    CONSTRAINT "orderCustomFieldDefinition_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderCustomFieldDefinition_uuid_key" ON "orderCustomFieldDefinition"("uuid");
CREATE UNIQUE INDEX "orderCustomFieldDefinition_companyId_entity_key_key" ON "orderCustomFieldDefinition"("companyId", "entity", "key");
CREATE INDEX "orderCustomFieldDefinition_companyId_entity_isActive_idx" ON "orderCustomFieldDefinition"("companyId", "entity", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Master — empat tabel, diarsip lewat "isActive" (pola storeGroup/uom),
--    keunikan "code" per company dan "satu default per company" SENGAJA manual
--    (§3.10), bukan @@unique di schema.prisma (§0.24).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "orderType" (
    "id"               SERIAL       NOT NULL,
    "uuid"             UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"        INTEGER      NOT NULL,
    "code"             VARCHAR(32)  NOT NULL,
    "name"             VARCHAR(120) NOT NULL,
    "nameEn"           VARCHAR(120),
    "requiresApproval" BOOLEAN      NOT NULL DEFAULT false,
    "allowDiscount"    BOOLEAN      NOT NULL DEFAULT true,
    "allowFreeItem"    BOOLEAN      NOT NULL DEFAULT false,
    "numberPrefix"     VARCHAR(8)   NOT NULL DEFAULT 'ORD',
    "isDefault"        BOOLEAN      NOT NULL DEFAULT false,
    "isActive"         BOOLEAN      NOT NULL DEFAULT true,
    "createdById"      INTEGER      NOT NULL,
    "createdAt"        TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"        TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "orderType_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderType_uuid_key" ON "orderType"("uuid");
CREATE INDEX "orderType_companyId_isActive_idx" ON "orderType"("companyId", "isActive");

CREATE TABLE "orderReason" (
    "id"          SERIAL             NOT NULL,
    "uuid"        UUID               NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER            NOT NULL,
    "reasonType"  "OrderReasonType"  NOT NULL,
    "code"        VARCHAR(32)        NOT NULL,
    "name"        VARCHAR(150)       NOT NULL,
    "nameEn"      VARCHAR(150),
    "sortOrder"   INTEGER            NOT NULL DEFAULT 0,
    "isActive"    BOOLEAN            NOT NULL DEFAULT true,
    "createdById" INTEGER            NOT NULL,
    "createdAt"   TIMESTAMP(3)       NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3)       DEFAULT NOW(),

    CONSTRAINT "orderReason_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderReason_uuid_key" ON "orderReason"("uuid");
CREATE INDEX "orderReason_companyId_reasonType_isActive_idx" ON "orderReason"("companyId", "reasonType", "isActive");

CREATE TABLE "orderPaymentTerm" (
    "id"          SERIAL       NOT NULL,
    "uuid"        UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"   INTEGER      NOT NULL,
    "code"        VARCHAR(32)  NOT NULL,
    "name"        VARCHAR(120) NOT NULL,
    "dueDays"     INTEGER      NOT NULL DEFAULT 0,
    "isDefault"   BOOLEAN      NOT NULL DEFAULT false,
    "isActive"    BOOLEAN      NOT NULL DEFAULT true,
    "createdById" INTEGER      NOT NULL,
    "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"   TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "orderPaymentTerm_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderPaymentTerm_uuid_key" ON "orderPaymentTerm"("uuid");
CREATE INDEX "orderPaymentTerm_companyId_isActive_idx" ON "orderPaymentTerm"("companyId", "isActive");

CREATE TABLE "orderPaymentMethod" (
    "id"                SERIAL       NOT NULL,
    "uuid"              UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"         INTEGER      NOT NULL,
    "code"              VARCHAR(32)  NOT NULL,
    "name"              VARCHAR(120) NOT NULL,
    "requiresReference" BOOLEAN      NOT NULL DEFAULT false,
    "isDefault"         BOOLEAN      NOT NULL DEFAULT false,
    "isActive"          BOOLEAN      NOT NULL DEFAULT true,
    "createdById"       INTEGER      NOT NULL,
    "createdAt"         TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"         TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "orderPaymentMethod_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderPaymentMethod_uuid_key" ON "orderPaymentMethod"("uuid");
CREATE INDEX "orderPaymentMethod_companyId_isActive_idx" ON "orderPaymentMethod"("companyId", "isActive");

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. "order" (header). Jangkar "customerId" WAJIB (§0.9); "storeId"/"storeVisitId"
--    opsional. TIDAK ada "deletedAt" (§0.6) — satu-satunya jalur "menghilangkan"
--    order adalah cancel.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "order" (
    "id"                    SERIAL         NOT NULL,
    "uuid"                  UUID           NOT NULL DEFAULT gen_random_uuid(),
    "companyId"             INTEGER        NOT NULL,
    "orderNumber"           VARCHAR(32)    NOT NULL,
    "clientRequestId"       UUID           NOT NULL,
    "orderTypeId"           INTEGER        NOT NULL,
    "customerId"            INTEGER        NOT NULL,
    "storeId"               INTEGER,
    "storeVisitId"          INTEGER,
    "storeGroupId"          INTEGER,
    "ownerId"               INTEGER        NOT NULL,
    "status"                "OrderStatus"  NOT NULL DEFAULT 'draft',
    "source"                "OrderSource"  NOT NULL DEFAULT 'mobile',
    "orderDate"             DATE           NOT NULL,
    "requestedDeliveryDate" DATE,
    "note"                  TEXT,
    "currencyCode"          VARCHAR(10)    NOT NULL,
    "priceListId"           INTEGER,
    "orderPaymentTermId"    INTEGER,
    "paymentTermDays"       INTEGER,
    "dueDate"               DATE,
    "orderPaymentMethodId"  INTEGER,
    "paymentReference"      VARCHAR(64),
    "subtotal"              DECIMAL(18, 2) NOT NULL DEFAULT 0,
    "discountTotal"         DECIMAL(18, 2) NOT NULL DEFAULT 0,
    "taxTotal"              DECIMAL(18, 2) NOT NULL DEFAULT 0,
    "grandTotal"            DECIMAL(18, 2) NOT NULL DEFAULT 0,
    "itemCount"             INTEGER        NOT NULL DEFAULT 0,
    "submittedAt"           TIMESTAMP(3),
    "approvedAt"            TIMESTAMP(3),
    "approvedById"          INTEGER,
    "rejectedAt"            TIMESTAMP(3),
    "rejectedById"          INTEGER,
    "rejectNote"            VARCHAR(255),
    "cancelledAt"           TIMESTAMP(3),
    "cancelledById"         INTEGER,
    "cancelReasonId"        INTEGER,
    "cancelNote"            VARCHAR(255),
    "customFields"          JSONB          NOT NULL DEFAULT '{}',
    "createdById"           INTEGER        NOT NULL,
    "createdAt"             TIMESTAMP(3)   NOT NULL DEFAULT NOW(),
    "updatedAt"             TIMESTAMP(3)   DEFAULT NOW(),

    CONSTRAINT "order_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "order_uuid_key" ON "order"("uuid");
CREATE INDEX "order_companyId_status_orderDate_idx" ON "order"("companyId", "status", "orderDate");
CREATE INDEX "order_companyId_orderDate_id_idx" ON "order"("companyId", "orderDate", "id");
CREATE INDEX "order_companyId_customerId_orderDate_idx" ON "order"("companyId", "customerId", "orderDate");
CREATE INDEX "order_companyId_storeId_orderDate_idx" ON "order"("companyId", "storeId", "orderDate");
CREATE INDEX "order_companyId_ownerId_orderDate_idx" ON "order"("companyId", "ownerId", "orderDate");
CREATE INDEX "order_companyId_storeGroupId_orderDate_idx" ON "order"("companyId", "storeGroupId", "orderDate");
CREATE INDEX "order_companyId_storeVisitId_idx" ON "order"("companyId", "storeVisitId");
CREATE INDEX "order_companyId_dueDate_idx" ON "order"("companyId", "dueDate");
-- Keyset delta (§10.3).
CREATE INDEX "order_companyId_updatedAt_id_idx" ON "order"("companyId", "updatedAt", "id");

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. "orderItem". SNAPSHOT (§0.5) — sku/nama/kode UOM/conversionRate/harga/pajak
--    tidak pernah dibaca ulang dari master. "companyId" denormalisasi (pola
--    priceListItem).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "orderItem" (
    "id"              SERIAL         NOT NULL,
    "uuid"            UUID           NOT NULL DEFAULT gen_random_uuid(),
    "companyId"       INTEGER        NOT NULL,
    "orderId"         INTEGER        NOT NULL,
    "lineNo"          INTEGER        NOT NULL,
    "productId"       INTEGER        NOT NULL,
    "uomId"           INTEGER        NOT NULL,
    "productSku"      VARCHAR(100)   NOT NULL,
    "productName"     VARCHAR(200)   NOT NULL,
    "uomCode"         VARCHAR(32)    NOT NULL,
    "conversionRate"  DECIMAL(18, 6) NOT NULL,
    "priceListItemId" INTEGER,
    "listUnitPrice"   DECIMAL(18, 2),
    "unitPrice"       DECIMAL(18, 2) NOT NULL,
    "qty"             DECIMAL(18, 4) NOT NULL,
    "qtyInBaseUom"    DECIMAL(18, 4) NOT NULL,
    "discountPercent" DECIMAL(5, 2)  NOT NULL DEFAULT 0,
    "discountAmount"  DECIMAL(18, 2) NOT NULL DEFAULT 0,
    "isFreeGood"      BOOLEAN        NOT NULL DEFAULT false,
    "taxRateId"       INTEGER,
    "taxRate"         DECIMAL(8, 4)  NOT NULL DEFAULT 0,
    "taxIsInclusive"  BOOLEAN        NOT NULL DEFAULT false,
    "grossAmount"     DECIMAL(18, 2) NOT NULL,
    "discountValue"   DECIMAL(18, 2) NOT NULL,
    "netAmount"       DECIMAL(18, 2) NOT NULL,
    "taxAmount"       DECIMAL(18, 2) NOT NULL,
    "lineTotal"       DECIMAL(18, 2) NOT NULL,
    "note"            VARCHAR(255),
    "customFields"    JSONB          NOT NULL DEFAULT '{}',
    "createdAt"       TIMESTAMP(3)   NOT NULL DEFAULT NOW(),
    "updatedAt"       TIMESTAMP(3)   DEFAULT NOW(),

    CONSTRAINT "orderItem_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderItem_uuid_key" ON "orderItem"("uuid");
CREATE UNIQUE INDEX "orderItem_orderId_lineNo_key" ON "orderItem"("orderId", "lineNo");
CREATE INDEX "orderItem_orderId_idx" ON "orderItem"("orderId");
CREATE INDEX "orderItem_companyId_productId_idx" ON "orderItem"("companyId", "productId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 7. "storeVisitNoOrder" (§0.17) — "no order" BUKAN sebuah order, tabel sendiri.
--    Satu-satunya hard delete di modul ini (kunjungan yang ditandai no-order lalu
--    ternyata berorder cukup menghapus barisnya).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "storeVisitNoOrder" (
    "id"            SERIAL       NOT NULL,
    "uuid"          UUID         NOT NULL DEFAULT gen_random_uuid(),
    "companyId"     INTEGER      NOT NULL,
    "storeVisitId"  INTEGER      NOT NULL,
    "storeId"       INTEGER      NOT NULL,
    "userId"        INTEGER      NOT NULL,
    "orderReasonId" INTEGER      NOT NULL,
    "note"          VARCHAR(255),
    "createdAt"     TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt"     TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeVisitNoOrder_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "storeVisitNoOrder_uuid_key" ON "storeVisitNoOrder"("uuid");
CREATE UNIQUE INDEX "storeVisitNoOrder_storeVisitId_key" ON "storeVisitNoOrder"("storeVisitId");
CREATE INDEX "storeVisitNoOrder_companyId_createdAt_idx" ON "storeVisitNoOrder"("companyId", "createdAt");
CREATE INDEX "storeVisitNoOrder_companyId_orderReasonId_idx" ON "storeVisitNoOrder"("companyId", "orderReasonId");

-- ─────────────────────────────────────────────────────────────────────────────
-- 8. "orderCounter" (§7) — penomoran atomik per company + periode + prefix, pola
--    "storeVisitCounter" (src/store-visits/store-visits.service.ts:691).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "orderCounter" (
    "companyId"  INTEGER     NOT NULL,
    "period"     VARCHAR(6)  NOT NULL,
    "prefix"     VARCHAR(8)  NOT NULL,
    "lastNumber" INTEGER     NOT NULL DEFAULT 0,

    CONSTRAINT "orderCounter_pkey" PRIMARY KEY ("companyId", "period", "prefix")
);

-- ─────────────────────────────────────────────────────────────────────────────
-- 9. Kolom baru pada tabel yang sudah jalan (§5.4). Nullable, jadi tidak ada
--    backfill yang dibutuhkan.
-- ─────────────────────────────────────────────────────────────────────────────

-- §5.4 — batas persentase diskon order, NULL = tanpa batas. WAJIB dicerminkan di
-- CompaniesEntity (§19.1 langkah 4), atau `tsc` gagal TS2420.
ALTER TABLE "company" ADD COLUMN "maxOrderDiscountPercent" DECIMAL(5, 2);

-- ─────────────────────────────────────────────────────────────────────────────
-- 10. Foreign key
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "orderCustomFieldDefinition"
  ADD CONSTRAINT "orderCustomFieldDefinition_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderCustomFieldDefinition_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "orderType"
  ADD CONSTRAINT "orderType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderType_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "orderReason"
  ADD CONSTRAINT "orderReason_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderReason_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "orderPaymentTerm"
  ADD CONSTRAINT "orderPaymentTerm_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderPaymentTerm_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "orderPaymentMethod"
  ADD CONSTRAINT "orderPaymentMethod_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderPaymentMethod_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "order"
  ADD CONSTRAINT "order_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_orderTypeId_fkey" FOREIGN KEY ("orderTypeId") REFERENCES "orderType"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_customerId_fkey" FOREIGN KEY ("customerId") REFERENCES "customer"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_storeVisitId_fkey" FOREIGN KEY ("storeVisitId") REFERENCES "storeVisit"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_storeGroupId_fkey" FOREIGN KEY ("storeGroupId") REFERENCES "storeGroup"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_priceListId_fkey" FOREIGN KEY ("priceListId") REFERENCES "priceList"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_orderPaymentTermId_fkey" FOREIGN KEY ("orderPaymentTermId") REFERENCES "orderPaymentTerm"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_orderPaymentMethodId_fkey" FOREIGN KEY ("orderPaymentMethodId") REFERENCES "orderPaymentMethod"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_ownerId_fkey" FOREIGN KEY ("ownerId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_approvedById_fkey" FOREIGN KEY ("approvedById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_rejectedById_fkey" FOREIGN KEY ("rejectedById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_cancelledById_fkey" FOREIGN KEY ("cancelledById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "order_cancelReasonId_fkey" FOREIGN KEY ("cancelReasonId") REFERENCES "orderReason"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- `onDelete: Cascade` di sini AMAN dan disengaja, persis pola `productUom`
-- (product_master.md §3.7): baris "orderItem" adalah bagian dari header-nya, bukan
-- bukti terpisah, dan tidak ada hard delete order (§0.6) yang membuat cascade-nya
-- benar-benar berjalan lewat REPLACE item (§9.1) — bukan DELETE header.
ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderItem_orderId_fkey" FOREIGN KEY ("orderId") REFERENCES "order"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "orderItem_productId_fkey" FOREIGN KEY ("productId") REFERENCES "product"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderItem_uomId_fkey" FOREIGN KEY ("uomId") REFERENCES "uom"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderItem_priceListItemId_fkey" FOREIGN KEY ("priceListItemId") REFERENCES "priceListItem"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "orderItem_taxRateId_fkey" FOREIGN KEY ("taxRateId") REFERENCES "taxRate"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "storeVisitNoOrder"
  ADD CONSTRAINT "storeVisitNoOrder_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "storeVisitNoOrder_storeVisitId_fkey" FOREIGN KEY ("storeVisitId") REFERENCES "storeVisit"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "storeVisitNoOrder_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "storeVisitNoOrder_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE,
  ADD CONSTRAINT "storeVisitNoOrder_orderReasonId_fkey" FOREIGN KEY ("orderReasonId") REFERENCES "orderReason"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "orderCounter"
  ADD CONSTRAINT "orderCounter_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 11. Partial unique index + GIN + index partial biasa (§3.10)
--
--     TIDAK bisa ditulis di schema.prisma. Kembarannya yang idempoten ada di
--     prisma/apply-partial-indexes.js — kalau salah satu diubah, ubah keduanya.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "order_number_unique_per_company"
  ON "order" ("companyId", "orderNumber");

CREATE UNIQUE INDEX IF NOT EXISTS "order_client_request_unique"
  ON "order" ("ownerId", "clientRequestId");

CREATE UNIQUE INDEX IF NOT EXISTS "orderType_code_unique_per_company"
  ON "orderType" ("companyId", "code");

CREATE UNIQUE INDEX IF NOT EXISTS "orderType_one_default_per_company"
  ON "orderType" ("companyId")
  WHERE "isDefault";

CREATE UNIQUE INDEX IF NOT EXISTS "orderReason_code_unique_per_company"
  ON "orderReason" ("companyId", "reasonType", "code");

CREATE UNIQUE INDEX IF NOT EXISTS "orderPaymentTerm_code_unique_per_company"
  ON "orderPaymentTerm" ("companyId", "code");

CREATE UNIQUE INDEX IF NOT EXISTS "orderPaymentMethod_code_unique_per_company"
  ON "orderPaymentMethod" ("companyId", "code");

CREATE UNIQUE INDEX IF NOT EXISTS "orderPaymentTerm_one_default_per_company"
  ON "orderPaymentTerm" ("companyId")
  WHERE "isDefault";

CREATE UNIQUE INDEX IF NOT EXISTS "orderPaymentMethod_one_default_per_company"
  ON "orderPaymentMethod" ("companyId")
  WHERE "isDefault";

-- GIN, tidak bisa dinyatakan schema.prisma sama sekali. `jsonb_path_ops` HANYA
-- melayani operator containment `@>` (pola product_customFields_gin, §18.A1
-- product_master.md).
CREATE INDEX IF NOT EXISTS "order_customFields_gin"
  ON "order" USING GIN ("customFields" jsonb_path_ops);

-- Bukan UNIQUE — index partial biasa untuk query "order terbuka milik customer
-- ini" (admin, validasi) tanpa scan seluruh riwayat.
CREATE INDEX IF NOT EXISTS "order_open_by_customer"
  ON "order" ("companyId", "customerId")
  WHERE "status" IN ('draft', 'pending_approval');

-- ─────────────────────────────────────────────────────────────────────────────
-- 12. CHECK constraint (§3.10 nomor 10–14)
-- ─────────────────────────────────────────────────────────────────────────────

-- customFields WAJIB objek, bukan array/skalar (§0.12 flat). Pola
-- product_customFields_object_chk.
ALTER TABLE "order"
  ADD CONSTRAINT "order_customFields_object_chk" CHECK (jsonb_typeof("customFields") = 'object');

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_customFields_object_chk" CHECK (jsonb_typeof("customFields") = 'object');

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_qty_positive_chk" CHECK ("qty" > 0);

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_discount_percent_chk" CHECK ("discountPercent" BETWEEN 0 AND 100);

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_amounts_nonneg_chk" CHECK (
    "grossAmount" >= 0 AND "discountValue" >= 0 AND "netAmount" >= 0
    AND "taxAmount" >= 0 AND "lineTotal" >= 0
  );

ALTER TABLE "order"
  ADD CONSTRAINT "order_totals_nonneg_chk" CHECK (
    "subtotal" >= 0 AND "discountTotal" >= 0 AND "taxTotal" >= 0 AND "grandTotal" >= 0
  );

-- ─────────────────────────────────────────────────────────────────────────────
-- 13. Trigger — §3.11, dua keluarga.
--
--     "touch_updated_at" (fungsi sudah ada) dipasang di enam tabel: order,
--     orderItem, dan keempat master. "orderCustomFieldDefinition" dan
--     "storeVisitNoOrder" SENGAJA TIDAK ikut (§3.11 tidak menyebutnya) — yang
--     pertama identik polanya dengan productCustomField tapi belum masuk stream
--     delta apa pun di F1, yang kedua bukan bukti transaksi dan jarang di-UPDATE
--     setelah dibuat (§0.17).
--
--     "bump_order_updated_at" adalah fungsi BARU, salinan pola
--     "bump_product_updated_at": tiga trigger STATEMENT-level di "orderItem" yang
--     menaikkan "order"."updatedAt" saat baris item berubah, supaya item tidak
--     butuh stream delta/tombstone sendiri (§0.8).
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TRIGGER "order_touch_updated_at"              BEFORE UPDATE ON "order"              FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "orderItem_touch_updated_at"          BEFORE UPDATE ON "orderItem"          FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "orderType_touch_updated_at"          BEFORE UPDATE ON "orderType"          FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "orderReason_touch_updated_at"        BEFORE UPDATE ON "orderReason"        FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "orderPaymentTerm_touch_updated_at"   BEFORE UPDATE ON "orderPaymentTerm"   FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER "orderPaymentMethod_touch_updated_at" BEFORE UPDATE ON "orderPaymentMethod" FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE OR REPLACE FUNCTION bump_order_updated_at() RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    UPDATE "order" SET "updatedAt" = NOW()
     WHERE "id" IN (SELECT DISTINCT "orderId" FROM old_rows);
  ELSE
    UPDATE "order" SET "updatedAt" = NOW()
     WHERE "id" IN (SELECT DISTINCT "orderId" FROM new_rows);
  END IF;
  RETURN NULL;
END; $$ LANGUAGE plpgsql;

CREATE TRIGGER "orderItem_bump_order_ins"
  AFTER INSERT ON "orderItem" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_order_updated_at();
CREATE TRIGGER "orderItem_bump_order_upd"
  AFTER UPDATE ON "orderItem" REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_order_updated_at();
CREATE TRIGGER "orderItem_bump_order_del"
  AFTER DELETE ON "orderItem" REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION bump_order_updated_at();
