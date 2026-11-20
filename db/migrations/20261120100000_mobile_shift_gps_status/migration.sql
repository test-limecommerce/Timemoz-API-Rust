-- Status GPS/izin lokasi terakhir per shift (POST /mobile/clock/gps-status).
-- Hanya tambah enum + kolom: semua kolom nullable, kecuali "gpsDisabledCount" yang
-- memakai DEFAULT konstan (Postgres 11+ hanya menulis metadata, tanpa rewrite tabel).
-- Shift lama terbaca gpsStatus NULL dan gpsDisabledCount 0. Tidak ada data yang hilang.
CREATE TYPE "MobileShiftGpsStatus" AS ENUM ('ok', 'gps_disabled', 'permission_denied', 'permission_denied_forever');

ALTER TABLE "mobileShift"
  ADD COLUMN "gpsStatus" "MobileShiftGpsStatus",
  ADD COLUMN "gpsStatusUpdatedAt" TIMESTAMP(3),
  ADD COLUMN "gpsLastDisabledAt" TIMESTAMP(3),
  ADD COLUMN "gpsLastDisabledLat" DOUBLE PRECISION,
  ADD COLUMN "gpsLastDisabledLng" DOUBLE PRECISION,
  ADD COLUMN "gpsLastDisabledLocationAt" TIMESTAMP(3),
  ADD COLUMN "gpsDisabledCount" INTEGER NOT NULL DEFAULT 0;
