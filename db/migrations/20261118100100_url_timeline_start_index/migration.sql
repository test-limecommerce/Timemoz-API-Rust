-- Index komposit ("trackingTimelineId", "startAt") di "url" untuk POST /activity-logs.
--
-- `ActivityLogsService.upsertUrl` mencari baris terakhir timeline
-- (`WHERE "trackingTimelineId" = $1 AND "startAt" <= $2 ORDER BY "startAt" DESC, id DESC
-- LIMIT 1`) SAMBIL memegang advisory lock per timeline. Lewat index lama yang hanya
-- ("trackingTimelineId"), Postgres membaca semua baris timeline dari heap lalu
-- mengurutkannya, dan selama itu kiriman lain untuk timeline yang sama menunggu lock.
-- Dengan index ini query tersebut cukup satu probe index (scan mundur).
--
-- Index ("trackingTimelineId") yang lama SENGAJA dibiarkan (pola sama dengan
-- 20261028120000_tracking_timeline_tracking_start_index); menghapusnya keputusan terpisah.
--
-- CONCURRENTLY supaya insert/update activity log desktop tidak terkunci selama index
-- dibangun. CONCURRENTLY TIDAK boleh berada di dalam transaksi, jadi file ini sengaja
-- hanya berisi satu statement, dan TIDAK bisa diterapkan lewat `npm run db:migrate`
-- (psql --single-transaction akan gagal tanpa perubahan apa pun). Terapkan dengan:
--   psql "$DB" -v ON_ERROR_STOP=1 -f prisma/migrations/<folder ini>/migration.sql
-- Bila build CONCURRENTLY gagal di tengah jalan, index tertinggal INVALID dan
-- IF NOT EXISTS akan melewatinya: cek `pg_index.indisvalid`, DROP lalu ulangi.
--
-- Tidak ada data yang dihapus atau diubah.

CREATE INDEX CONCURRENTLY IF NOT EXISTS "url_trackingTimelineId_startAt_idx"
  ON "url" ("trackingTimelineId", "startAt");
