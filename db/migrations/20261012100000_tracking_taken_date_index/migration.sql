-- Cron harian `runSyncForDate` (agregasi summary) memfilter "tracking" hanya berdasarkan
-- "takenDate" (tanpa "userId"). Index yang ada semuanya diawali "userId", jadi filter
-- tanggal saja berujung seq scan seluruh tabel.

-- CreateIndex
CREATE INDEX "tracking_takenDate_idx" ON "tracking"("takenDate");
