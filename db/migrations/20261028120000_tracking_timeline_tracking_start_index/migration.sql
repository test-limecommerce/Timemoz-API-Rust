-- Index komposit (trackingId, startAt) untuk pencarian timeline per pemilik.
--
-- Query berikut memfilter lewat relasi `tracking: { userId }` plus rentang `startAt`:
--   - `usedApprovedManualSeconds` (src/manual-time-settings/manual-time-quota.ts),
--     jalan di dalam transaksi yang memegang kunci kuota,
--   - overlap query di `approveManualTimeInTx` (src/manual-time-approvals/approve-manual-time.ts),
--   - `trimOwnerApprovedManualInOtherTrackings` dan `remainingWorkTimeSeconds`
--     (src/tracking-timelines/tracking-timelines.service.ts).
-- Index lama hanya `("trackingId")`, sehingga Postgres membaca SEMUA timeline milik
-- user dari heap sebelum filter `startAt` diterapkan. Dengan `("trackingId", "startAt")`,
-- tiap probe tracking milik user dibatasi rentang `startAt` langsung di index.
--
-- Index `("trackingId")` yang lama SENGAJA dibiarkan di sini. Index komposit ini
-- membuatnya berlebihan (prefix kiri yang sama), tetapi menghapusnya adalah keputusan
-- terpisah.
--
-- CONCURRENTLY supaya penulisan ke "trackingTimeline" (sinkronisasi desktop saat jam
-- puncak) tidak terkunci selama index dibangun. CONCURRENTLY tidak boleh berada di
-- dalam transaksi, jadi file ini sengaja HANYA berisi satu statement.
--
-- Index ini bisa dinyatakan di `schema.prisma` (`@@index([trackingId, startAt])`),
-- jadi TIDAK perlu ditambahkan ke `prisma/apply-partial-indexes.js`.

CREATE INDEX CONCURRENTLY IF NOT EXISTS "trackingTimeline_trackingId_startAt_idx"
  ON "trackingTimeline" ("trackingId", "startAt");
