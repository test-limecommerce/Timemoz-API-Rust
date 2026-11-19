-- Latest Version Mobile: satu baris per platform (Android/iOS) yang dibaca
-- endpoint publik GET /v1/settings/latest-version-mobile dan ditulis lewat
-- scripts/upsert-latest-version-mobile.js (bukan lewat API admin).

CREATE TYPE "LatestVersionMobileType" AS ENUM ('ANDROID', 'IOS');

CREATE TABLE "latestVersionMobile" (
    "id"      SERIAL                    NOT NULL,
    "version" VARCHAR(255)              NOT NULL,
    "type"    "LatestVersionMobileType" NOT NULL,
    "force"   BOOLEAN                   NOT NULL DEFAULT false,

    CONSTRAINT "latestVersionMobile_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "latestVersionMobile_type_key" ON "latestVersionMobile"("type");
