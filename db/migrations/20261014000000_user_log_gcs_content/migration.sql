-- Isi userLog pindah ke GCS; database cukup menyimpan path + ukuran asli.
-- Tabel ini tidak pernah dibuat lewat migration (asalnya `db push`), jadi
-- pakai IF NOT EXISTS supaya aman di database yang sudah sinkron.
ALTER TABLE "userLog" ALTER COLUMN "content" DROP NOT NULL;
ALTER TABLE "userLog" ADD COLUMN IF NOT EXISTS "filePath" VARCHAR(500);
ALTER TABLE "userLog" ADD COLUMN IF NOT EXISTS "sizeBytes" INTEGER;
