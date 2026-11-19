-- docs/api-mobile-store-visit-event.md §2 — riwayat posisi device selama kunjungan
-- toko aktif (enter/exit terhadap radius toko). Tabel baru, bukan perluasan
-- "geofenceAuditLog": kolom "geofenceId" di tabel itu NOT NULL dan sudah dipakai
-- kontrak filter "geofenceUuid" pada GET /v1/admin/clock/attendance yang mengasumsikan
-- setiap baris punya geofence.

-- CreateTable
CREATE TABLE "storeVisitLocationEvent" (
    "id" SERIAL NOT NULL,
    "visitId" INTEGER NOT NULL,
    "storeId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "event" VARCHAR(10) NOT NULL,
    "lat" DOUBLE PRECISION NOT NULL,
    "lng" DOUBLE PRECISION NOT NULL,
    "accuracy" DOUBLE PRECISION NOT NULL,
    "timestamp" TIMESTAMP(3) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "storeVisitLocationEvent_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE INDEX "storeVisitLocationEvent_visitId_idx" ON "storeVisitLocationEvent"("visitId");

-- CreateIndex
CREATE INDEX "storeVisitLocationEvent_userId_timestamp_idx" ON "storeVisitLocationEvent"("userId", "timestamp");

-- AddForeignKey
-- Cascade, BUKAN Restrict seperti "storeVisit"."user": baris ini turunan kunjungan,
-- bukan bukti kerja independen (§2 dokumen).
ALTER TABLE "storeVisitLocationEvent" ADD CONSTRAINT "storeVisitLocationEvent_visitId_fkey" FOREIGN KEY ("visitId") REFERENCES "storeVisit"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitLocationEvent" ADD CONSTRAINT "storeVisitLocationEvent_storeId_fkey" FOREIGN KEY ("storeId") REFERENCES "store"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "storeVisitLocationEvent" ADD CONSTRAINT "storeVisitLocationEvent_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
