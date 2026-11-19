-- Pencabutan JWT. Token user sengaja tidak pernah kedaluwarsa (lihat CLAUDE.md),
-- jadi token yang sudah di-logout harus benar-benar ditolak saat validasi, dan
-- token mentah tidak boleh tetap tersimpan di database.

-- Batas waktu per user: token dengan `iat` lebih lama dari nilai ini ditolak.
ALTER TABLE "users" ADD COLUMN "tokensValidAfter" TIMESTAMP(3);

-- Ganti token mentah dengan SHA-256 hex-nya. Harus sama persis dengan
-- createHash('sha256').update(token).digest('hex') di aplikasi.
ALTER TABLE "jwtBlackList" ADD COLUMN "tokenHash" VARCHAR(64);

UPDATE "jwtBlackList"
SET "tokenHash" = encode(sha256(convert_to("jwt", 'UTF8')), 'hex');

-- Logout berulang dengan token yang sama menghasilkan baris kembar.
DELETE FROM "jwtBlackList" a
USING "jwtBlackList" b
WHERE a."tokenHash" = b."tokenHash"
  AND a."id" > b."id";

ALTER TABLE "jwtBlackList" ALTER COLUMN "tokenHash" SET NOT NULL;

CREATE UNIQUE INDEX "jwtBlackList_tokenHash_key" ON "jwtBlackList"("tokenHash");
CREATE INDEX "jwtBlackList_userId_idx" ON "jwtBlackList"("userId");

ALTER TABLE "jwtBlackList" DROP COLUMN "jwt";
