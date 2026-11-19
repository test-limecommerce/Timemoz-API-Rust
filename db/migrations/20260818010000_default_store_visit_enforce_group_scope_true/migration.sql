-- Company baru dibuat strict: sales hanya melihat toko di region yang dipegangnya, dan
-- toko tanpa region tidak terlihat oleh siapa pun sampai region-nya diisi.
--
-- ALTER COLUMN ... SET DEFAULT hanya berlaku untuk baris yang lahir SETELAH ini. Company
-- yang sudah ada menyimpan nilainya sendiri di kolom, jadi mereka tetap `false` dan tidak
-- berubah perilaku sedikit pun. Itu memang yang diinginkan: mereka sudah berjalan dengan
-- toko tanpa region yang terlihat semua orang, dan mencabutnya serentak lewat migration
-- akan menghilangkan toko dari sales tanpa ada yang menekan tombol apa pun.
--
-- Untuk menyalakannya pada company yang sudah ada, pakai PATCH /companies/{uuid} satu per
-- satu setelah seluruh tokonya diberi region — JANGAN ditambahkan sebagai UPDATE massal di
-- sini.
ALTER TABLE "company"
  ALTER COLUMN "storeVisitEnforceGroupScope" SET DEFAULT true;
