-- Table Field Visibility — Fase F6 (docs/table_field_visibility.md §7.4).
--
-- Nol ALTER pada tabel existing (§0.3): satu tabel baru. Aman dijalankan pada
-- database yang sedang melayani trafik.
--
-- Tidak ada DDL manual di berkas ini: kunci uniknya tidak memuat kolom nullable,
-- jadi ia bisa dinyatakan penuh di `schema.prisma` dan `prisma db push` akan
-- membuatnya sendiri. `prisma/apply-partial-indexes.js` sengaja TIDAK disentuh —
-- yang ada di sana khusus index yang Prisma tidak bisa nyatakan (§3.4, §3.5).

-- Pencacah kuota reveal harian. Dipakai lewat SATU pernyataan
-- `UPDATE … SET count = count + 1 WHERE … AND count < :limit`, jadi yang
-- memutuskan "kuota masih ada" adalah Postgres, bukan pembacaan yang bisa basi
-- di antara dua request paralel (§7.4 — TOCTOU).
CREATE TABLE "fieldRevealQuota" (
    "id"        SERIAL       NOT NULL,
    "companyId" INTEGER      NOT NULL,
    "actorId"   INTEGER      NOT NULL,
    "tableKey"  VARCHAR(48)  NOT NULL,
    "columnKey" VARCHAR(48)  NOT NULL,
    -- `YYYY-MM-DD` di company.timeZone (§7.4) — bukan DATE, supaya batas harinya
    -- ditentukan kode aplikasi alih-alih zona waktu server database.
    "day"       VARCHAR(10)  NOT NULL,
    "count"     INTEGER      NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),

    CONSTRAINT "fieldRevealQuota_pkey" PRIMARY KEY ("id")
);

-- Kunci pencacah. Inilah yang membuat "buat baris kalau belum ada" tetap aman di
-- bawah paralelisme: yang kalah balapan mendapat pelanggaran unik, bukan baris
-- kedua yang diam-diam menggandakan kuotanya.
CREATE UNIQUE INDEX "fieldRevealQuota_companyId_actorId_tableKey_columnKey_day_key"
  ON "fieldRevealQuota"("companyId", "actorId", "tableKey", "columnKey", "day");

CREATE INDEX "fieldRevealQuota_companyId_day_idx"
  ON "fieldRevealQuota"("companyId", "day");
