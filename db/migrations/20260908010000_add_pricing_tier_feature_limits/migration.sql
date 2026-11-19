-- Batas & gerbang fitur Sales Tracker pada paket harga.
--
-- Semua kolom `NOT NULL DEFAULT <konstanta>`, dan default itulah seluruh isi
-- migration ini: paket yang sudah ada mendarat dengan `maxStores = 10` dan
-- kelima gerbang fitur dalam keadaan mati. Belum ada kode yang membaca kolom
-- ini, jadi migration ini tidak mengubah perilaku paket mana pun; penegakannya
-- menyusul terpisah.
--
-- `order` adalah kata kunci SQL, jadi wajib dikutip ganda seperti di bawah.
--
-- `IF NOT EXISTS` supaya migration ini aman dijalankan ulang pada database dev
-- yang kolomnya sudah lebih dulu masuk lewat `prisma db push`.
--
-- Tidak ada rewrite tabel: sejak PostgreSQL 11, `ADD COLUMN ... NOT NULL
-- DEFAULT <konstanta>` ditulis ke katalog, bukan ke setiap baris.
ALTER TABLE "pricingTiers"
  ADD COLUMN IF NOT EXISTS "maxStores"     INTEGER NOT NULL DEFAULT 10,
  ADD COLUMN IF NOT EXISTS "auditLog"      BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS "form"          BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS "timelineRoute" BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS "product"       BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS "order"         BOOLEAN NOT NULL DEFAULT false;
