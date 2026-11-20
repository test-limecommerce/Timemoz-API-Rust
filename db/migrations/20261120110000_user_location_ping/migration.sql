-- Ping lokasi background dari user tanpa geofence (POST /mobile/geofences/event).
-- Sebelumnya ping ini dibuang (NO_APPLICABLE_GEOFENCE). Hanya tabel baru: tidak ada
-- tabel lama yang diubah dan tidak ada data yang hilang.
CREATE TABLE "userLocationPing" (
    "id"        SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "userId"    INTEGER NOT NULL,
    "lat"       DOUBLE PRECISION NOT NULL,
    "lng"       DOUBLE PRECISION NOT NULL,
    "accuracy"  DOUBLE PRECISION NOT NULL,
    "timestamp" TIMESTAMP(3) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),

    CONSTRAINT "userLocationPing_pkey" PRIMARY KEY ("id")
);

CREATE INDEX "userLocationPing_userId_timestamp_idx" ON "userLocationPing"("userId", "timestamp");
CREATE INDEX "userLocationPing_companyId_idx" ON "userLocationPing"("companyId");

ALTER TABLE "userLocationPing"
  ADD CONSTRAINT "userLocationPing_companyId_fkey"
  FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

ALTER TABLE "userLocationPing"
  ADD CONSTRAINT "userLocationPing_userId_fkey"
  FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;
