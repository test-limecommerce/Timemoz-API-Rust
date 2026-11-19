-- Index untuk RolesGuard: query `userAccessLevel WHERE userId = ? AND status = ?`
-- dijalankan di setiap request ber-guard. Sebelumnya tidak ada index sama sekali
-- (Postgres tidak auto-index foreign key), sehingga terjadi sequential scan.

CREATE INDEX IF NOT EXISTS "userAccessLevel_userId_status_idx"
  ON "userAccessLevel" ("userId", "status");

CREATE INDEX IF NOT EXISTS "userAccessLevel_accessLevelId_idx"
  ON "userAccessLevel" ("accessLevelId");

CREATE INDEX IF NOT EXISTS "userAccessLevel_companyId_idx"
  ON "userAccessLevel" ("companyId");
