-- Index penutup untuk agregasi pemakaian aplikasi per timeline.
--
-- `ReportService.fetchAppUsage` (email scheduled report + export PDF dashboard)
-- menjalankan `SUM("endAt" - "startAt") ... WHERE "trackingTimelineId" = ANY(...)
-- GROUP BY "trackingTimelineId", "name"`. Lewat index lama (hanya
-- "trackingTimelineId") Postgres mengambil setiap baris dari heap: di produksi
-- ~52 ribu baris / ~960 ms / 4.500 blok dibaca dari disk untuk satu report bulanan
-- satu company — dan slot sendTime 21:00 WIB menumpuk report seperti itu.
-- Dengan INCLUDE, kolom yang dibutuhkan terbaca langsung dari index (index-only scan).
--
-- `INCLUDE` TIDAK bisa dinyatakan di `schema.prisma`. Kembarannya yang idempoten ada
-- di `prisma/apply-partial-indexes.js`; kalau salah satu diubah, ubah keduanya.
--
-- CONCURRENTLY supaya insert ke "app" (activity log desktop) tidak terkunci selama
-- index dibangun. CONCURRENTLY tidak boleh berada di dalam transaksi, jadi file ini
-- sengaja HANYA berisi satu statement.
--
-- Index lama tetap dibiarkan: namanya di produksi (`app_trackingtimelineid_idx`)
-- berbeda dengan nama bawaan Prisma di DB dev, jadi menghapusnya dikerjakan terpisah.

CREATE INDEX CONCURRENTLY IF NOT EXISTS "app_timeline_usage_idx"
  ON "app" ("trackingTimelineId") INCLUDE ("name", "startAt", "endAt");
