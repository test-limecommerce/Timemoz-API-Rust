-- Boost batas paket per company (SALES / TIME). Lihat src/plan-limits/plan-boost.ts.
--
-- Tabel baru tanpa baris awal: tidak ada company yang berubah batasnya sampai
-- admin internal menyalakan boost lewat PATCH /admin-company/set/plan-boost/:uuid.
CREATE TABLE IF NOT EXISTS "companyPlanBoost" (
  "id" SERIAL NOT NULL,
  "companyId" INTEGER NOT NULL,
  "product" "PricingProductType" NOT NULL,
  "enabled" BOOLEAN NOT NULL DEFAULT false,
  "previousValues" JSONB,
  "boostedValues" JSONB,
  "enabledAt" TIMESTAMP(3),
  "disabledAt" TIMESTAMP(3),
  "updatedByOperatorId" INTEGER,
  "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "updatedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

  CONSTRAINT "companyPlanBoost_pkey" PRIMARY KEY ("id")
);

-- Satu baris per (company, produk): toggle berikutnya meng-update baris yang sama.
CREATE UNIQUE INDEX IF NOT EXISTS "companyPlanBoost_companyId_product_key"
  ON "companyPlanBoost"("companyId", "product");

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'companyPlanBoost_companyId_fkey'
  ) THEN
    ALTER TABLE "companyPlanBoost"
      ADD CONSTRAINT "companyPlanBoost_companyId_fkey"
      FOREIGN KEY ("companyId") REFERENCES "company"("id")
      ON DELETE CASCADE ON UPDATE CASCADE;
  END IF;
END $$;
