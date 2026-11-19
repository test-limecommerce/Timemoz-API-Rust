-- AlterEnum
-- Postgres tidak mengizinkan ALTER TYPE ... ADD VALUE dan pernyataan yang memakai nilai
-- barunya berada dalam transaksi/migration yang sama (§2.5 docs/store_visit_grouping.md).
-- Karena itu migration ini SENGAJA hanya berisi baris ini — jangan tambahkan pernyataan
-- lain di sini, dan jangan gabungkan ke migration sebelumnya.
ALTER TYPE "StoreVisitAuditEvent" ADD VALUE 'outOfScope';
