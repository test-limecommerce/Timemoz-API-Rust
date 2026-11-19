-- Order Delivery Adjustment (specs/003-order-delivery-adjustment, data-model.md).
--
-- Status baru `delivered`, reason type baru `delivery_adjustment`, enum baru
-- "OrderDeliveryKind", 8 kolom di "order", 9 kolom di "orderItem", dan dua tabel
-- history append-only ("orderDelivery", "orderDeliveryLine").
--
-- Nilai enum yang baru ditambah lewat ADD VALUE TIDAK dipakai di statement mana pun
-- di migration ini (R13) — Postgres menolak memakai nilai baru di transaksi yang
-- sama. "OrderDeliveryKind" dibuat utuh di sini, jadi boleh dipakai partial index.
--
-- Bagian 5–6 (partial unique index + CHECK) tidak bisa dinyatakan di schema.prisma.
-- `prisma db push` tidak membuatnya dan MENGHAPUSNYA kalau ada. Kembarannya yang
-- idempoten ada di `prisma/apply-partial-indexes.js`; kalau salah satu diubah,
-- ubah keduanya.

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. Enum
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TYPE "OrderStatus" ADD VALUE IF NOT EXISTS 'delivered';

ALTER TYPE "OrderReasonType" ADD VALUE IF NOT EXISTS 'delivery_adjustment';

CREATE TYPE "OrderDeliveryKind" AS ENUM ('initial', 'correction');

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. Kolom baru "order" — NULL sampai delivered (FR-013)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "order"
  ADD COLUMN "deliveredAt"        TIMESTAMP(3),
  ADD COLUMN "deliveryRecordedAt" TIMESTAMP(3),
  ADD COLUMN "deliveredById"      INTEGER,
  ADD COLUMN "deliveryRevision"   INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN "finalSubtotal"      DECIMAL(18,2),
  ADD COLUMN "finalDiscountTotal" DECIMAL(18,2),
  ADD COLUMN "finalTaxTotal"      DECIMAL(18,2),
  ADD COLUMN "finalGrandTotal"    DECIMAL(18,2);

ALTER TABLE "order"
  ADD CONSTRAINT "order_deliveredById_fkey" FOREIGN KEY ("deliveredById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. Kolom baru "orderItem"
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "orderItem"
  ADD COLUMN "deliveredQty"          DECIMAL(18,4),
  ADD COLUMN "deliveredQtyInBaseUom" DECIMAL(18,4),
  ADD COLUMN "finalGrossAmount"      DECIMAL(18,2),
  ADD COLUMN "finalDiscountValue"    DECIMAL(18,2),
  ADD COLUMN "finalNetAmount"        DECIMAL(18,2),
  ADD COLUMN "finalTaxAmount"        DECIMAL(18,2),
  ADD COLUMN "finalLineTotal"        DECIMAL(18,2),
  ADD COLUMN "deliveryReasonId"      INTEGER,
  ADD COLUMN "deliveryNote"          VARCHAR(255);

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_deliveryReasonId_fkey" FOREIGN KEY ("deliveryReasonId") REFERENCES "orderReason"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. Tabel history (R4) — append-only, tanpa "updatedAt"
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE "orderDelivery" (
    "id"                SERIAL NOT NULL,
    "uuid"              UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId"         INTEGER NOT NULL,
    "orderId"           INTEGER NOT NULL,
    "kind"              "OrderDeliveryKind" NOT NULL,
    "revision"          INTEGER NOT NULL,
    "source"            "OrderSource" NOT NULL,
    "clientRequestId"   UUID,
    "deliveredAtBefore" TIMESTAMP(3),
    "deliveredAtAfter"  TIMESTAMP(3) NOT NULL,
    "grandTotalBefore"  DECIMAL(18,2) NOT NULL,
    "grandTotalAfter"   DECIMAL(18,2) NOT NULL,
    "note"              VARCHAR(255),
    "recordedById"      INTEGER NOT NULL,
    "recordedAt"        TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "orderDelivery_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderDelivery_uuid_key" ON "orderDelivery"("uuid");
CREATE UNIQUE INDEX "orderDelivery_orderId_revision_key" ON "orderDelivery"("orderId", "revision");
CREATE INDEX "orderDelivery_companyId_recordedAt_idx" ON "orderDelivery"("companyId", "recordedAt");
CREATE INDEX "orderDelivery_orderId_idx" ON "orderDelivery"("orderId");

ALTER TABLE "orderDelivery"
  ADD CONSTRAINT "orderDelivery_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "orderDelivery_orderId_fkey" FOREIGN KEY ("orderId") REFERENCES "order"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "orderDelivery_recordedById_fkey" FOREIGN KEY ("recordedById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

CREATE TABLE "orderDeliveryLine" (
    "id"              SERIAL NOT NULL,
    "orderDeliveryId" INTEGER NOT NULL,
    "orderItemId"     INTEGER NOT NULL,
    "lineNo"          INTEGER NOT NULL,
    "orderedQty"      DECIMAL(18,4) NOT NULL,
    "qtyBefore"       DECIMAL(18,4) NOT NULL,
    "qtyAfter"        DECIMAL(18,4) NOT NULL,
    "reasonId"        INTEGER,
    "reasonCode"      VARCHAR(32),
    "reasonName"      VARCHAR(150),
    "note"            VARCHAR(255),

    CONSTRAINT "orderDeliveryLine_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "orderDeliveryLine_orderDeliveryId_orderItemId_key" ON "orderDeliveryLine"("orderDeliveryId", "orderItemId");
CREATE INDEX "orderDeliveryLine_orderItemId_idx" ON "orderDeliveryLine"("orderItemId");

ALTER TABLE "orderDeliveryLine"
  ADD CONSTRAINT "orderDeliveryLine_orderDeliveryId_fkey" FOREIGN KEY ("orderDeliveryId") REFERENCES "orderDelivery"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "orderDeliveryLine_orderItemId_fkey" FOREIGN KEY ("orderItemId") REFERENCES "orderItem"("id") ON DELETE CASCADE ON UPDATE CASCADE,
  ADD CONSTRAINT "orderDeliveryLine_reasonId_fkey" FOREIGN KEY ("reasonId") REFERENCES "orderReason"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. Partial unique index (FR-018) — benteng kedua di belakang CAS status
-- ─────────────────────────────────────────────────────────────────────────────

CREATE UNIQUE INDEX IF NOT EXISTS "orderDelivery_one_initial_per_order"
  ON "orderDelivery" ("orderId")
  WHERE "kind" = 'initial';

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. CHECK constraint (R13)
-- ─────────────────────────────────────────────────────────────────────────────

ALTER TABLE "order"
  ADD CONSTRAINT "order_final_totals_nonneg_chk" CHECK (
    ("finalSubtotal" IS NULL OR "finalSubtotal" >= 0)
    AND ("finalDiscountTotal" IS NULL OR "finalDiscountTotal" >= 0)
    AND ("finalTaxTotal" IS NULL OR "finalTaxTotal" >= 0)
    AND ("finalGrandTotal" IS NULL OR "finalGrandTotal" >= 0)
  );

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_delivered_qty_chk" CHECK (
    "deliveredQty" IS NULL OR ("deliveredQty" >= 0 AND "deliveredQty" <= "qty")
  );

ALTER TABLE "orderItem"
  ADD CONSTRAINT "orderItem_final_amounts_nonneg_chk" CHECK (
    ("finalGrossAmount" IS NULL OR "finalGrossAmount" >= 0)
    AND ("finalDiscountValue" IS NULL OR "finalDiscountValue" >= 0)
    AND ("finalNetAmount" IS NULL OR "finalNetAmount" >= 0)
    AND ("finalTaxAmount" IS NULL OR "finalTaxAmount" >= 0)
    AND ("finalLineTotal" IS NULL OR "finalLineTotal" >= 0)
  );

ALTER TABLE "orderDeliveryLine"
  ADD CONSTRAINT "orderDeliveryLine_qty_chk" CHECK (
    "qtyAfter" >= 0 AND "qtyAfter" <= "orderedQty" AND "qtyBefore" >= 0
  );
