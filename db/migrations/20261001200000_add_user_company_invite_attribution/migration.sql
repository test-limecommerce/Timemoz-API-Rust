-- Atribusi bergabung per keanggotaan company: siapa yang mengundang dan lewat
-- jalur apa. Dibaca GET /auth/me untuk properti profil Mixpanel.
--
-- inviteMethod: 'EMAIL' = diundang admin (POST /users/invite & /invite/batch,
-- link aktivasi maupun password otomatis); 'LINK' = join mandiri lewat invite
-- code (POST /auth/register-invite). Tidak ada backfill — baris lama dan
-- pembuat company tetap NULL.
ALTER TABLE "userCompany" ADD COLUMN IF NOT EXISTS "invitedById" INTEGER;
ALTER TABLE "userCompany" ADD COLUMN IF NOT EXISTS "inviteMethod" VARCHAR(16);

CREATE INDEX IF NOT EXISTS "userCompany_invitedById_idx" ON "userCompany"("invitedById");

-- Pengundang yang dihapus tidak ikut menghapus keanggotaan yang ia undang.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'userCompany_invitedById_fkey'
  ) THEN
    ALTER TABLE "userCompany"
      ADD CONSTRAINT "userCompany_invitedById_fkey"
      FOREIGN KEY ("invitedById") REFERENCES "users"("id")
      ON DELETE SET NULL ON UPDATE CASCADE;
  END IF;
END $$;
