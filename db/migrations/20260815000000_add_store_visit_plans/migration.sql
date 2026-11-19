-- AlterTable
ALTER TABLE "storeVisit" ADD COLUMN "planId" INTEGER;

-- CreateTable
CREATE TABLE "storeVisitPlan" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "companyId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "storeId" INTEGER NOT NULL,
    "visitDate" DATE NOT NULL,
    "sortOrder" INTEGER,
    "note" VARCHAR(255),
    "createdById" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "storeVisitPlan_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "storeVisitPlan_uuid_key" ON "storeVisitPlan"("uuid");

-- CreateIndex
-- Satu kewajiban per (sales, tanggal, toko). Index biasa, bukan partial — tidak ada
-- soft delete di tabel ini (§0.8 docs/store_visit_schedule.md).
CREATE UNIQUE INDEX "storeVisitPlan_userId_visitDate_storeId_key" ON "storeVisitPlan"("userId", "visitDate", "storeId");

-- CreateIndex
-- Daftar & kalender admin: rentang tanggal per company (§2.1 docs/store_visit_schedule.md).
CREATE INDEX "storeVisitPlan_companyId_visitDate_idx" ON "storeVisitPlan"("companyId", "visitDate");

-- CreateIndex
-- Laporan kepatuhan per toko, dan pemeriksaan "toko ini punya jadwal hari ini".
CREATE INDEX "storeVisitPlan_storeId_visitDate_idx" ON "storeVisitPlan"("storeId", "visitDate");

-- CreateIndex
-- §0.7 docs/store_visit_schedule.md: kunjungan kedua ke toko yang sama di hari yang sama
-- tidak bisa mengklaim jadwal yang sudah dipegang kunjungan pertama. NULL berulang
-- diperbolehkan Postgres, jadi kunjungan `unplanned` tidak saling bertabrakan (§1.5).
CREATE UNIQUE INDEX "storeVisit_planId_key" ON "storeVisit"("planId");

-- CreateIndex
-- Menopang filter `planned=false` pada daftar kunjungan admin (§4.2 docs/store_visit_schedule.md).
CREATE INDEX "storeVisit_companyId_planId_idx" ON "storeVisit"("companyId", "planId");

-- AddForeignKey
ALTER TABLE "storeVisitPlan" ADD CONSTRAINT "storeVisitPlan_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitPlan" ADD CONSTRAINT "storeVisitPlan_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitPlan" ADD CONSTRAINT "storeVisitPlan_createdById_fkey" FOREIGN KEY ("createdById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
-- Restrict, BUKAN Cascade atau SetNull. Menghapus jadwal yang sudah tercapai berarti
-- mengubah kunjungan yang sah jadi `unplanned` secara retroaktif (§0.8); database yang
-- menolaknya, supaya balapan TOCTOU §5.2/§5.4 docs/store_visit_schedule.md tidak punya
-- jendela sama sekali.
ALTER TABLE "storeVisit" ADD CONSTRAINT "storeVisit_planId_fkey" FOREIGN KEY ("planId") REFERENCES "storeVisitPlan"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
