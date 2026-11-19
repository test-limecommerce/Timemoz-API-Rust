-- false = log app & URL dari desktop tidak disimpan untuk company ini.
ALTER TABLE "company" ADD COLUMN IF NOT EXISTS "useActivityTrack" BOOLEAN NOT NULL DEFAULT false;
